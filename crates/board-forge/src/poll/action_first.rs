//! Cheap discovery is ordering evidence only; `pull` still authorizes every action.
use super::*;

const UPSTREAM_REPOSITORIES: usize = 2;

/// One search page and fresh decisions, never an unbounded org sweep.
pub(crate) const APPROVED_REQUESTS: usize = 256;

pub(crate) fn approved_path(config: &Config) -> String {
    let owner = config.dispatch_repo.split('/').next().unwrap_or_default();
    let query = format!(
        "user:{owner} user:{} is:pr is:open review:approved draft:false -label:needs-human archived:false",
        config.bot
    );
    let query: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    format!("search/issues?q={query}&sort=updated&order=asc&per_page=100")
}

impl<R: Rest, G: Transport> Pass<'_, R, G> {
    pub(super) fn approved(
        &mut self,
        follow: &BTreeSet<&String>,
        cursor: Option<&str>,
        outcome: &mut Outcome,
    ) -> Result<()> {
        outcome.approved_read = true;
        let response = self.get(&approved_path(self.config))?;
        ensure!(
            response["incomplete_results"] == false
                && response["total_count"].as_u64().is_some_and(|n| n <= 100),
            "approved PR search incomplete or exceeds its 100-result bound"
        );
        let items = response["items"]
            .as_array()
            .context("approved PR search has no items")?;
        ensure!(
            response["total_count"].as_u64() == Some(items.len() as u64),
            "approved PR search is truncated"
        );
        let mut listed: BTreeMap<&String, Vec<Value>> = BTreeMap::new();
        // Validate the whole page before taking any decision. Search is only
        // discovery: the trusted follow set and fresh pull read authorize work.
        for item in items {
            let repo = item["repository_url"]
                .as_str()
                .and_then(|s| s.strip_prefix(&format!("{API_ROOT}repos/")))
                .context("approved PR search has an invalid repository URL")?;
            validate_repository(repo)?;
            ensure!(
                item["pull_request"].is_object() && item["number"].as_u64().is_some_and(|n| n > 0),
                "approved PR search has an invalid PR identity"
            );
            if let Some(&repo) = follow.iter().find(|r| r.eq_ignore_ascii_case(repo))
                && self.config.ours(repo)
                && self.envelope.as_ref().is_none_or(|e| e.admits(repo))
            {
                listed.entry(repo).or_default().push(item.clone());
            }
        }
        outcome.lines.push(format!(
            "pull requests: approved search found {}, admitted {}",
            items.len(),
            listed.values().map(Vec::len).sum::<usize>()
        ));
        for (repo, turn) in walk(follow, cursor) {
            if let Some(open) = listed.get(repo) {
                self.follow_open(
                    repo,
                    open.iter()
                        .filter(|p| p["number"].as_u64().is_some_and(|n| turn.takes(n)))
                        .cloned()
                        .collect(),
                    outcome,
                )?;
            }
        }
        if let Some(at) = &self.cut {
            outcome.approved_cut = Some(at.clone());
            outcome.lines.push(format!(
                "pull requests: approved decisions stopped at {at}; request share spent"
            ));
        }
        Ok(())
    }
}

/// A caller gives approved own-space work a bounded turn before intake,
/// authored discovery and activity can consume the ordinary stage share.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_approved_capped<R: Rest, G: Transport>(
    rest: &R,
    graphql: &G,
    config: &Config,
    follow: &[String],
    apply: Option<Apply<'_, R>>,
    caps: &caps::Caps,
    now: OffsetDateTime,
    cursor: Option<&str>,
) -> Result<(Outcome, usize)> {
    let mut pass = Pass {
        apply,
        caps: Some(caps),
        followed: follow.iter().map(|r| r.to_ascii_lowercase()).collect(),
        remaining: APPROVED_REQUESTS,
        ..Pass::new(rest, graphql, config, now)
    };
    if pass.apply.is_some() {
        let user = pass.get("user")?;
        ensure!(
            text(&user, "/login") == config.bot,
            "--apply token is not the configured bot"
        );
    }
    let mut outcome = Outcome::default();
    if let Err(error) = pass.approved(&follow.iter().collect(), cursor, &mut outcome) {
        outcome.error("pull requests: approved search", &error);
    }
    rest.under_way(false);
    outcome.dispatched = pass.dispatched;
    outcome.merged = pass.merged;
    outcome.preview_merges = pass.preview_merges;
    outcome.updated = pass.updated;
    outcome.deferred = pass.deferred;
    outcome.review_intake = pass.review_intake;
    outcome.reviewing = pass.reviewing;
    outcome.review_runs = pass.review_runs;
    outcome.review_unread = pass.review_unread;
    Ok((outcome, APPROVED_REQUESTS - pass.remaining))
}

pub(crate) const SCREEN: &str = r#"query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    pullRequests(first: 100, states: OPEN, orderBy: {field: CREATED_AT, direction: ASC}) {
      nodes { number isDraft reviewDecision mergeable author { login }
        autoMergeRequest { enabledAt } mergeQueueEntry { id }
        reviews(last: 10) { nodes { state commit { oid } } }
        headRefOid labels(first: 100) { nodes { name } } }
      pageInfo { hasNextPage }
    }
  }
}"#;

fn ready(p: &Value) -> bool {
    let approved = p["reviewDecision"] == "APPROVED"
        || p["reviews"]["nodes"].as_array().is_some_and(|reviews| {
            reviews
                .iter()
                .any(|r| r["state"] == "APPROVED" && r["commit"]["oid"] == p["headRefOid"])
        });
    p["isDraft"] == false
        && approved
        && p["mergeable"] == "MERGEABLE"
        && p["autoMergeRequest"].is_null()
        && p["mergeQueueEntry"].is_null()
        && !p["labels"]["nodes"]
            .as_array()
            .is_some_and(|labels| holds(labels))
}

impl<R: Rest, G: Transport> Pass<'_, R, G> {
    fn discovery_room(&self, extra: usize) -> bool {
        let margin = PULL_SHARE_MARGIN + extra;
        self.remaining > margin && self.rest.share_left().is_none_or(|left| left > margin)
    }

    fn screen(&mut self, repo: &str) -> Result<Vec<Value>> {
        let (owner, name) = repo.split_once('/').context("repository")?;
        self.spend()?;
        let data = self
            .graphql
            .graphql(SCREEN, json!({"owner": owner, "name": name}))?;
        let pulls = &data["repository"]["pullRequests"];
        ensure!(
            pulls["pageInfo"]["hasNextPage"] == false,
            "actionFirst listing incomplete; repository deferred (at most 100 open PRs)"
        );
        let nodes = pulls["nodes"]
            .as_array()
            .context("actionFirst listing has no nodes")?;
        nodes
            .iter()
            .map(|p| {
                ensure!(
                    p["number"].as_u64().is_some(),
                    "actionFirst listing has no PR number"
                );
                Ok(json!({"number": p["number"], "user": p["author"],
                "head": {"sha": p["headRefOid"]}, "labels": p["labels"]["nodes"],
                "ready": ready(p) && !self.config.policy.no_merge.iter().any(|r| r.eq_ignore_ascii_case(repo))}))
            })
            .collect()
    }

    pub(super) fn action_first(
        &mut self,
        follow: &BTreeSet<&String>,
        state: &State,
        missing: &BTreeSet<String>,
        outcome: &mut Outcome,
    ) -> (Option<String>, Option<String>, BTreeMap<String, u64>) {
        let cursor = state.share_cursor.as_deref();
        let discovery_cursor = state.action_first_cursor.as_deref();
        let mut positions = state.action_first_positions.clone();
        positions.retain(|repo, _| follow.iter().any(|r| r.eq_ignore_ascii_case(repo)));
        let mut listed = BTreeMap::new();
        let mut next = None;
        // Discovery must leave a full decision's margin. Resume unlisted
        // repositories next pass without cutting off already discovered work.
        for (repo, turn) in walk(follow, discovery_cursor.or(cursor)) {
            if !self.config.ours(repo) || matches!(turn, Turn::Before(_)) {
                continue;
            }
            if missing.contains(&repo.to_ascii_lowercase())
                || self.envelope.as_ref().is_some_and(|e| !e.admits(repo))
            {
                outcome.undecided.insert(repo.to_ascii_lowercase());
                continue;
            }
            // Census reads are cached before listing, rather than spending
            // the decision margin after discovery has reserved it. Unlisted
            // repositories still get their census after decisions below.
            let census = !self.config.policy.review_pools.is_empty()
                && review_workflow(self.config, repo).is_some()
                && !self.review_runs.contains_key(repo.as_str());
            let room = self.discovery_room(usize::from(census));
            if room {
                self.count_reviews(repo, outcome);
            }
            if !room || !self.discovery_room(0) {
                next.get_or_insert_with(|| repo.to_ascii_lowercase());
                outcome.undecided.insert(repo.to_ascii_lowercase());
                continue;
            }
            match self.screen(repo) {
                Ok(pulls) => {
                    listed.insert(repo.as_str(), pulls);
                }
                Err(error) => {
                    outcome.undecided.insert(repo.to_ascii_lowercase());
                    if crate::pass::budget_spent(&error) {
                        next.get_or_insert_with(|| repo.to_ascii_lowercase());
                    } else {
                        outcome.error(repo, &error);
                    }
                }
            }
        }
        // Both classes rotate from the cut so persistent false positives
        // cannot monopolize the priority share. Neither phase lists again.
        let mut turns = Vec::new();
        let mut before = Vec::new();
        for (repo, turn) in walk(follow, cursor) {
            if matches!(turn, Turn::Before(_)) {
                continue;
            }
            let key = repo.to_ascii_lowercase();
            if let Turn::From(n) = turn {
                positions.entry(key.clone()).or_insert(n);
            }
            if let Some(&n) = positions.get(&key) {
                turns.push((repo, Turn::From(n)));
                before.push((repo, Turn::Before(n)));
            } else {
                turns.push((repo, Turn::All));
            }
        }
        // A repository's earlier PRs come after every other repository,
        // not immediately after its suffix: otherwise it can monopolize
        // every decision share even while discovery finds other ready PRs.
        turns.extend(before);
        for priority in [true, false] {
            for &(repo, turn) in &turns {
                let Some(open) = listed.get(repo.as_str()) else {
                    continue;
                };
                self.count_reviews(repo, outcome);
                if self.cut.is_some() {
                    outcome.undecided.insert(repo.to_ascii_lowercase());
                    continue;
                }
                let key = repo.to_ascii_lowercase();
                let open = open
                    .iter()
                    .filter(|p| {
                        p["ready"] == priority
                            && p["number"].as_u64().is_some_and(|n| turn.takes(n))
                    })
                    .cloned()
                    .collect();
                if let Err(error) = self.follow_open(repo, open, outcome) {
                    outcome.undecided.insert(repo.to_ascii_lowercase());
                    if crate::pass::budget_spent(&error) {
                        self.stop_at(repo, None);
                    } else {
                        outcome.error(repo, &error);
                    }
                }
                // A cut between PRs is a successful, partial walk. Keep
                // outstanding asks for its unread PRs, just as on an error.
                if self.cut.is_some() {
                    outcome.undecided.insert(repo.to_ascii_lowercase());
                    if let Some((cut_repo, number)) =
                        self.cut.as_deref().and_then(|c| c.split_once('#'))
                        && cut_repo == key
                        && let Ok(number) = number.parse()
                    {
                        // Discovery may skip this repository next pass. A cut
                        // elsewhere must not erase its unfinished decision.
                        positions.insert(key, number);
                    }
                } else if !priority && !matches!(turn, Turn::From(_)) {
                    positions.remove(&key);
                }
            }
        }
        // Upstream follow-up is deliberately read-only: two bounded listing
        // calls, no round, review start, marker or merge outside our space.
        let mut upstream = 0;
        let mut read = 0;
        let mut upstream_next = None;
        for (repo, turn) in walk(follow, cursor) {
            if self.config.ours(repo) || matches!(turn, Turn::Before(_)) {
                continue;
            }
            self.count_reviews(repo, outcome);
            outcome.undecided.insert(repo.to_ascii_lowercase());
            if self.cut.is_some()
                || missing.contains(&repo.to_ascii_lowercase())
                || self.envelope.as_ref().is_some_and(|e| !e.admits(repo))
            {
                continue;
            }
            if read == UPSTREAM_REPOSITORIES {
                upstream_next.get_or_insert_with(|| repo.to_ascii_lowercase());
                continue;
            }
            read += 1;
            match self.screen(repo) {
                Ok(pulls) => {
                    for p in pulls {
                        if text(&p, "/user/login") == self.config.bot {
                            upstream += 1;
                            outcome.lines.push(format!(
                                "{repo}#{} read: upstream follow-up only; ready candidate={}",
                                p["number"], p["ready"]
                            ));
                        }
                    }
                }
                Err(error) => outcome
                    .lines
                    .push(format!("{repo} upstream read deferred: {error:#}")),
            }
        }
        let count = |actions: &[&str]| {
            outcome
                .steps
                .iter()
                .filter(|s| s.ran && actions.contains(&s.action))
                .count()
        };
        outcome.lines.push(format!(
            "pull requests: merged/enqueued {}, fix rounds {}, continuations {}, upstream reads {upstream}",
            self.merged + self.preview_merges, count(&["fix", "rebase"]), count(&["continue"])
        ));
        outcome.lines.push(format!(
            "updated {} behind auto-merge PR(s)",
            outcome
                .steps
                .iter()
                .filter(|s| s.ran
                    && s.action == "update"
                    && s.reason.starts_with("behind auto-merge PR;"))
                .count()
        ));
        for repo in follow {
            self.count_reviews(repo, outcome);
        }
        if let Some(repo) = &upstream_next {
            outcome.lines.push(format!("pull requests: upstream listing cap {UPSTREAM_REPOSITORIES}; next pass starts upstream reads at {repo}"));
        }
        if let Some(repo) = &next {
            outcome.activity += 1;
            outcome.lines.push(format!(
                "pull requests: discovery deferred; next pass starts at {repo}"
            ));
        }
        (next, upstream_next, positions)
    }
}
