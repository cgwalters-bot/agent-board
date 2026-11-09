//! Opt-in rolling CI observations. No classification is inferred from logs.
use crate::poll::{validate_repository, validate_workflow};
use crate::supervise::{Api, Bounded, Limits, iso, millis};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

const DAY: i64 = 86_400_000;
const PAGES: usize = 10;
const LOG_BYTES: u64 = 1024 * 1024;
const TOTAL_BYTES: usize = 32 * 1024 * 1024;
const REQUESTS: u32 = 500;
const SECONDS: u64 = 120;

/// Trusted local CI health settings, at most 1 MiB; unknown fields are rejected.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Explicit repository/workflow pairs: 1 to 20, with no duplicates or discovery.
    pub targets: Vec<Target>,

    /// Optional reviewed classifications keyed by a prior report's 64-hex signature.
    /// Observations do not assign classes; caller assertions are copied verbatim.
    #[serde(default)]
    pub index: BTreeMap<String, Classification>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    /// GitHub OWNER/REPO whose Actions history is read.
    pub repository: String,

    /// Workflow file name, for example ci.yml, not its display name.
    pub workflow: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Classification {
    /// Reviewed class: unknown, flake, regression or infrastructure.
    pub class: String,

    /// Nonempty list of HTTPS evidence links; relevance is the caller's responsibility.
    pub evidence: Vec<String>,

    /// Nonblank responsible person or team.
    pub owner: String,

    /// Optional GitHub issue URL; a URL alone does not verify issue state.
    pub issue: Option<String>,

    /// Caller-verified fix state: unknown, open, staged or deployed.
    /// A green run never promotes staged to deployed.
    pub fix_status: String,

    /// RFC 3339 first-seen timestamp, no later than the report's exclusive until.
    pub first_seen: String,

    /// Optional RFC 3339 explanation timestamp between first_seen and until.
    /// Must be paired with explanation.
    pub explained_at: Option<String>,

    /// Nonblank explanation, paired with explained_at; absent/null means unexplained.
    pub explanation: Option<String>,
}

impl Config {
    pub fn parse(text: &str, until: i64) -> Result<Self> {
        ensure!(text.len() <= 1024 * 1024, "config exceeds 1 MiB");
        let config: Self = serde_json::from_str(text).context("CI health config")?;
        ensure!(
            !config.targets.is_empty() && config.targets.len() <= 20,
            "expected 1..20 targets"
        );
        let mut seen = BTreeSet::new();
        for target in &config.targets {
            validate_repository(&target.repository)?;
            validate_workflow(&target.workflow)?;
            ensure!(
                seen.insert((&target.repository, &target.workflow)),
                "duplicate target"
            );
        }
        for (signature, entry) in &config.index {
            ensure!(
                signature.len() == 64 && signature.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid signature"
            );
            ensure!(
                ["flake", "regression", "infrastructure", "unknown"]
                    .contains(&entry.class.as_str()),
                "invalid class"
            );
            ensure!(
                ["unknown", "open", "staged", "deployed"].contains(&entry.fix_status.as_str()),
                "invalid fix_status"
            );
            ensure!(
                !entry.owner.trim().is_empty()
                    && !entry.evidence.is_empty()
                    && entry.evidence.iter().all(|s| s.starts_with("https://")),
                "owner and HTTPS evidence required"
            );
            if let Some(issue) = &entry.issue {
                ensure!(issue.starts_with("https://"), "issue must be an HTTPS URL");
                crate::github::parse_github_issue(issue, None, crate::github::DEFAULT_HOST)
                    .context("invalid issue URL")?;
            }
            let first = millis(&json!(entry.first_seen)).context("invalid first_seen")?;
            ensure!(first <= until, "future first_seen");
            match (&entry.explained_at, &entry.explanation) {
                (Some(at), Some(text)) => {
                    let at = millis(&json!(at)).context("invalid explained_at")?;
                    ensure!(
                        at >= first && at <= until && !text.trim().is_empty(),
                        "invalid explanation date/text"
                    );
                }
                (None, None) => (),
                _ => anyhow::bail!("explanation and explained_at must be paired"),
            }
        }
        Ok(config)
    }
}

/// All rates are observed rates, never a healthy verdict or a coverage estimate.
#[derive(Default, Serialize)]
struct Counts {
    runs: usize,
    attempts: usize,
    expected_attempts: usize,
    success: usize,
    failed: usize,
    cancelled: usize,
    skipped: usize,
    in_progress: usize,
    other: usize,
    first_attempt_failed: usize,
    first_attempt_success: usize,
    first_attempt_denominator: usize,
    first_attempt_pass_rate: Option<f64>,
    denominator: usize,
    pass_rate: Option<f64>,
}

impl Counts {
    fn observe(&mut self, run: &Value, attempt: u64) {
        self.attempts += 1;
        if run["status"] != "completed" {
            self.in_progress += 1;
            return;
        }
        match run["conclusion"].as_str().unwrap_or_default() {
            "success" => {
                self.success += 1;
                self.first_attempt_success += usize::from(attempt == 1);
            }
            "failure" | "timed_out" | "startup_failure" | "action_required" => {
                self.failed += 1;
                self.first_attempt_failed += usize::from(attempt == 1);
            }
            "cancelled" => self.cancelled += 1,
            "skipped" => self.skipped += 1,
            _ => self.other += 1,
        }
        self.denominator = self.success + self.failed;
        self.pass_rate =
            (self.denominator > 0).then(|| self.success as f64 / self.denominator as f64);
        self.first_attempt_denominator = self.first_attempt_success + self.first_attempt_failed;
        self.first_attempt_pass_rate = (self.first_attempt_denominator > 0)
            .then(|| self.first_attempt_success as f64 / self.first_attempt_denominator as f64);
    }
}

fn failed(value: &Value) -> bool {
    ["failure", "timed_out", "startup_failure", "action_required"]
        .contains(&value["conclusion"].as_str().unwrap_or_default())
}

struct Reader<'a> {
    api: &'a dyn Api,
    bytes: Cell<usize>,
    aggregate_bytes: &'a Cell<usize>,
    byte_limit: usize,
    deadline: Cell<i64>,
    clock: &'a dyn Fn() -> i64,
    operations: Cell<u32>,
    operation_limit: u32,
}

impl Reader<'_> {
    fn charge(&self, bytes: usize) -> Result<()> {
        let total = self.bytes.get().saturating_add(bytes);
        self.bytes.set(total);
        let aggregate = self.aggregate_bytes.get().saturating_add(bytes);
        self.aggregate_bytes.set(aggregate);
        ensure!(aggregate <= TOTAL_BYTES, "aggregate byte budget exhausted");
        ensure!(total <= self.byte_limit, "target byte allocation exhausted");
        Ok(())
    }

    fn get(&self, path: &str) -> Result<Value> {
        self.available()?;
        self.operations.set(self.operations.get() + 1);
        let value = self.api.get(path)?;
        self.charge(serde_json::to_vec(&value)?.len())?;
        Ok(value)
    }

    fn available(&self) -> Result<()> {
        ensure!(
            self.operations.get() < self.operation_limit,
            "target request allocation exhausted"
        );
        ensure!(
            self.aggregate_bytes.get() < TOTAL_BYTES,
            "aggregate byte budget exhausted"
        );
        ensure!(
            self.bytes.get() < self.byte_limit,
            "target byte allocation exhausted"
        );
        ensure!(
            (self.clock)() < self.deadline.get(),
            "target time allocation exhausted"
        );
        Ok(())
    }

    fn pages(&self, path: &str, key: &str) -> Result<Vec<Value>> {
        let mut all = vec![];
        let mut total = None;
        for page in 1..=PAGES {
            let separator = if path.contains('?') { '&' } else { '?' };
            let value = self.get(&format!("{path}{separator}per_page=100&page={page}"))?;
            let count = value["total_count"]
                .as_u64()
                .context("missing total_count")? as usize;
            ensure!(
                total.is_none_or(|n| n == count),
                "pagination changed while reading"
            );
            total = Some(count);
            let items = value[key].as_array().context("missing page items")?;
            ensure!(items.len() <= 100, "oversized page");
            all.extend(items.iter().cloned());
            if all.len() == count {
                return Ok(all);
            }
            ensure!(!items.is_empty() && all.len() < count, "incomplete pages");
        }
        anyhow::bail!("pagination limit reached (at most 1000 records)")
    }
}

/// First error, not a trailing process exit annotation. Preserve digits/SHAs:
/// different error codes and test names must not acquire the same identity.
fn first_error(log: &str) -> Option<(String, String)> {
    let clean = crate::supervise::health::strip_colours(log);
    let mut test = String::new();
    for line in clean.lines() {
        let line = crate::supervise::health::log_line(line)
            .unwrap_or(line)
            .trim();
        if let Some(name) = line
            .strip_prefix("---- ")
            .and_then(|s| s.strip_suffix(" stdout ----"))
        {
            test = name.to_owned();
        }
        let lower = line.to_ascii_lowercase();
        if (lower.contains("##[error]")
            || lower.starts_with("error:")
            || lower.starts_with("error[")
            || lower.contains(" panicked at "))
            && !lower.contains("process completed with exit code")
        {
            return Some((test, line.split_whitespace().collect::<Vec<_>>().join(" ")));
        }
    }
    None
}

#[derive(Serialize)]
struct Group {
    signature: String,
    repository: String,
    job: String,
    test: String,
    first_error: String,
    occurrences: usize,
    run_ids: BTreeSet<u64>,
    first_seen: String,
    samples: Vec<Value>,
}

fn groups_for(
    reader: &Reader<'_>,
    root: &str,
    repo: &str,
    run: &Value,
    attempt: u64,
    groups: &mut BTreeMap<String, Group>,
) -> Result<()> {
    let id = run["id"].as_u64().context("run id")?;
    let jobs = reader.pages(
        &format!("{root}/actions/runs/{id}/attempts/{attempt}/jobs"),
        "jobs",
    )?;
    ensure!(!jobs.is_empty(), "failed attempt has no jobs");
    let mut observed = false;
    let mut seen = BTreeSet::new();
    for job in jobs.iter().filter(|j| failed(j)) {
        observed = true;
        let job_id = job["id"].as_u64().context("job id")?;
        ensure!(seen.insert(job_id), "duplicate job id in pages");
        reader.available()?;
        reader.operations.set(reader.operations.get() + 1);
        // A full-size log near the end of a share would predictably spend
        // another target's bytes. A share-truncated log is not signature evidence.
        let limit = LOG_BYTES
            .min((reader.byte_limit - reader.bytes.get()) as u64)
            .min((TOTAL_BYTES - reader.aggregate_bytes.get()) as u64);
        let log = reader
            .api
            .download(&format!("{root}/actions/jobs/{job_id}/logs"), limit)?;
        reader.charge(log.len())?;
        ensure!(
            log.len() < limit as usize,
            "possibly truncated log (log bound or byte allocation)"
        );
        let (test, error) =
            first_error(std::str::from_utf8(&log)?).context("no meaningful first error in log")?;
        let name = job["name"].as_str().context("job name")?;
        let signature = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(repo, name, &test, &error))?)
        );
        let group = groups.entry(signature.clone()).or_insert_with(|| Group {
            signature,
            repository: repo.into(),
            job: name.into(),
            test,
            first_error: error.chars().take(500).collect(),
            occurrences: 0,
            run_ids: BTreeSet::new(),
            first_seen: run["created_at"].as_str().unwrap_or_default().into(),
            samples: vec![],
        });
        group.occurrences += 1;
        group.run_ids.insert(id);
        if run["created_at"].as_str().unwrap_or_default() < group.first_seen.as_str() {
            group.first_seen = run["created_at"].as_str().unwrap_or_default().into();
        }
        if group.samples.len() < 3 {
            group
                .samples
                .push(json!({"run_id":id,"attempt":attempt,"job_id":job_id,"url":run["html_url"]}));
        }
    }
    ensure!(observed, "failed attempt has no failed jobs");
    Ok(())
}

struct History {
    branch: Option<String>,
    total: Option<usize>,
    listed: usize,
    stopped: bool,
    listing_complete: bool,
    seen: BTreeSet<u64>,
    runs: Vec<(Value, &'static str, u64)>,
    populations: BTreeMap<&'static str, Counts>,
    warnings: Vec<String>,
}

impl History {
    fn new() -> Self {
        Self {
            branch: None,
            total: None,
            listed: 0,
            stopped: false,
            listing_complete: false,
            seen: BTreeSet::new(),
            runs: vec![],
            populations: BTreeMap::from([
                ("default_push", Counts::default()),
                ("pull_request", Counts::default()),
            ]),
            warnings: vec![],
        }
    }

    fn page(&mut self, value: Value, since: i64, until: i64) -> Result<()> {
        let total = value["total_count"]
            .as_u64()
            .context("missing total_count")? as usize;
        ensure!(
            self.total.is_none_or(|n| n == total),
            "pagination changed while reading"
        );
        self.total = Some(total);
        let items = value["workflow_runs"]
            .as_array()
            .context("missing page items")?;
        ensure!(items.len() <= 100, "oversized page");
        ensure!(self.listed + items.len() <= total, "incomplete pages");
        for run in items {
            let at = millis(&run["created_at"]).context("run created_at")?;
            if at < since || at >= until {
                continue;
            }
            let population = match run["event"].as_str() {
                Some("push") if run["head_branch"].as_str() == self.branch.as_deref() => {
                    "default_push"
                }
                Some("pull_request") => "pull_request",
                _ => continue,
            };
            let id = run["id"].as_u64().context("run id")?;
            ensure!(self.seen.insert(id), "duplicate run id in pages");
            let attempts = run["run_attempt"].as_u64().context("run_attempt")?;
            ensure!(
                (1..=100).contains(&attempts),
                "invalid or excessive attempt count"
            );
            let counts = self.populations.get_mut(population).context("population")?;
            counts.runs += 1;
            counts.expected_attempts += attempts as usize;
            // The listing itself observes the latest attempt, even if enrichment
            // later runs out of allocation. Never count it again during enrichment.
            counts.observe(run, attempts);
            self.runs.push((run.clone(), population, attempts));
        }
        self.listed += items.len();
        self.listing_complete = self.listed == total;
        ensure!(
            self.listing_complete || !items.is_empty(),
            "incomplete pages"
        );
        Ok(())
    }
}

/// `until` is exclusive UTC midnight; use an injected clock for bounded reads.
pub fn collect(api: &dyn Api, config: &Config, until: i64, clock: &dyn Fn() -> i64) -> Value {
    let limits = Limits {
        max_requests: REQUESTS,
        collection_timeout_seconds: SECONDS,
        ..Limits::default()
    };
    let started = clock();
    let bounded = Bounded::new(api, &limits, clock);
    // A busy target must not spend another target's API/decoded-byte share.
    // Unused shares are not redistributed: the allocation is independent of
    // config order and unavailable targets, not a promise of complete coverage.
    let count = config.targets.len();
    let aggregate_bytes = Cell::new(0);
    let readers: Vec<_> = (0..count)
        .map(|_| Reader {
            api: &bounded,
            bytes: Cell::new(0),
            aggregate_bytes: &aggregate_bytes,
            byte_limit: TOTAL_BYTES / count,
            deadline: Cell::new(i64::MAX),
            clock,
            operations: Cell::new(0),
            operation_limit: REQUESTS / count as u32,
        })
        .collect();
    let since = until - 30 * DAY;
    let mut targets = vec![];
    let mut groups = BTreeMap::new();
    let mut histories: Vec<_> = (0..count).map(|_| History::new()).collect();
    let mut order: Vec<_> = (0..count).collect();
    order.sort_by_key(|&i| (&config.targets[i].repository, &config.targets[i].workflow));
    for &i in &order {
        let result = readers[i]
            .get(&format!("/repos/{}", config.targets[i].repository))
            .and_then(|v| {
                v["default_branch"]
                    .as_str()
                    .map(str::to_owned)
                    .context("default branch unavailable")
            });
        match result {
            Ok(branch) => histories[i].branch = Some(branch),
            Err(error) => {
                histories[i].stopped = true;
                histories[i]
                    .warnings
                    .push(format!("history unavailable/incomplete: {error}"));
            }
        }
    }
    // One history page per target per round, before any attempt/job/log reads.
    for page in 1..=PAGES {
        for &i in &order {
            let history = &mut histories[i];
            if history.stopped || history.listing_complete {
                continue;
            }
            let target = &config.targets[i];
            let path = format!(
                "/repos/{}/actions/workflows/{}/runs?created={}..{}&per_page=100&page={page}",
                target.repository,
                target.workflow,
                &iso(since)[..10],
                &iso(until)[..10]
            );
            let result = readers[i]
                .get(&path)
                .and_then(|v| history.page(v, since, until));
            if let Err(error) = result {
                history.stopped = true;
                history
                    .warnings
                    .push(format!("history unavailable/incomplete: {error}"));
            } else if page == PAGES && !history.listing_complete {
                history.warnings.push("history unavailable/incomplete: pagination limit reached (at most 1000 records)".into());
            }
        }
    }
    // Each target also gets an equal share of the remaining wall-clock window.
    // The global deadline still applies; an in-flight request may overrun either.
    let enrichment_ms = (SECONDS as i64 * 1000 - (clock() - started)).max(0) / count as i64;
    for &i in &order {
        let target = &config.targets[i];
        let reader = &readers[i];
        reader.deadline.set(clock().saturating_add(enrichment_ms));
        let history = &mut histories[i];
        history.runs.sort_by_key(|(run, _, _)| run["id"].as_u64());
        let result = (|| -> Result<()> {
            let root = format!("/repos/{}", target.repository);
            for (run, population, attempts) in &history.runs {
                let id = run["id"].as_u64().context("run id")?;
                let counts = history
                    .populations
                    .get_mut(population)
                    .context("population")?;
                for attempt in 1..=*attempts {
                    if attempt == *attempts && !failed(run) {
                        continue;
                    }
                    reader.available()?;
                    let observed = if attempt == *attempts {
                        Ok(run.clone())
                    } else {
                        reader.get(&format!("{root}/actions/runs/{id}/attempts/{attempt}"))
                    };
                    let result = (|| -> Result<()> {
                        let observed = observed?;
                        ensure!(
                            observed["id"].as_u64() == Some(id)
                                && observed["run_attempt"].as_u64() == Some(attempt),
                            "attempt identity mismatch"
                        );
                        if attempt != *attempts {
                            counts.observe(&observed, attempt);
                        }
                        if failed(&observed) {
                            groups_for(
                                reader,
                                &root,
                                &target.repository,
                                &observed,
                                attempt,
                                &mut groups,
                            )?;
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        history
                            .warnings
                            .push(format!("run {id} attempt {attempt}: {error}"));
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            history
                .warnings
                .push(format!("enrichment unavailable/incomplete: {error}"));
        }
    }
    for (target, history) in config.targets.iter().zip(histories) {
        let mut warnings = history.warnings;
        if history.populations.values().all(|p| p.runs == 0) {
            warnings.push("no eligible history observed".into());
        }
        targets.push(json!({"target":target,"listing_complete":history.listing_complete,"complete":warnings.is_empty(),"warnings":warnings,"populations":history.populations}));
    }
    let groups: Vec<Value> = groups.into_values().map(|group| {
        let entry = config.index.get(&group.signature);
        let mut findings = vec![];
        if group.run_ids.len() >= 2 && entry.is_none_or(|e| e.issue.is_none()) { findings.push("recurring_without_issue"); }
        if let Some(entry) = entry {
            let first = millis(&json!(entry.first_seen)).unwrap_or(until);
            if entry.class == "flake" && entry.explained_at.is_none() && until - first > 7 * DAY {
                findings.push("flake_unexplained_over_seven_days");
            }
        }
        json!({"observation":group,"classification":entry,"class":entry.map_or("unknown", |e| e.class.as_str()),"findings":findings})
    }).collect();
    json!({"schema":"agent-board/ci-health/v1","since":iso(since),"until":iso(until),"complete":targets.iter().all(|t| t["complete"] == true),"targets":targets,"groups":groups,
        "bounds":{"api_operations":REQUESTS,"collection_seconds":SECONDS,"pages_per_listing":PAGES,"log_bytes":LOG_BYTES,"aggregate_decoded_bytes":TOTAL_BYTES},"decoded_bytes":aggregate_bytes.get()})
}

pub fn human(report: &Value) -> String {
    let mut out = format!(
        "CI health {}..{} complete={}\n",
        report["since"], report["until"], report["complete"]
    );
    for target in report["targets"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "{} {}\n",
            target["target"]["repository"], target["target"]["workflow"]
        ));
        for population in ["default_push", "pull_request"] {
            let p = &target["populations"][population];
            out.push_str(&format!("  {population}: {} runs, {} attempts; {}/{} passed; first failures={} cancelled={} skipped={} active={} other={}\n", p["runs"],p["attempts"],p["success"],p["denominator"],p["first_attempt_failed"],p["cancelled"],p["skipped"],p["in_progress"],p["other"]));
        }
        for warning in target["warnings"].as_array().into_iter().flatten() {
            out.push_str(&format!("  WARNING: {warning}\n"));
        }
    }
    out.push_str(&format!(
        "{} failure groups (classifications and findings in JSON); observed rates only\n",
        report["groups"].as_array().map_or(0, Vec::len)
    ));
    out
}

#[cfg(test)]
mod tests;
