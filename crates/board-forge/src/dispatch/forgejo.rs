//! Native Forgejo v17 Actions primitives, deliberately not wired to pass.
//! Response URLs are never request authority. All I/O uses the trusted instance,
//! shares one bounded budget, and performs no automatic mutation retries.
use super::{GitHub, Handle, Start, StartOutcome, Target};
use crate::forgejo::{Config, Transport};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

mod lease;
pub use lease::{Lease, Slot};

const PAGE_SIZE: usize = 50;
const PAGES: usize = 10;
const REQUESTS: usize = 500;
const DEADLINE: Duration = Duration::from_secs(15 * 60);
const ACTIVE: [&str; 3] = ["blocked", "waiting", "running"];

/// Construct only from trusted deployment settings, never plan/issue text.
/// There is no CLI switch or production activation in this adapter slice.
pub struct Forgejo<'a, T> {
    transport: &'a T,
    instance: &'a Config,
    target: &'a Target,
    inputs: BTreeSet<String>,
    bot: u64,
    remaining: Cell<usize>,
    began: Instant,
}

impl<'a, T: Transport> Forgejo<'a, T> {
    pub fn new(
        transport: &'a T,
        instance: &'a Config,
        target: &'a Target,
        accepted_inputs: BTreeSet<String>,
        bot: u64,
    ) -> Result<Self> {
        instance.validate()?;
        target.validate()?;
        ensure!(
            bot > 0 && instance.recorders.contains(&bot),
            "dispatch bot must be a configured recorder id"
        );
        ensure!(
            accepted_inputs
                .iter()
                .all(|s| ["task", "repo", "item", "kind", "pool", "base"].contains(&s.as_str()))
                && ["task", "repo", "item", "kind"]
                    .iter()
                    .all(|s| accepted_inputs.contains(*s)),
            "unsupported accepted dispatch inputs"
        );
        let backend = Self {
            transport,
            instance,
            target,
            inputs: accepted_inputs,
            bot,
            remaining: Cell::new(REQUESTS),
            began: Instant::now(),
        };
        backend.charge()?;
        let user = transport.get(&instance.api_root()?.join("user")?)?.body;
        ensure!(user["id"] == bot, "token does not belong to configured bot");
        Ok(backend)
    }

    fn charge(&self) -> Result<()> {
        ensure!(
            self.remaining.get() > 0 && self.began.elapsed() < DEADLINE,
            "Forgejo dispatch budget exhausted; held"
        );
        self.remaining.set(self.remaining.get() - 1);
        Ok(())
    }

    fn path(&self, suffix: &str) -> Result<url::Url> {
        Ok(self
            .instance
            .api_root()?
            .join(&format!("repos/{}/actions/{suffix}", self.target.caller))?)
    }

    fn get(&self, suffix: &str) -> Result<Value> {
        self.charge()?;
        Ok(self.transport.get(&self.path(suffix)?)?.body)
    }

    fn send(&self, suffix: &str, body: &Value) -> Result<(u16, Value)> {
        self.charge()?;
        self.transport.send("POST", &self.path(suffix)?, body)
    }

    fn body(&self, start: &Start) -> Result<Value> {
        crate::poll::validate_repository(start.repository)?;
        ensure!(
            self.instance
                .repositories
                .iter()
                .any(|r| r.eq_ignore_ascii_case(start.repository)),
            "target outside trusted Forgejo repository allowlist"
        );
        ensure!(
            start.item > 0 && ["implement", "research", "triage"].contains(&start.kind),
            "unsupported Forgejo job"
        );
        ensure!(
            start.runner.is_none_or(|r| r == "hosted"),
            "unsupported runner"
        );
        let mut body = GitHub {
            target: self.target,
            run_details: false,
        }
        .body(start);
        ensure!(
            body["inputs"]
                .as_object()
                .context("dispatch inputs")?
                .keys()
                .all(|key| self.inputs.contains(key)),
            "caller does not accept requested input"
        );
        body["return_run_info"] = true.into();
        Ok(body)
    }

    /// A complete listing, with a fixed total and no repeated ids. A second
    /// complete pass is required by recovery; active inventory uses two sweeps.
    fn list(&self, status: Option<&str>) -> Result<Vec<Value>> {
        let mut found = BTreeMap::new();
        let mut total = None;
        for page in 1..=PAGES {
            let mut url = self.path("runs")?;
            url.query_pairs_mut()
                .append_pair("workflow_id", &self.target.workflow)
                .append_pair("limit", &PAGE_SIZE.to_string())
                .append_pair("page", &page.to_string());
            if let Some(status) = status {
                url.query_pairs_mut().append_pair("status", status);
            }
            self.charge()?;
            let body = self.transport.get(&url)?.body;
            let count = body["total_count"]
                .as_u64()
                .context("missing Actions total_count")?;
            ensure!(
                count <= (PAGE_SIZE * PAGES) as u64 && total.is_none_or(|n| n == count),
                "Actions listing total moved or exceeds cap; held"
            );
            total = Some(count);
            let runs = body["workflow_runs"]
                .as_array()
                .context("missing workflow_runs")?;
            ensure!(runs.len() <= PAGE_SIZE, "oversized Actions page");
            for run in runs {
                let id = positive(run, "id")?;
                ensure!(
                    run["workflow_id"] == self.target.workflow,
                    "foreign workflow in listing"
                );
                active(run)?;
                ensure!(
                    found.insert(id, run.clone()).is_none(),
                    "duplicate run in listing; held"
                );
            }
            ensure!(found.len() as u64 <= count, "Actions listing exceeds total");
            if found.len() as u64 == count {
                return Ok(found.into_values().collect());
            }
            ensure!(!runs.is_empty(), "incomplete Actions listing; held");
        }
        bail!("Actions listing exceeds page cap; held")
    }

    /// Includes queued-for-runner (`waiting`) and approval (`blocked`), never
    /// actions/tasks. Current status wins over the index's filter.
    pub fn active_runs(&self) -> Result<Vec<Value>> {
        let mut runs = BTreeMap::new();
        for _ in 0..2 {
            for status in ACTIVE {
                for run in self.list(Some(status))? {
                    runs.insert(positive(&run, "id")?, run);
                }
            }
        }
        runs.into_values()
            .filter_map(|r| match active(&r) {
                Ok(true) => Some(Ok(r)),
                Ok(false) => None,
                Err(e) => Some(Err(e)),
            })
            .collect()
    }

    /// Status is native: success/failure/cancelled/skipped are terminal;
    /// there is no GitHub `completed` or run-level attempt.
    pub fn status(&self, id: u64) -> Result<Value> {
        ensure!(id > 0, "invalid run id");
        let run = self.get(&format!("runs/{id}"))?;
        ensure!(
            positive(&run, "id")? == id && run["workflow_id"] == self.target.workflow,
            "run identity mismatch"
        );
        active(&run)?;
        Ok(run)
    }

    fn verify(&self, run: &Value, start: &Start, claim: OffsetDateTime) -> Result<Handle> {
        let id = positive(run, "id")?;
        // `prettyref` is the run's ref in short form; there is no plain `ref`.
        let git_ref = &self.target.git_ref;
        let short = ["refs/heads/", "refs/tags/"]
            .iter()
            .find_map(|p| git_ref.strip_prefix(p))
            .unwrap_or(git_ref);
        ensure!(run["prettyref"] == short, "run ref outside configured ref");
        ensure!(
            run["workflow_id"] == self.target.workflow
                && run["trigger_user"]["id"] == self.bot
                && run["event"] == "workflow_dispatch"
                && run["trigger_event"] == "workflow_dispatch",
            "run trigger identity mismatch"
        );
        let created = OffsetDateTime::parse(
            run["created"].as_str().context("missing run created")?,
            &Rfc3339,
        )?;
        // Forgejo records `created` in whole seconds, unlike the durable claim.
        ensure!(
            created >= claim.replace_nanosecond(0)?,
            "run predates claim"
        );
        let body = self.body(start)?;
        let payload: Value = serde_json::from_str(
            run["event_payload"]
                .as_str()
                .context("event_payload must be a JSON string")?,
        )?;
        for (key, value) in body["inputs"].as_object().context("inputs")? {
            if value != "" {
                ensure!(payload["inputs"][key] == *value, "run input {key} mismatch");
            }
        }
        let pool = start
            .pool
            .filter(|p| !p.is_empty())
            .map(|p| format!(" ({p})"))
            .unwrap_or_default();
        ensure!(
            run["title"] == format!("{} {}#{}{pool}", start.kind, start.repository, start.item),
            "run title mismatch"
        );
        // `html_url` is built from the server's public ROOT_URL, which an
        // in-cluster instance URL never matches; check the repository and
        // build the link here. Forgejo URLs use index_in_repo, not the run id.
        ensure!(
            run["repository"]["full_name"]
                .as_str()
                .is_some_and(|r| r.eq_ignore_ascii_case(&self.target.caller)),
            "run outside configured caller"
        );
        let index = positive(run, "index_in_repo")?;
        let url = self
            .instance
            .base()
            .join(&format!("{}/actions/runs/{index}", self.target.caller))?
            .to_string();
        active(run)?;
        Ok(Handle {
            id: id.to_string(),
            url,
        })
    }

    /// Recovery is read-only. Exactly one verified run in two complete stable
    /// inventories; never pick the newest title or resend a missing start.
    pub fn recover(&self, start: &Start, claim: OffsetDateTime) -> Result<Handle> {
        self.body(start)?;
        let first = self.list(None)?;
        let second = self.list(None)?;
        let ids = |runs: &[Value]| -> Result<BTreeSet<u64>> {
            runs.iter().map(|r| positive(r, "id")).collect()
        };
        ensure!(
            ids(&first)? == ids(&second)?,
            "Actions inventory moved; held"
        );
        let matches: Vec<_> = second
            .iter()
            .filter_map(|r| self.verify(r, start, claim).ok())
            .collect();
        ensure!(
            matches.len() == 1,
            "expected one verified start, found {}; held, do not POST again",
            matches.len()
        );
        let handle = &matches[0];
        self.verify(&self.status(handle.id.parse()?)?, start, claim)
    }

    /// `admit` must durably claim the plan and reserve capacity, rechecking
    /// caps/pacing/ownership at this final send boundary. An ambiguous outcome
    /// keeps that reservation. Calling again for the same claim is forbidden.
    pub fn start(
        &self,
        start: &Start,
        claim: OffsetDateTime,
        mut admit: impl FnMut() -> Result<()>,
    ) -> StartOutcome {
        let mut preflight = || -> Result<Value> {
            let body = self.body(start)?;
            // Confirmation lists the whole history; a start it could not list
            // would be ambiguous, so refuse while there is no room for one more.
            ensure!(
                self.list(None)?.len() < PAGE_SIZE * PAGES,
                "Actions history at listing cap; a start could not be confirmed"
            );
            self.active_runs()?;
            admit()?;
            Ok(body)
        };
        let body = match preflight() {
            Ok(b) => b,
            Err(e) => return StartOutcome::NotStarted(e),
        };
        let reply = self.send(
            &format!("workflows/{}/dispatches", self.target.workflow),
            &body,
        );
        match reply {
            Ok((201, answer)) if answer["id"].as_u64().is_some_and(|id| id > 0) => {
                let id = answer["id"].as_u64().unwrap_or_default();
                let run = match self.status(id) {
                    Ok(run) => run,
                    // An unreadable status is not evidence of a wrong run.
                    Err(e) => return StartOutcome::Ambiguous(e),
                };
                match self.verify(&run, start, claim) {
                    Ok(handle) => match self.recover(start, claim) {
                        Ok(found) if found == handle => StartOutcome::Started(handle),
                        _ => StartOutcome::Ambiguous(anyhow::anyhow!(
                            "start listing not uniquely confirmed; held"
                        )),
                    },
                    Err(e) => {
                        // The named run went wrong. Cancellation is best effort,
                        // never a reason to free capacity or retry dispatch.
                        let _ = self.send(&format!("runs/{id}/cancel"), &json!({}));
                        StartOutcome::Ambiguous(e)
                    }
                }
            }
            Ok((status, _)) if (400..500).contains(&status) => {
                StartOutcome::NotStarted(anyhow::anyhow!("Forgejo dispatch refused: HTTP {status}"))
            }
            Ok((status, _)) => StartOutcome::Ambiguous(anyhow::anyhow!(
                "Forgejo start unconfirmed: HTTP {status}; held, do not retry"
            )),
            Err(e) => StartOutcome::Ambiguous(e),
        }
    }

    /// At most one rerun, guarded by complete per-job attempts and a durable
    /// caller claim in `admit`. A lost reply is not retried, even if jobs still
    /// appear at attempt 1. Returns the same handle, not a new run id.
    pub fn rerun(
        &self,
        id: u64,
        start: &Start,
        claim: OffsetDateTime,
        mut admit: impl FnMut() -> Result<()>,
    ) -> StartOutcome {
        let mut prepare = || -> Result<Handle> {
            let run = self.status(id)?;
            let handle = self.verify(&run, start, claim)?;
            ensure!(run["status"] == "failure", "only failed runs may be rerun");
            let jobs = self.get(&format!("runs/{id}/jobs"))?;
            let jobs = jobs.as_array().context("jobs must be a complete array")?;
            ensure!(
                !jobs.is_empty() && jobs.len() <= PAGE_SIZE * PAGES,
                "empty or oversized job listing"
            );
            let mut ids = BTreeSet::new();
            for job in jobs {
                ensure!(
                    job["run_id"] == id
                        && job["attempt"] == 1
                        && !active(job)?
                        && ids.insert(positive(job, "id")?),
                    "jobs active, duplicated, foreign or already rerun"
                );
            }
            admit()?;
            Ok(handle)
        };
        let handle = match prepare() {
            Ok(h) => h,
            Err(e) => return StartOutcome::NotStarted(e),
        };
        match self.send(&format!("runs/{id}/rerun"), &json!({})) {
            Ok((204, _)) => StartOutcome::Started(handle),
            Ok((status, _)) if (400..500).contains(&status) => {
                StartOutcome::NotStarted(anyhow::anyhow!("Forgejo rerun refused: HTTP {status}"))
            }
            _ => StartOutcome::Ambiguous(anyhow::anyhow!(
                "Forgejo rerun reply unknown; held, do not retry"
            )),
        }
    }
}

fn positive(value: &Value, key: &str) -> Result<u64> {
    value[key]
        .as_u64()
        .filter(|id| *id > 0)
        .with_context(|| format!("missing positive {key}"))
}

fn active(run: &Value) -> Result<bool> {
    match run["status"].as_str() {
        Some("waiting" | "blocked" | "running") => Ok(true),
        Some("success" | "failure" | "cancelled" | "skipped") => Ok(false),
        _ => bail!("unknown Forgejo status; held"),
    }
}

#[cfg(test)]
mod tests;
