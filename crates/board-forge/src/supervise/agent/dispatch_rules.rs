//! The executor must not let an agent select a scarce variant or start work
//! that the pass is holding. Check dispatches before reserving or writing;
//! each then takes a lease of the pass's slots before its writes, as any start
//! outside the pass does ([`lease::check_outside`]), so that the pass's
//! `wip` bounds them.
use super::*;
use crate::pass::{self, holds, lease, mix, pressure};
use crate::supervise::health::{
    USAGE_MARKER, issue_comments, issue_comments_until, own_comments, usage_snapshot,
};
use time::OffsetDateTime;

const BOARD_QUERY: &str = r#"query($owner:String!,$name:String!,$number:Int!,$field:String!){
  repository(owner:$owner,name:$name){issue(number:$number){number repository{nameWithOwner}
    projectItems(first:20){pageInfo{hasNextPage} nodes{
      project{number owner{... on Organization{login} ... on User{login}}}
      priority:fieldValueByName(name:$field){... on ProjectV2ItemFieldSingleSelectValue{name}}
    }}
  }}
}"#;

/// What a failed read of the pass's private snapshots says: the executor's
/// log may be public, and a GraphQL error may name the issue it read.
const SNAPSHOT_UNREADABLE: &str = "Pass snapshots could not be read";

/// The oldest holds snapshot used: two of the pass's renewals missed.
const HOLDS_MAX_AGE_MS: i64 = 2 * holds::HOLDS_REFRESH_HOURS as i64 * 60 * 60 * 1000;

/// The most lease refs one listing page proves complete: a full page may
/// have more.
const LEASE_PAGE: usize = 100;

pub(super) struct Check {
    pass: pass::Config,
    holds: Vec<holds::Published>,
    usage: Value,
    now: OffsetDateTime,
    open_prs: BTreeMap<String, usize>,
    /// The issues a run in flight or a lease holds, by [`pass::key`].
    busy: BTreeMap<String, String>,
}

impl Check {
    pub(super) fn load(api: &dyn Api, policy: &Policy, now: i64) -> Result<Self> {
        let path = policy
            .agent
            .pass_config
            .as_deref()
            .context("dispatchPassRules needs agent.passConfig")?;
        let source = std::fs::read_to_string(path).context("reading agent.passConfig")?;
        let pass = pass_config(&source).context("checking agent.passConfig")?;
        Self::read(api, policy, pass, now)
    }

    fn read(api: &dyn Api, policy: &Policy, pass: pass::Config, now: i64) -> Result<Self> {
        ensure!(
            pass.bot.eq_ignore_ascii_case(&policy.settings.bot)
                && pass
                    .dispatch_repo
                    .eq_ignore_ascii_case(&policy.settings.dispatch_repo)
                && pass.dispatch_workflow == policy.settings.dispatch_workflow
                && pass.forgejo_board().is_none(),
            "Pass dispatch policy does not match the supervisor"
        );
        ensure!(
            pass.pool(&policy.agent.pool).is_some(),
            "Supervisor agent.pool must be a base pool in agent.passConfig"
        );
        let s = policy.settings;
        let target = Target {
            caller: s.dispatch_repo.clone(),
            workflow: s.dispatch_workflow.clone(),
            git_ref: String::new(),
        };
        lease::check_outside(&pass, &target, &policy.agent.pool)?;
        let at = pass
            .holds
            .as_ref()
            .context("dispatchPassRules needs pass holds")?;
        let usage = pass
            .usage
            .as_ref()
            .filter(|u| u.issue > 0)
            .context("dispatchPassRules needs published pass usage")?;
        let comments = issue_comments(api, &at.repository, at.issue, COMMENT_PAGES)
            .map_err(|_| anyhow::anyhow!(SNAPSHOT_UNREADABLE))?;
        let first = snapshot_comment(&comments, &pass.bot, holds::HOLDS_MARKER)
            .context("reading pass holds")?;
        let parts: Vec<_> = comments
            .iter()
            .filter(|c| authored(c, &pass.bot))
            .filter_map(|c| {
                c["body"]
                    .as_str()
                    .filter(|b| b.starts_with(holds::PART_MARKER))
                    .map(|b| (c, b))
            })
            .collect();
        let bodies: Vec<_> = parts.iter().map(|(_, b)| *b).collect();
        let (body, used) =
            holds::assemble_from(first, &bodies).context("Incomplete pass holds snapshot")?;
        for n in used {
            authenticated(parts[n].0, &pass.bot)?;
        }
        let snapshot = holds::published(&body).context("Unusable pass holds snapshot")?;
        ensure!(
            snapshot["truncated"] == 0
                && millis(&snapshot["at"])
                    .is_some_and(|at| at <= now && at >= now - HOLDS_MAX_AGE_MS),
            "Pass holds are stale or incomplete"
        );
        let entries = snapshot["holds"].as_array().context("Missing pass holds")?;
        ensure!(
            entries.iter().all(|h| h.get("item").is_some()),
            "Pass hold is missing its scope"
        );
        let holds =
            serde_json::from_value(snapshot["holds"].clone()).context("Malformed pass holds")?;
        let shared = (usage.issue == at.issue
            && usage.repository.eq_ignore_ascii_case(&at.repository))
        .then_some(comments.as_slice());
        let now_at = OffsetDateTime::from_unix_timestamp_nanos(i128::from(now) * 1_000_000)?;
        let usage = read_usage(api, &pass, shared, now_at)?;
        // No WIP count here: the lease each dispatch takes at its send
        // reserves its slot, counting every run and lease then.
        let mut busy = running(api, policy)?;
        for held in live_leases(api, &pass.dispatch_repo, now)? {
            let item = crate::github::parse_github_issue(
                &held.lease.item,
                None,
                crate::github::DEFAULT_HOST,
            )
            .context("Lease has an invalid item")?;
            busy.entry(pass::key(&item))
                .or_insert_with(|| "leased".to_owned());
        }
        Ok(Self {
            pass,
            holds,
            usage,
            now: now_at,
            open_prs: BTreeMap::new(),
            busy,
        })
    }

    /// The pass configuration whose leases the dispatches take.
    pub(super) fn config(&self) -> &pass::Config {
        &self.pass
    }

    /// Admission of `r` again at its send, as the batch's reads may be
    /// older than a start or hold since: the item's holds read anew, and the
    /// usage its lease is taken by, authenticated and adjusted as here. A
    /// lease the item holds since is the lease claim's to refuse.
    pub(super) fn at_send(&self, api: &dyn Api, policy: &Policy, r: &Request) -> Result<Value> {
        self.target(api, policy, r, &running(api, policy)?)?;
        read_usage(api, &self.pass, None, self.now)
    }

    pub(super) fn dispatch(
        &mut self,
        api: &dyn Api,
        policy: &Policy,
        r: &mut Request,
    ) -> Result<()> {
        let facts = self.target(api, policy, r, &self.busy)?;
        if let Some(cap) = self.pass.open_pr_cap.filter(|_| r.kind == "implement") {
            let repo = r.repository.to_ascii_lowercase();
            let count = match self.open_prs.get(&repo) {
                Some(&count) => count,
                None => {
                    let count = open_pr_count(api, &repo, &self.pass.bot).context("open-pr-cap")?;
                    self.open_prs.insert(repo, count);
                    count
                }
            };
            ensure!(
                count < cap,
                "open-pr-cap: {} has {count}/{cap} open bot PRs",
                r.repository
            );
        }
        ensure!(
            pass::budget_hold(&self.pass, Some(&self.usage), self.now, 1.0).is_none(),
            "budget: weekly spend is at the cap or unavailable"
        );
        let settings = self
            .pass
            .pool(&r.pool)
            .context("Missing base pool settings")?;
        pass::room(Some(&self.usage), &r.pool, settings, self.now)
            .map_err(|_| anyhow::anyhow!("pace: {} does not admit a start", r.pool))?;
        let selected = if self.pass.mix.is_empty() {
            r.pool.clone()
        } else {
            let p0 = match self.pass.github_board() {
                Some(board) => at_p0(api, board, r)?,
                None => false,
            };
            // As in stage 4, epic ancestry authorizes an implement but does not
            // make the child an epic. The executor cannot dispatch to an epic;
            // plan_epic only asks stage 3 to plan it, which does its own draw.
            mix::select_with(
                &self.pass,
                Some(&self.usage),
                (p0.then_some("P0"), Some(&facts.labels), false),
                r.pool.clone(),
                self.now,
                crate::pool_choice::index,
            )
        };
        ensure!(
            policy.pools.contains(&selected),
            "Pass selected a pool not in the supervisor's pools file"
        );
        r.selection = Some(if selected == r.pool {
            "supervisor default"
        } else {
            "pass mix"
        });
        r.pool = selected;
        Ok(())
    }

    /// What holds `r`'s item, by the published holds and the item as read
    /// now, with the issues runs or leases in `busy` held: none, or why.
    fn target(
        &self,
        api: &dyn Api,
        policy: &Policy,
        r: &Request,
        busy: &BTreeMap<String, String>,
    ) -> Result<pass::Facts> {
        use holds::HoldKind::*;
        let item = format!("{}#{}", r.repository, r.item);
        for h in &self.holds {
            if matches!(h.kind, Blocking | OverlapIgnored | Stalled)
                || h.item
                    .as_ref()
                    .is_some_and(|i| !i.eq_ignore_ascii_case(&item))
            {
                continue;
            }
            // This snapshot is private; the executor's log may not be. Its
            // fixed reason code suffices without disclosing related work.
            bail!("{}: held by the pass", h.kind.name());
        }
        let issue = super::preflight_target(api, policy, r)?;
        // Absence from the snapshot clears nothing: what the pass would hold
        // now (a person's hold, an open dependency, a trusted pull request,
        // a run or lease) holds the item here too.
        let at = board_core::ItemRef {
            host: crate::github::DEFAULT_HOST.into(),
            repository: r.repository.clone(),
            number: r.item,
        };
        let graphql = crate::supervise::Graphql(api);
        let (facts, held) = pass::outside_hold(&graphql, &self.pass, &at, busy)?;
        if let Some(h) = held {
            bail!("{}: held now", h.kind.name());
        }
        // The pass waits on the operator it assigned until they respond. Any
        // assignment of theirs holds here, which reads no history.
        if self.pass.assign_holds {
            let operator = &self.pass.operator;
            let assigned = issue["assignees"]
                .as_array()
                .context("Missing issue assignees")?
                .iter()
                .any(|a| {
                    a["login"]
                        .as_str()
                        .is_none_or(|l| l.eq_ignore_ascii_case(operator))
                });
            ensure!(!assigned, "{}: assigned to the operator", NeedsHuman.name());
        }
        Ok(facts)
    }
}

/// The issues a dispatch run is queued or running on, by [`pass::key`].
fn running(api: &dyn Api, policy: &Policy) -> Result<BTreeMap<String, String>> {
    let mut busy = BTreeMap::new();
    super::visit_active(api, policy, |run| {
        if let Some((repo, item)) = super::run_target(run) {
            let at = format!("{repo}#{item}").to_ascii_lowercase();
            busy.entry(at).or_insert_with(|| "listed".to_owned());
        }
        Ok(false)
    })?;
    Ok(busy)
}

/// The pass's usage snapshot, from `shared` where it is the holds issue's
/// comments, authenticated and adjusted by the pass's pace policy at `now`.
fn read_usage(
    api: &dyn Api,
    pass: &pass::Config,
    shared: Option<&[Value]>,
    now: OffsetDateTime,
) -> Result<Value> {
    let usage = pass
        .usage
        .as_ref()
        .context("dispatchPassRules needs published pass usage")?;
    let read;
    let comments = match shared {
        Some(comments) => comments,
        None => {
            // Only the first marker counts, so the pages after it do not.
            read = issue_comments_until(api, &usage.repository, usage.issue, COMMENT_PAGES, |c| {
                authored(c, &pass.bot)
                    && c["body"]
                        .as_str()
                        .is_some_and(|b| b.starts_with(USAGE_MARKER))
            })
            .map_err(|_| anyhow::anyhow!(SNAPSHOT_UNREADABLE))?;
            &read
        }
    };
    let usage = usage_snapshot(
        snapshot_comment(comments, &pass.bot, USAGE_MARKER).context("reading pass usage")?,
    )
    .context("Unusable pass usage snapshot")?;
    Ok(pass::adjusted_usage(pass, usage, now))
}

fn pass_config(source: &str) -> Result<pass::Config> {
    // Repository expansion is neither needed nor authorized here: the
    // supervisor's own follow list still scopes every request. Validate only
    // the pass fields used for admission, including the mix, open PR cap,
    // leases and assignment holds.
    let fields = [
        "operator",
        "bot",
        "dispatchRepo",
        "dispatchWorkflow",
        "board",
        "pools",
        "mix",
        "openPrCap",
        "wip",
        "leases",
        "assignHolds",
        "usage",
        "pace",
    ];
    let source: Value = serde_json::from_str(source).context("pass config is not JSON")?;
    let source = source.as_object().context("pass config is not an object")?;
    let mut settings: serde_json::Map<_, _> = fields
        .into_iter()
        .filter_map(|key| source.get(key).map(|value| (key.to_owned(), value.clone())))
        .collect();
    let at = source.get("holds").context("Missing pass holds")?;
    settings.insert(
        "holds".into(),
        json!({"repository": at["repository"], "issue": at["issue"]}),
    );
    if let Some(budget) = source.get("budget") {
        let mut budget = budget.as_object().context("Invalid pass budget")?.clone();
        // Keep the marker for a split-only budget without resolving its repos.
        if let Some(sources) = budget.get_mut("harnessRepositories") {
            *sources = json!([]);
        }
        settings.insert("budget".into(), Value::Object(budget));
    }
    pass::Config::parse(&Value::Object(settings).to_string())
}

fn authored(comment: &Value, bot: &str) -> bool {
    comment["author"]["login"]
        .as_str()
        .is_some_and(|n| n.eq_ignore_ascii_case(bot))
}

fn authenticated(comment: &Value, bot: &str) -> Result<()> {
    ensure!(
        comment.get("lastEditedAt").is_some()
            && comment.get("editor").is_some()
            && !own_comments(std::slice::from_ref(comment), bot).is_empty(),
        "Snapshot comment has untrusted editor evidence"
    );
    Ok(())
}

/// The bot's first comment starting with `marker`, which the pass edits in
/// place: GraphQL lists comments oldest first. No timestamp or comment id
/// orders them, and GraphQL's 32-bit `databaseId` is null for newer ones.
fn snapshot_comment<'a>(comments: &'a [Value], bot: &str, marker: &str) -> Result<&'a str> {
    let (body, comment) = comments
        .iter()
        .filter(|c| authored(c, bot))
        .find_map(|c| {
            c["body"]
                .as_str()
                .filter(|b| b.starts_with(marker))
                .map(|b| (b, c))
        })
        .context("No bot-authored snapshot comment")?;
    authenticated(comment, bot)?;
    Ok(body)
}

/// The unexpired leases in `repo` at `now`, as the pass reads them. A lease
/// whose run has ended holds until the pass reaps it: more slots counted,
/// never fewer.
fn live_leases(api: &dyn Api, repo: &str, now: i64) -> Result<Vec<lease::Held>> {
    let refs = api
        .get(&format!(
            "/repos/{repo}/git/matching-refs/{}/?per_page={LEASE_PAGE}",
            lease::LEASE_REFS
        ))
        .context("listing leases")?;
    let refs = refs.as_array().context("Malformed lease listing")?;
    ensure!(
        refs.len() < LEASE_PAGE,
        "Lease listing is too long to check"
    );
    let mut live = Vec::new();
    for r in refs {
        let name = r["ref"]
            .as_str()
            .and_then(|n| n.strip_prefix("refs/"))
            .context("Lease ref has no name")?;
        let sha = r["object"]["sha"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("Lease ref has no commit")?;
        let commit = api
            .get(&format!("/repos/{repo}/git/commits/{sha}"))
            .context("reading a lease")?;
        let Some(held) = lease::held(name, commit["message"].as_str().unwrap_or_default()) else {
            continue;
        };
        let expires = millis(&Value::from(held.lease.expires_at.as_str()));
        if expires.is_none_or(|at| at > now) {
            live.push(held);
        }
    }
    Ok(live)
}

/// Whether the issue's Priority on `board` is P0.
fn at_p0(api: &dyn Api, board: &pass::Board, r: &Request) -> Result<bool> {
    let (owner, name) = r
        .repository
        .split_once('/')
        .context("Dispatch repository")?;
    let data = api.graphql(
        BOARD_QUERY,
        json!({"owner": owner, "name": name, "number": r.item, "field": board.priority_field}),
    )?;
    let issue = &data["repository"]["issue"];
    ensure!(
        issue["number"] == r.item
            && issue["repository"]["nameWithOwner"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(&r.repository)),
        "Board issue identity did not match"
    );
    let items = &issue["projectItems"];
    ensure!(
        items["pageInfo"]["hasNextPage"] == false,
        "Incomplete board evidence"
    );
    let matches = items["nodes"]
        .as_array()
        .context("Missing board evidence")?
        .iter()
        .filter(|n| {
            n["project"]["number"] == board.project
                && n["project"]["owner"]["login"]
                    .as_str()
                    .is_some_and(|l| l.eq_ignore_ascii_case(&board.owner))
        });
    let mut selected = None;
    for n in matches {
        ensure!(
            selected.is_none(),
            "Duplicate project item in board evidence"
        );
        selected = Some(n["priority"]["name"] == "P0");
    }
    Ok(selected.unwrap_or_default())
}

fn open_pr_count(api: &dyn Api, repo: &str, bot: &str) -> Result<usize> {
    let mut open = Vec::new();
    for page in 1..=pressure::OPEN_LIMIT / 100 {
        let values = api.get(&format!(
            "/repos/{repo}/pulls?state=open&per_page=100&page={page}"
        ))?;
        let values = values
            .as_array()
            .filter(|v| v.len() <= 100)
            .context("Malformed open PR page")?;
        open.extend(values.iter().cloned());
        if values.len() < 100 {
            return pressure::census(&open, bot);
        }
    }
    bail!("Open PR listing is too long to check")
}
