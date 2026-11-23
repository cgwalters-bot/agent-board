//! Search results order decisions; only fresh checks and the round ledger authorize them.
use super::*;

const SEARCH_LIMIT: usize = 100;
const TURN_REQUEST_LIMIT: usize = 192;
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

/// No decision was taken in the priority turn. The ordinary walk may still
/// need to handle this PR (for example, to reconcile a completed fix run).
#[derive(Debug)]
pub(super) struct RedSkip(pub(super) &'static str);

impl std::fmt::Display for RedSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for RedSkip {}

pub(super) fn search_path(config: &Config, status: &str) -> String {
    let owner = config.dispatch_repo.split('/').next().unwrap_or_default();
    let query = format!(
        "author:{bot} user:{owner} user:{bot} is:pr is:open status:{status} archived:false",
        bot = config.bot
    );
    let query: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    format!("search/issues?q={query}&sort=updated&order=asc&per_page={SEARCH_LIMIT}")
}

impl<R: Rest, G: Transport> Pass<'_, R, G> {
    pub(super) fn red_checks(
        &mut self,
        follow: &BTreeSet<&String>,
        missing: &BTreeSet<String>,
        outcome: &mut Outcome,
    ) {
        if !follow.iter().any(|r| self.config.ours(r)) {
            return;
        }
        // Leave the ordinary walk room for both discovery and decisions. The
        // turn can use at most half the remaining share, with a local ceiling.
        let share = self
            .remaining
            .min(self.rest.share_left().unwrap_or(usize::MAX));
        let allowance = (share / 2).min(TURN_REQUEST_LIMIT);
        if allowance < PULL_SHARE_MARGIN + 2 {
            outcome
                .lines
                .push("pull requests: red-check turn deferred; not enough request share".into());
            return;
        }
        // This is an admission floor, not a cap on an operation already under
        // way. Once a claim is posted, its read-back and dispatch must finish
        // under the normal pass bounds, even past this turn's allowance.
        let floor = self.remaining - allowance;
        let cut = self.cut.take();
        self.red_candidates(follow, missing, floor, outcome);
        self.cut = cut;
    }

    fn red_candidates(
        &mut self,
        follow: &BTreeSet<&String>,
        missing: &BTreeSet<String>,
        floor: usize,
        outcome: &mut Outcome,
    ) {
        let mut seen = BTreeSet::new();
        let mut admitted = Vec::new();
        for status in ["failure", "pending"] {
            let listed = match self.red_search(status) {
                Ok((listed, partial)) => {
                    if partial {
                        outcome.lines.push(format!(
                            "pull requests: red-check {status} search incomplete; only returned PRs considered"
                        ));
                    }
                    listed
                }
                Err(error) => {
                    outcome.error(&format!("pull requests: red-check {status} search"), &error);
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
                    outcome
                        .lines
                        .push(format!("{repo}#{number} red-check skip: {why}"));
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
            if let Err(error) = self.follow_candidates(repo, open, Some(floor), outcome) {
                outcome.error(repo, &error);
            }
        }
    }

    fn red_search(&mut self, status: &str) -> Result<(Vec<(String, u64)>, bool)> {
        let response = self.get(&search_path(self.config, status))?;
        let items = response["items"]
            .as_array()
            .context("red-check search has no items")?;
        let total = response["total_count"]
            .as_u64()
            .context("red-check search has no count")?;
        let incomplete = response["incomplete_results"]
            .as_bool()
            .context("red-check search has no completeness flag")?;
        ensure!(
            items.len() <= SEARCH_LIMIT && items.len() as u64 <= total,
            "red-check search exceeds its result bound or reported count"
        );
        let listed = items
            .iter()
            .map(|item| {
                let repo = item["repository_url"]
                    .as_str()
                    .and_then(|s| s.strip_prefix(&format!("{API_ROOT}repos/")))
                    .context("red-check search has an invalid repository URL")?;
                validate_repository(repo)?;
                let number = item["number"]
                    .as_u64()
                    .filter(|&n| n > 0 && n <= i32::MAX as u64 && item["pull_request"].is_object())
                    .context("red-check search has an invalid PR identity")?;
                Ok((repo.to_owned(), number))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((listed, incomplete || (items.len() as u64) < total))
    }
}
