//! Search results order decisions; only fresh PR facts and the round ledger authorize them.
use super::*;

const SEARCH_LIMIT: usize = 100;
const RED_CHECK_REQUEST_LIMIT: usize = 192;
const REVIEW_TURN_REQUEST_LIMIT: usize = 192;
const DEFAULT_PENDING_HOURS: u16 = 6;
const MAX_PENDING_HOURS: u16 = 24 * 7;

#[derive(Clone, Copy, Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RedChecks {
    #[serde(default)]
    pending_hours: Option<u16>,
}

impl RedChecks {
    pub(super) fn pending_hours(self) -> u16 {
        self.pending_hours.unwrap_or(DEFAULT_PENDING_HOURS)
    }

    pub(super) fn validate(self, at: &str) -> Result<()> {
        ensure!(
            (1..=MAX_PENDING_HOURS).contains(&self.pending_hours()),
            "{at}redChecks.pendingHours is not 1 to {MAX_PENDING_HOURS}"
        );
        Ok(())
    }

    pub(super) fn expired(self, since: Option<&str>, now: OffsetDateTime) -> bool {
        timestamp(since).is_some_and(|since| {
            now - since > time::Duration::hours(i64::from(self.pending_hours()))
        })
    }
}

/// Presence opts into the turn; there are no tunable bounds.
#[derive(Clone, Copy, Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewTurn {}

pub(super) fn review_turn_option<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<ReviewTurn>, D::Error> {
    use serde::de::Error;
    let value = <Value as serde::Deserialize>::deserialize(deserializer)?;
    if !value.is_object() {
        return Err(D::Error::custom("reviewTurn must be an empty object"));
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(D::Error::custom)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PriorityTurn {
    RedCheck,
    Review,
}

impl PriorityTurn {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::RedCheck => "red-check",
            Self::Review => "review-turn",
        }
    }

    fn searches(self) -> &'static [&'static str] {
        match self {
            Self::RedCheck => &["status:failure", "status:pending"],
            Self::Review => &["review:changes_requested"],
        }
    }

    fn request_limit(self) -> usize {
        match self {
            Self::RedCheck => RED_CHECK_REQUEST_LIMIT,
            Self::Review => REVIEW_TURN_REQUEST_LIMIT,
        }
    }
}

/// No decision was taken in the priority turn. The ordinary walk may still
/// need to handle this PR (for example, to reconcile a completed fix run).
#[derive(Debug)]
pub(super) struct PrioritySkip(pub(super) &'static str);

impl std::fmt::Display for PrioritySkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for PrioritySkip {}

/// A search-derived read provided no public evidence. Do not report its PR.
#[derive(Debug)]
pub(super) struct RedHidden;

impl std::fmt::Display for RedHidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("red-check PR visibility unconfirmed")
    }
}

impl std::error::Error for RedHidden {}

pub(super) fn require_public(pull: &Value, owner: &str, name: &str) -> Result<()> {
    let repository = &pull["baseRepository"];
    ensure!(
        repository["visibility"] == "PUBLIC"
            && text(repository, "/nameWithOwner").eq_ignore_ascii_case(&format!("{owner}/{name}")),
        RedHidden
    );
    Ok(())
}

fn red_turn_stopped(error: &anyhow::Error) -> bool {
    error.is::<crate::github::RateLimited>() || crate::pass::budget_spent(error)
}

pub(super) fn red_evidence_error(error: anyhow::Error) -> anyhow::Error {
    if red_turn_stopped(&error) {
        error
    } else {
        RedHidden.into()
    }
}

fn report_red_stop(outcome: &mut Outcome, error: &anyhow::Error) {
    let reason = if error.is::<crate::github::RateLimited>() {
        outcome.errors += 1;
        "GitHub rate limited"
    } else {
        "request budget spent"
    };
    outcome
        .lines
        .push(format!("pull requests: red-check turn stopped; {reason}"));
}

pub(super) fn search_path(config: &Config, qualifier: &str) -> String {
    let owner = config.dispatch_repo.split('/').next().unwrap_or_default();
    let public = if qualifier.starts_with("status:") {
        " is:public"
    } else {
        ""
    };
    let query = format!(
        "author:{bot} user:{owner} user:{bot} is:pr is:open {qualifier} archived:false{public}",
        bot = config.bot
    );
    let query: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    format!("search/issues?q={query}&sort=updated&order=asc&per_page={SEARCH_LIMIT}")
}

impl<R: Rest, G: Transport> Pass<'_, R, G> {
    /// A newer formal verdict by the same reviewer on another commit
    /// invalidates the priority evidence. Never let a job veto the operator.
    /// If the ordinary selector falls back to a job, provenance is checked
    /// separately before its review can authorize a fix.
    fn fresh_formal<'a>(&self, pull: &'a Value) -> Verdict<'a> {
        let (operator, job) = verdicts(pull, self.config);
        let candidate = operator.or(job)?;
        let by_operator = operator.is_some();
        let reviews: Vec<_> = nodes(pull, "/reviews/nodes").iter().collect();
        let latest = newest_verdict(
            &reviews,
            |r| {
                if by_operator {
                    operator_review(r, self.config)
                } else {
                    job_review(r)
                }
            },
            false,
        );
        (latest == Some(candidate)).then_some(candidate)
    }

    pub(super) fn review_turn_candidate(
        &self,
        repo: &str,
        number: u64,
        pull: &Value,
    ) -> Result<()> {
        if pull["state"] != "OPEN"
            || pull["number"].as_u64() != Some(number)
            || text(pull, "/author/login") != self.config.bot
            || !text(pull, "/baseRepository/nameWithOwner").eq_ignore_ascii_case(repo)
        {
            return Err(PrioritySkip("fresh open PR identity does not match search result").into());
        }
        if pull["isDraft"] != false
            || !pull["mergeQueueEntry"].is_null()
            || !pull["autoMergeRequest"].is_null()
            || pull.pointer("/labels/pageInfo/hasNextPage") != Some(&Value::Bool(false))
            || !pull.pointer("/labels/nodes").is_some_and(Value::is_array)
            || is_held(pull)
            || labelled(pull)
            || review_hold(self.config, pull).is_some()
        {
            return Err(PrioritySkip("PR is held, draft, queued, or has unread holds").into());
        }
        let head = text(pull, "/headRefOid");
        if head.is_empty()
            || pull.pointer("/reviews/pageInfo/hasPreviousPage") != Some(&Value::Bool(false))
            || !matches!(self.fresh_formal(pull), Some((Review::ChangesRequested, _)))
        {
            return Err(PrioritySkip("no fresh formal change request on this head").into());
        }
        Ok(())
    }

    pub(super) fn review_turn_verdict<'a>(
        &self,
        pull: &'a Value,
        run: Option<&Value>,
    ) -> Result<&'a Value> {
        match head_verdict(pull, self.config, run) {
            Some((Review::ChangesRequested, review))
                if reviewed_on_base(pull, self.config, run) =>
            {
                Ok(review)
            }
            _ => Err(PrioritySkip("formal change request is not an eligible verdict").into()),
        }
    }

    pub(super) fn pull_review_run(&mut self, repo: &str, pull: &Value) -> Result<Option<Value>> {
        match review_run_id(pull, self.config) {
            Some(run) if !labelled(pull) => Ok(Some(
                self.get_url(
                    &format!("{API_ROOT}repos/{repo}/actions/runs/{run}"),
                    slim_run,
                    false,
                )?
                .0,
            )),
            _ => Ok(None),
        }
    }

    pub(super) fn priority_turn(
        &mut self,
        turn: PriorityTurn,
        follow: &BTreeSet<&String>,
        missing: &BTreeSet<String>,
        outcome: &mut Outcome,
    ) {
        if !follow.iter().any(|r| self.config.ours(r)) {
            return;
        }
        if turn == PriorityTurn::RedCheck && !self.rest.allows_sensitive_reads() {
            outcome
                .lines
                .push("pull requests: red-check turn skipped in record/replay".into());
            return;
        }
        // Leave the ordinary walk room for both discovery and decisions. The
        // turn can use at most half the remaining share, with a local ceiling.
        let share = self
            .remaining
            .min(self.rest.share_left().unwrap_or(usize::MAX));
        let allowance = (share / 2).min(turn.request_limit());
        if allowance < PULL_SHARE_MARGIN + 2 {
            outcome.lines.push(format!(
                "pull requests: {} turn deferred; not enough request share",
                turn.name()
            ));
            return;
        }
        // This is an admission floor, not a cap on an operation already under
        // way. Once a claim is posted, its read-back and dispatch must finish
        // under the normal pass bounds, even past this turn's allowance.
        let floor = self.remaining - allowance;
        let cut = self.cut.take();
        self.priority_candidates(turn, follow, missing, floor, outcome);
        self.cut = cut;
    }

    fn priority_candidates(
        &mut self,
        turn: PriorityTurn,
        follow: &BTreeSet<&String>,
        missing: &BTreeSet<String>,
        floor: usize,
        outcome: &mut Outcome,
    ) {
        let mut seen = BTreeSet::new();
        let mut admitted = Vec::new();
        let kind = turn.name();
        for &qualifier in turn.searches() {
            // Keep the red-check diagnostics stable.
            let search = qualifier.strip_prefix("status:").unwrap_or(qualifier);
            let listed = match self.priority_search(turn, qualifier) {
                Ok((listed, partial)) => {
                    if partial {
                        outcome.lines.push(format!(
                            "pull requests: {kind} {search} search incomplete; only returned PRs considered"
                        ));
                    }
                    listed
                }
                Err(error) if turn == PriorityTurn::RedCheck && red_turn_stopped(&error) => {
                    report_red_stop(outcome, &error);
                    return;
                }
                Err(error) => {
                    outcome.error(&format!("pull requests: {kind} {search} search"), &error);
                    continue;
                }
            };
            for (repo, number) in listed {
                let key = (repo.to_ascii_lowercase(), number);
                if !seen.insert(key.clone()) {
                    continue;
                }
                let followed = follow.iter().find(|r| r.eq_ignore_ascii_case(&repo));
                let skip = if outcome.decided.contains(&key) {
                    Some("already decided this pass")
                } else if followed.is_none() || !self.config.ours(&repo) {
                    Some("outside followed own-space repositories")
                } else if missing.contains(&key.0) {
                    Some("repository not found")
                } else if self.envelope.as_ref().is_some_and(|e| !e.admits(&repo)) {
                    Some("outside this pass's envelope")
                } else {
                    None
                };
                if let Some(why) = skip {
                    if turn != PriorityTurn::RedCheck {
                        outcome
                            .lines
                            .push(format!("{repo}#{number} {kind} skip: {why}"));
                    }
                } else if let Some(repo) = followed {
                    admitted.push((repo.as_str(), number));
                }
            }
        }
        // A stable prefix of marked heads must not spend the turn every pass.
        // Rotate the bounded set at the same cadence as the ordinary walk.
        if !admitted.is_empty() {
            admitted.sort_unstable();
            let start = clock_start_index(self.now, admitted.len());
            admitted.rotate_left(start);
        }
        for group in admitted.chunk_by(|a, b| a.0 == b.0 && a.1 < b.1) {
            let repo = group[0].0;
            let open = group
                .iter()
                .map(|&(_, number)| {
                    json!({
                        "number": number, "user": {"login": self.config.bot}
                    })
                })
                .collect();
            if let Err(error) = self.follow_candidates(repo, open, Some((turn, floor)), outcome) {
                if turn == PriorityTurn::RedCheck {
                    report_red_stop(outcome, &error);
                    break;
                }
                outcome.error(repo, &error);
            }
        }
    }

    fn priority_search(
        &mut self,
        turn: PriorityTurn,
        qualifier: &str,
    ) -> Result<(Vec<(String, u64)>, bool)> {
        let kind = turn.name();
        let path = search_path(self.config, qualifier);
        let response = if turn == PriorityTurn::RedCheck {
            self.spend()?;
            self.rest.get_sensitive(&format!("{API_ROOT}{path}"))?.body
        } else {
            self.get(&path)?
        };
        let items = response["items"]
            .as_array()
            .with_context(|| format!("{kind} search has no items"))?;
        let total = response["total_count"]
            .as_u64()
            .with_context(|| format!("{kind} search has no count"))?;
        let incomplete = response["incomplete_results"]
            .as_bool()
            .with_context(|| format!("{kind} search has no completeness flag"))?;
        ensure!(
            items.len() <= SEARCH_LIMIT && items.len() as u64 <= total,
            "{kind} search exceeds its result bound or reported count"
        );
        let listed = items
            .iter()
            .map(|item| {
                let repo = item["repository_url"]
                    .as_str()
                    .and_then(|s| s.strip_prefix(&format!("{API_ROOT}repos/")))
                    .with_context(|| format!("{kind} search has an invalid repository URL"))?;
                if turn == PriorityTurn::RedCheck {
                    ensure!(
                        validate_repository(repo).is_ok(),
                        "{kind} search has an invalid repository name"
                    );
                } else {
                    validate_repository(repo)?;
                }
                let number = item["number"]
                    .as_u64()
                    .filter(|&n| n > 0 && n <= i32::MAX as u64 && item["pull_request"].is_object())
                    .with_context(|| format!("{kind} search has an invalid PR identity"))?;
                Ok((repo.to_owned(), number))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((listed, incomplete || (items.len() as u64) < total))
    }
}
