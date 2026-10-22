//! `agent-board status`: how loaded an organization is, in one read-only
//! pass (agent-board#262). Runner load across the organization's
//! repositories, agent runs of the configured dispatch workflow, the pools
//! of the usage snapshot the poll paces by, the followed repositories'
//! merge queues, and the board.
//!
//! Every read goes through the supervisor's [`Bounded`](crate::supervise::Bounded)
//! API, which refuses writes and stops at a request budget and a deadline:
//! a large organization or a stalled API ends the pass instead of exhausting
//! the token's rate limit. A section that cannot be read whole says so
//! rather than showing a short count as the load.
use crate::poll::{PollPolicy, option, pace, validate_repository, validate_workflow};
use crate::supervise::health::{USAGE_MARKER, usage_snapshot};
use crate::supervise::{Api, encode, iso, millis, run_title};
use anyhow::{Context, Result, bail, ensure};
use board_core::{Priority, Snapshot, Status};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use time::OffsetDateTime;

/// The requests one pass may make: two run listings per repository and one
/// job listing per active run. 128 queued runs in a 30-repository
/// organization take about 200.
pub const REQUEST_LIMIT: u32 = 600;
/// How long one pass may take.
pub const TIMEOUT_SECONDS: u64 = 120;
/// The longest `--job-limit`: no GitHub plan runs more concurrent jobs.
pub const JOB_LIMIT_MAX: u64 = 10_000;
/// The report's sections, for `--json`.
pub const FIELDS: &[&str] = &["runners", "agents", "pools", "queues", "board"];
const PAGE: usize = 100;
/// Pages of the organization's repositories: at most 500 repositories.
const REPOSITORY_PAGES: usize = 5;
/// Pages of one repository's runs of one status, or one run's jobs.
const RUN_PAGES: usize = 3;
/// The window of agent run outcomes.
const OUTCOME_SECONDS: i64 = 3600;
/// Completed runs are listed by creation, the only time the API filters by,
/// so runs created this long ago are read for those that ended in the window.
const OUTCOME_CREATED_SECONDS: i64 = 6 * 3600;
/// Pages of the usage issue's comments searched for the snapshot.
const USAGE_PAGES: usize = 3;
/// Merge-queue entries and open pull requests read per followed repository.
const QUEUE_READS: usize = 100;
const ACTIVE: &[&str] = &["queued", "in_progress"];
const SELF_HOSTED: &str = "self-hosted";

/// What the pass reads besides `owner`, from the trusted agent-board.json.
#[derive(Debug)]
pub struct Settings {
    pub owner: String,
    /// Agent runs are this repository's runs of `dispatch_workflow`.
    pub dispatch_repo: String,
    pub dispatch_workflow: String,
    /// Whose merge queues and approved pull requests are shown.
    pub follow: Vec<String>,
    pub pace: Option<pace::Pace>,
    /// The plan's concurrent hosted-job limit, if given.
    pub job_limit: Option<u64>,
}

impl Settings {
    pub fn from_json(owner: &str, config: &Value, job_limit: Option<u64>) -> Result<Self> {
        ensure!(crate::is_github_login(owner), "invalid --owner {owner:?}");
        let policy = PollPolicy::from_json(config)?;
        let dispatch_repo = config["dispatchRepo"]
            .as_str()
            .context("agent-board config: dispatchRepo is required")?;
        validate_repository(dispatch_repo).context("dispatchRepo")?;
        let dispatch_workflow = option(config, "dispatchWorkflow")?
            .and_then(Value::as_str)
            .context("agent-board config: status needs dispatchWorkflow")?;
        validate_workflow(dispatch_workflow)?;
        let follow: Vec<String> = serde_json::from_value(config["follow"].clone())
            .context("agent-board config: follow is not a list of repositories")?;
        for repo in &follow {
            validate_repository(repo).context("follow")?;
        }
        if let Some(limit) = job_limit {
            ensure!(
                (1..=JOB_LIMIT_MAX).contains(&limit),
                "--job-limit is not 1 to {JOB_LIMIT_MAX}"
            );
        }
        Ok(Self {
            owner: owner.into(),
            dispatch_repo: dispatch_repo.into(),
            dispatch_workflow: dispatch_workflow.into(),
            follow,
            pace: policy.pace,
            job_limit,
        })
    }
}

/// The report: each section, or `{"unavailable": why}`. The board section
/// is there only where the caller read a board.
pub fn collect(
    api: &dyn Api,
    s: &Settings,
    now: OffsetDateTime,
    board: Option<Result<Snapshot>>,
) -> Value {
    let section =
        |read: Result<Value>| read.unwrap_or_else(|e| json!({"unavailable": format!("{e:#}")}));
    let now_ms = crate::supervise::now_millis(now);
    // The cheap sections first, so that the runner census, which grows with
    // the load, cannot spend their budget.
    let mut report = json!({});
    // Pools are paced only where the config names a usage snapshot.
    if s.pace.is_some() {
        report["pools"] = section(pools(api, s, now));
    }
    report["queues"] = section(queues(api, s));
    report["agents"] = section(agents(api, s, now_ms));
    report["runners"] = section(runners(api, s, now_ms));
    if let Some(snapshot) = board {
        report["board"] = section(snapshot.map(|s| board_counts(&s)));
    }
    report
}

/// Whether every section was read.
pub fn complete(report: &Value) -> bool {
    report
        .as_object()
        .is_some_and(|o| o.values().all(|v| v.get("unavailable").is_none()))
}

/// Every entry of a paged REST list, `key`'s array or the body itself, in
/// pages of [`PAGE`] up to `pages`; a list longer than that is an error, as
/// a count of part of it would understate the load.
fn listed(api: &dyn Api, path: &str, key: Option<&str>, pages: usize) -> Result<Vec<Value>> {
    let sep = if path.contains('?') { '&' } else { '?' };
    let mut all = vec![];
    for page in 1..=pages {
        let body = api.get(&format!("{path}{sep}per_page={PAGE}&page={page}"))?;
        let items = match key {
            Some(key) => &body[key],
            None => &body,
        }
        .as_array()
        .with_context(|| format!("{path}: not a list"))?;
        all.extend(items.iter().cloned());
        // `total_count` may grow between pages as runs start; a short page
        // ends the list either way.
        let total = key.and_then(|_| body["total_count"].as_u64());
        if items.len() < PAGE || total.is_some_and(|t| all.len() as u64 >= t) {
            return Ok(all);
        }
    }
    bail!("{path}: more than {} entries", pages * PAGE)
}

fn seconds_since(at: &Value, now_ms: i64) -> Option<i64> {
    millis(at).map(|at| (now_ms - at).max(0) / 1000)
}

/// Whether `run` is one of the configured dispatch workflow's.
fn agent_run(s: &Settings, repo: &str, run: &Value) -> bool {
    let workflow = format!(".github/workflows/{}", s.dispatch_workflow);
    repo.eq_ignore_ascii_case(&s.dispatch_repo)
        && run["path"]
            .as_str()
            .is_some_and(|p| p.split('@').next() == Some(workflow.as_str()))
}

/// Jobs running and queued across the organization's repositories by runner
/// label, agent runs apart from other CI, and the oldest queued job of each.
fn runners(api: &dyn Api, s: &Settings, now_ms: i64) -> Result<Value> {
    let mut repos: Vec<String> = listed(
        api,
        &format!("/orgs/{}/repos?type=all&sort=full_name", s.owner),
        None,
        REPOSITORY_PAGES,
    )?
    .iter()
    .filter(|r| r["archived"] != true && r["disabled"] != true)
    .filter_map(|r| r["full_name"].as_str().map(str::to_owned))
    .collect();
    if !repos
        .iter()
        .any(|r| r.eq_ignore_ascii_case(&s.dispatch_repo))
    {
        repos.push(s.dispatch_repo.clone());
    }
    // label -> class -> status -> jobs
    let mut labels: BTreeMap<String, BTreeMap<&str, BTreeMap<String, u64>>> = BTreeMap::new();
    let mut oldest: BTreeMap<&str, (i64, Value)> = BTreeMap::new();
    for repo in &repos {
        // A run that starts between the two listings is in both.
        let mut seen = BTreeSet::new();
        for status in ACTIVE {
            let runs = listed(
                api,
                &format!("/repos/{repo}/actions/runs?status={status}"),
                Some("workflow_runs"),
                RUN_PAGES,
            )?;
            for run in runs {
                let class = if agent_run(s, repo, &run) {
                    "agent"
                } else {
                    "other"
                };
                let id = run["id"].as_u64().context("run without id")?;
                if !seen.insert(id) {
                    continue;
                }
                let jobs = listed(
                    api,
                    &format!("/repos/{repo}/actions/runs/{id}/jobs"),
                    Some("jobs"),
                    RUN_PAGES,
                )?;
                for job in jobs {
                    let state = job["status"].as_str().unwrap_or_default();
                    if !ACTIVE.contains(&state) {
                        continue;
                    }
                    let names: Vec<&str> = job["labels"]
                        .as_array()
                        .map(|l| l.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    // A self-hosted job's other labels pick among the
                    // organization's own machines, not a hosted pool.
                    let label = if names.contains(&SELF_HOSTED) || names.is_empty() {
                        SELF_HOSTED.to_owned()
                    } else {
                        names.join(",")
                    };
                    *labels
                        .entry(label)
                        .or_default()
                        .entry(class)
                        .or_default()
                        .entry(state.to_owned())
                        .or_default() += 1;
                    if state == "queued" {
                        let wait = seconds_since(&job["created_at"], now_ms).unwrap_or(0);
                        if oldest.get(class).is_none_or(|(w, _)| wait > *w) {
                            oldest.insert(class, (wait, job["html_url"].clone()));
                        }
                    }
                }
            }
        }
    }
    let hosted_in_progress: u64 = labels
        .iter()
        .filter(|(label, _)| label.as_str() != SELF_HOSTED)
        .flat_map(|(_, classes)| classes.values())
        .map(|states| states.get("in_progress").copied().unwrap_or(0))
        .sum();
    Ok(json!({
        "repositories": repos.len(),
        "job_limit": s.job_limit,
        "hosted_in_progress": hosted_in_progress,
        "labels": labels,
        "oldest_queued": oldest
            .into_iter()
            .map(|(class, (wait, url))| (class, json!({"wait_seconds": wait, "html_url": url})))
            .collect::<BTreeMap<_, _>>(),
    }))
}

/// `kind`, and the pool of a ` (POOL)` suffix, of a dispatch run's title.
fn kind_and_pool(title: &str) -> (String, String) {
    let kind = run_title(title).map_or("other", |(kind, _, _)| kind);
    let pool = title
        .strip_suffix(')')
        .and_then(|t| t.rsplit_once(" ("))
        .map(|(_, pool)| pool)
        .filter(|p| !p.is_empty() && !p.contains(|c: char| c.is_whitespace() || "()".contains(c)))
        .unwrap_or("default");
    (kind.into(), pool.into())
}

/// Agent runs active by kind and by pool, and the last hour's outcomes.
fn agents(api: &dyn Api, s: &Settings, now_ms: i64) -> Result<Value> {
    let runs_path = format!(
        "/repos/{}/actions/workflows/{}/runs",
        s.dispatch_repo, s.dispatch_workflow
    );
    let mut by_kind: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut by_pool: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for status in ACTIVE {
        for run in listed(
            api,
            &format!("{runs_path}?status={status}"),
            Some("workflow_runs"),
            RUN_PAGES,
        )? {
            if !seen.insert(run["id"].as_u64().context("run without id")?) {
                continue;
            }
            let (kind, pool) = kind_and_pool(run["display_title"].as_str().unwrap_or_default());
            *by_kind
                .entry(kind)
                .or_default()
                .entry(status.to_string())
                .or_default() += 1;
            *by_pool
                .entry(pool)
                .or_default()
                .entry(status.to_string())
                .or_default() += 1;
        }
    }
    let since = encode(&format!(
        ">={}",
        iso(now_ms - OUTCOME_CREATED_SECONDS * 1000)
    ));
    let mut outcomes: BTreeMap<&str, u64> = BTreeMap::new();
    let mut failed = vec![];
    for run in listed(
        api,
        &format!("{runs_path}?status=completed&created={since}"),
        Some("workflow_runs"),
        RUN_PAGES,
    )? {
        // A completed run's last update is its end.
        if millis(&run["updated_at"]).is_none_or(|at| at < now_ms - OUTCOME_SECONDS * 1000) {
            continue;
        }
        let outcome = match run["conclusion"].as_str().unwrap_or_default() {
            "success" => "succeeded",
            "failure" | "timed_out" | "startup_failure" => "failed",
            "cancelled" => "cancelled",
            _ => "other",
        };
        *outcomes.entry(outcome).or_default() += 1;
        if outcome == "failed" {
            failed.push(json!({"title": run["display_title"], "html_url": run["html_url"]}));
        }
    }
    Ok(json!({
        "by_kind": by_kind,
        "by_pool": by_pool,
        "last_hour": outcomes,
        "failed": failed,
    }))
}

/// Each pool of the usage snapshot, as the poll paces it.
fn pools(api: &dyn Api, s: &Settings, now: OffsetDateTime) -> Result<Value> {
    let pace = s.pace.as_ref().context("agent-board config has no pace")?;
    let path = format!("/repos/{}/issues/{}/comments", pace.repository, pace.issue);
    // The bot's first marked comment, as the poll takes it, so that an
    // unreadable one shows as no snapshot here as it holds runs there.
    let mut found = None;
    for page in 1..=USAGE_PAGES {
        let comments = api.get(&format!("{path}?per_page={PAGE}&page={page}"))?;
        let comments = comments.as_array().context("usage comments: not a list")?;
        // Only the configured bot's comment is the snapshot: anyone may
        // comment on the issue.
        found = comments
            .iter()
            .filter(|c| c["user"]["login"] == pace.bot.as_str())
            .filter_map(|c| c["body"].as_str())
            .find(|b| b.starts_with(USAGE_MARKER))
            .map(usage_snapshot);
        if found.is_some() || comments.len() < PAGE {
            break;
        }
        // Past the pages read, the comment may yet exist: unknown, not absent.
        ensure!(
            page < USAGE_PAGES,
            "usage comment not in the first {USAGE_PAGES} pages"
        );
    }
    let snapshot = found.flatten();
    let paces = pace::paces(snapshot.as_ref(), now, pace.margin_percent, &pace.overrides);
    let entries: BTreeMap<String, Value> = snapshot
        .as_ref()
        .and_then(|s| s["pools"].as_object())
        .map(|pools| {
            pools
                .iter()
                .map(|(name, p)| {
                    let field = |k: &str| p.get(k).cloned().unwrap_or(Value::Null);
                    (
                        name.clone(),
                        json!({
                            "used_percent": field("used_percent"),
                            "allowed_percent": field("allowed_percent"),
                            "hold": field("hold"),
                            "next_dispatch_at": field("next_dispatch_at"),
                        }),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "updated_at": snapshot.as_ref().map_or(Value::Null, |s| s["updated_at"].clone()),
        "pools": entries,
        // The poll's own line, so that both say the same of each pool.
        "line": pace::line(&paces, &BTreeSet::new(), &pace.overrides, now),
    }))
}

const QUEUES_QUERY: &str = r#"query($owner: String!, $name: String!, $reads: Int!) {
  repository(owner: $owner, name: $name) {
    mergeQueue { entries(first: $reads) { totalCount nodes { position state pullRequest { number url } } } }
    pullRequests(states: OPEN, first: $reads, orderBy: {field: UPDATED_AT, direction: DESC}) {
      totalCount
      nodes {
        number url isDraft reviewDecision mergeable mergeStateStatus isInMergeQueue
        commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
      }
    }
  }
}"#;

/// Why an approved pull request is not in its merge queue, as far as its
/// state tells.
fn not_queued(pull: &Value) -> &'static str {
    let rollup = pull["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["state"].as_str();
    match (
        pull["mergeable"].as_str(),
        pull["mergeStateStatus"].as_str(),
        rollup,
    ) {
        (Some("CONFLICTING"), _, _) | (_, Some("DIRTY"), _) => "conflicting",
        (_, _, Some("FAILURE" | "ERROR")) => "failing",
        (_, Some("BEHIND"), _) => "behind",
        (_, _, Some("PENDING" | "EXPECTED")) => "pending",
        (_, Some("BLOCKED"), _) => "blocked",
        _ => "not queued",
    }
}

/// Each followed repository's merge queue, and its approved pull requests
/// that are not in it.
fn queues(api: &dyn Api, s: &Settings) -> Result<Value> {
    let mut out = BTreeMap::new();
    for repo in &s.follow {
        let (owner, name) = repo.split_once('/').context("invalid repository")?;
        let data = api.graphql(
            QUEUES_QUERY,
            json!({"owner": owner, "name": name, "reads": QUEUE_READS}),
        )?;
        let repository = &data["repository"];
        ensure!(repository.is_object(), "{repo}: repository not readable");
        let entries = &repository["mergeQueue"]["entries"];
        let pulls = &repository["pullRequests"];
        let read = |v: &Value| v["nodes"].as_array().map_or(0, Vec::len) as u64;
        let approved: Vec<Value> = pulls["nodes"]
            .as_array()
            .map(|p| {
                p.iter()
                    .filter(|p| {
                        p["reviewDecision"] == "APPROVED"
                            && p["isInMergeQueue"] != true
                            && p["isDraft"] != true
                    })
                    .map(|p| json!({"number": p["number"], "html_url": p["url"], "reason": not_queued(p)}))
                    .collect()
            })
            .unwrap_or_default();
        out.insert(
            repo.clone(),
            json!({
                // A repository without a merge queue has none to count.
                "queued": entries["totalCount"].as_u64().unwrap_or(0),
                "approved_not_queued": approved,
                // Only the newest pull requests are read; the rest are not judged.
                "complete": entries["totalCount"].as_u64().unwrap_or(0) <= read(entries)
                    && pulls["totalCount"].as_u64().is_some_and(|t| t <= read(pulls)),
            }),
        );
    }
    Ok(json!(out))
}

/// Whether a board item has a run or a pull request: a Run or Result text,
/// or a linked pull request.
fn has_run_or_pull(item: &board_core::Item) -> bool {
    let text = |name: &str| {
        item.fields
            .get(name)
            .and_then(|v| v["text"].as_str())
            .is_some_and(|t| !t.trim().is_empty())
    };
    text("Run")
        || text("Result")
        || item
            .fields
            .values()
            .any(|v| v["pullRequests"].as_array().is_some_and(|p| !p.is_empty()))
}

/// Items by Status, and open P0 items with no run or pull request.
pub fn board_counts(snapshot: &Snapshot) -> Value {
    let mut by_status: BTreeMap<String, u64> = BTreeMap::new();
    let mut unstarted = vec![];
    for item in &snapshot.items {
        let status = item
            .status
            .as_ref()
            .and_then(|s| serde_json::to_value(s).ok())
            .and_then(|s| s.as_str().map(str::to_owned))
            .unwrap_or_else(|| "No status".into());
        *by_status.entry(status).or_default() += 1;
        if item.priority == Some(Priority::P0)
            && item.state == "open"
            && !matches!(item.status, Some(Status::Done | Status::Cancelled))
            && !has_run_or_pull(item)
        {
            unstarted.push(format!(
                "{}#{}",
                item.identity.repository, item.identity.number
            ));
        }
    }
    unstarted.sort();
    json!({
        "by_status": by_status,
        "p0_without_run": unstarted,
        "complete": snapshot.coverage.complete,
    })
}

/// Remote text without control characters, which could move a terminal's cursor.
fn clean(value: &Value) -> String {
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    text.chars().filter(|c| !c.is_control()).collect()
}

fn duration(seconds: i64) -> String {
    match seconds {
        ..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        _ => format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

fn counts(states: &Value) -> String {
    let n = |k: &str| states[k].as_u64().unwrap_or(0);
    format!("{} running, {} queued", n("in_progress"), n("queued"))
}

/// The report for a terminal, one line or so per fact.
pub fn text(report: &Value) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    let unavailable = |v: &Value| v.get("unavailable").map(clean);

    let r = &report["runners"];
    if let Some(why) = unavailable(r) {
        line(format!("runners: unavailable: {why}"));
    } else {
        let limit = r["job_limit"]
            .as_u64()
            .map_or(String::new(), |l| format!("/{l}"));
        line(format!(
            "runners: {} repositories, hosted jobs running {}{limit}",
            r["repositories"], r["hosted_in_progress"]
        ));
        for (label, classes) in r["labels"].as_object().into_iter().flatten() {
            line(format!(
                "  {}: agent {}; other {}",
                clean(&label.as_str().into()),
                counts(&classes["agent"]),
                counts(&classes["other"])
            ));
        }
        for (class, job) in r["oldest_queued"].as_object().into_iter().flatten() {
            line(format!(
                "  oldest queued {class} job: {} {}",
                duration(job["wait_seconds"].as_i64().unwrap_or(0)),
                clean(&job["html_url"])
            ));
        }
    }

    let a = &report["agents"];
    if let Some(why) = unavailable(a) {
        line(format!("agents: unavailable: {why}"));
    } else {
        line("agents:".into());
        for (key, name) in [("by_kind", "kind"), ("by_pool", "pool")] {
            for (k, states) in a[key].as_object().into_iter().flatten() {
                line(format!(
                    "  {name} {}: {}",
                    clean(&k.as_str().into()),
                    counts(states)
                ));
            }
        }
        let n = |k: &str| a["last_hour"][k].as_u64().unwrap_or(0);
        line(format!(
            "  last hour: {} succeeded, {} failed, {} cancelled",
            n("succeeded"),
            n("failed"),
            n("cancelled")
        ));
        for run in a["failed"].as_array().into_iter().flatten() {
            line(format!(
                "    failed: {} {}",
                clean(&run["title"]),
                clean(&run["html_url"])
            ));
        }
    }

    let p = &report["pools"];
    if let Some(why) = unavailable(p) {
        line(format!("pools: unavailable: {why}"));
    } else if p.is_object() {
        line(clean(&p["line"]));
    }

    let q = &report["queues"];
    if let Some(why) = unavailable(q) {
        line(format!("queues: unavailable: {why}"));
    } else {
        line("queues:".into());
        for (repo, queue) in q.as_object().into_iter().flatten() {
            let approved: Vec<String> = queue["approved_not_queued"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| format!("#{} {}", p["number"], clean(&p["reason"])))
                .collect();
            let approved = if approved.is_empty() {
                String::new()
            } else {
                format!("; approved, not queued: {}", approved.join(", "))
            };
            let partial = if queue["complete"] == true {
                ""
            } else {
                " (partial)"
            };
            line(format!(
                "  {repo}: {} queued{approved}{partial}",
                queue["queued"]
            ));
        }
    }

    let b = &report["board"];
    if let Some(why) = unavailable(b) {
        line(format!("board: unavailable: {why}"));
    } else if b.is_object() {
        let statuses: Vec<String> = b["by_status"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(s, n)| format!("{} {n}", clean(&s.as_str().into())))
            .collect();
        let partial = if b["complete"] == true {
            ""
        } else {
            " (partial)"
        };
        line(format!("board{partial}: {}", statuses.join(", ")));
        let unstarted: Vec<String> = b["p0_without_run"]
            .as_array()
            .into_iter()
            .flatten()
            .map(clean)
            .collect();
        if !unstarted.is_empty() {
            line(format!("  P0 without run or PR: {}", unstarted.join(", ")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::{Bounded, Limits};
    use std::cell::RefCell;
    use time::format_description::well_known::Rfc3339;

    const NOW: &str = "2026-10-22T12:00:00Z";

    /// Responses by API path, as fixtures/status/api.json records them;
    /// GraphQL by `graphql OWNER/NAME`.
    struct Fake {
        responses: serde_json::Map<String, Value>,
        paths: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new() -> Self {
            let text = include_str!("../../../fixtures/status/api.json");
            Self {
                responses: serde_json::from_str(text).unwrap(),
                paths: RefCell::default(),
            }
        }

        fn without(mut self, path: &str) -> Self {
            assert!(self.responses.remove(path).is_some(), "{path}");
            self
        }

        fn with(mut self, path: &str, edit: impl FnOnce(&mut Value)) -> Self {
            edit(self.responses.get_mut(path).unwrap());
            self
        }
    }

    impl Api for Fake {
        fn get(&self, path: &str) -> Result<Value> {
            self.paths.borrow_mut().push(path.into());
            self.responses
                .get(path)
                .cloned()
                .with_context(|| format!("GET {path}: GitHub HTTP status 404"))
        }

        fn graphql(&self, _: &str, variables: Value) -> Result<Value> {
            let key = format!(
                "graphql {}/{}",
                variables["owner"].as_str().unwrap(),
                variables["name"].as_str().unwrap()
            );
            self.paths.borrow_mut().push(key.clone());
            self.responses
                .get(&key)
                .cloned()
                .context("no such repository")
        }

        fn download(&self, _: &str, _: u64) -> Result<Vec<u8>> {
            bail!("status downloads nothing")
        }

        fn send(&self, method: &str, path: &str, _: &Value) -> Result<Value> {
            panic!("status must not write ({method} {path})")
        }
    }

    fn config() -> Value {
        json!({"operator": "owner", "bot": "worker", "dispatchRepo": "example/control",
            "dispatchWorkflow": "dispatch.yml", "follow": ["example/code"],
            "pace": {"repository": "example/ops", "issue": 7, "bot": "usage-bot"}})
    }

    fn settings() -> Settings {
        Settings::from_json("example", &config(), Some(60)).unwrap()
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::parse(NOW, &Rfc3339).unwrap()
    }

    fn bounded<'a>(api: &'a Fake, requests: u32, clock: &'a dyn Fn() -> i64) -> Bounded<'a> {
        let limits = Limits {
            max_requests: requests,
            ..Limits::default()
        };
        Bounded::new(api, &limits, clock)
    }

    fn board() -> Snapshot {
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        let template = snapshot.items[0].clone();
        let item = |number: u64, status: Option<Status>, state: &str, fields: Value| {
            let mut item = template.clone();
            item.identity.number = number;
            item.priority = Some(Priority::P0);
            item.status = status;
            item.state = state.into();
            item.fields = serde_json::from_value(fields).unwrap();
            item
        };
        snapshot.items.extend([
            item(10, Some(Status::Todo), "open", json!({})),
            item(
                11,
                Some(Status::Todo),
                "open",
                json!({"Run": {"text": "https://github.com/example/control/actions/runs/1"}}),
            ),
            item(
                12,
                Some(Status::InProgress),
                "open",
                json!({"Linked pull requests": {"pullRequests": [{"number": 8}]}}),
            ),
            item(13, Some(Status::Done), "closed", json!({})),
            item(
                14,
                None,
                "open",
                json!({"Run": {"text": " "}, "Result": {"text": ""}}),
            ),
        ]);
        snapshot
    }

    #[test]
    fn report_from_recorded_responses() {
        let fake = Fake::new();
        let clock = || crate::supervise::now_millis(now());
        let report = collect(
            &bounded(&fake, REQUEST_LIMIT, &clock),
            &settings(),
            now(),
            Some(Ok(board())),
        );
        assert!(complete(&report), "{report:#}");
        let r = &report["runners"];
        // The archived repository is not read.
        assert_eq!(r["repositories"], 2);
        assert!(
            !fake
                .paths
                .borrow()
                .iter()
                .any(|p| p.contains("example/old"))
        );
        assert_eq!(r["hosted_in_progress"], 2);
        assert_eq!(
            r["labels"],
            json!({
                "ubuntu-26.04": {"agent": {"queued": 2, "in_progress": 1}, "other": {"queued": 1, "in_progress": 1}},
                "self-hosted": {"other": {"in_progress": 1}},
            })
        );
        assert_eq!(r["oldest_queued"]["agent"]["wait_seconds"], 14 * 60);
        assert_eq!(r["oldest_queued"]["other"]["wait_seconds"], 3 * 60);
        let a = &report["agents"];
        assert_eq!(
            a["by_kind"],
            json!({"triage": {"queued": 2}, "implement": {"in_progress": 1}})
        );
        assert_eq!(
            a["by_pool"],
            json!({"claude": {"queued": 2}, "openai": {"in_progress": 1}})
        );
        // Run 34 failed before the hour.
        assert_eq!(
            a["last_hour"],
            json!({"succeeded": 1, "failed": 1, "cancelled": 1})
        );
        assert_eq!(
            a["failed"][0]["html_url"],
            "https://github.com/example/control/actions/runs/32"
        );
        // Only the configured bot's comment is the snapshot.
        assert_eq!(report["pools"]["pools"]["claude"]["used_percent"], 26);
        assert_eq!(
            report["board"]["p0_without_run"],
            json!(["example/intake#10", "example/intake#14"])
        );
        assert_eq!(
            text(&report),
            "\
runners: 2 repositories, hosted jobs running 2/60
  self-hosted: agent 0 running, 0 queued; other 1 running, 0 queued
  ubuntu-26.04: agent 1 running, 2 queued; other 1 running, 1 queued
  oldest queued agent job: 14m https://github.com/example/control/actions/runs/21/job/211
  oldest queued other job: 3m https://github.com/example/code/actions/runs/11/job/111
agents:
  kind implement: 1 running, 0 queued
  kind triage: 0 running, 2 queued
  pool claude: 0 running, 2 queued
  pool openai: 1 running, 0 queued
  last hour: 1 succeeded, 1 failed, 1 cancelled
    failed: implement example/code#4 (claude) https://github.com/example/control/actions/runs/32
pools: claude normal 26%/30%, openai held until 2026-10-23T00:00Z
queues:
  example/code: 2 queued; approved, not queued: #42 conflicting, #43 failing, #44 behind
board: Done 1, In Progress 1, No status 2, Todo 2, Triage 1
  P0 without run or PR: example/intake#10, example/intake#14
"
        );
    }

    #[test]
    fn unreadable_sections_say_so() {
        let clock = || crate::supervise::now_millis(now());
        for (fake, requests, settings, section, why) in [
            // A run's jobs that cannot be read leave no partial count.
            (
                Fake::new()
                    .without("/repos/example/control/actions/runs/24/jobs?per_page=100&page=1"),
                REQUEST_LIMIT,
                settings(),
                "runners",
                "404",
            ),
            // The census, read last, spends the budget the others left.
            (
                Fake::new(),
                12,
                settings(),
                "runners",
                "collection budget exhausted",
            ),
            (
                Fake::new().without("graphql example/code"),
                REQUEST_LIMIT,
                settings(),
                "queues",
                "no such repository",
            ),
            (
                Fake::new().without("/repos/example/ops/issues/7/comments?per_page=100&page=1"),
                REQUEST_LIMIT,
                settings(),
                "pools",
                "404",
            ),
        ] {
            let report = collect(&bounded(&fake, requests, &clock), &settings, now(), None);
            assert!(!complete(&report));
            let found = report[section]["unavailable"].as_str().unwrap_or_default();
            assert!(found.contains(why), "{section}: {report:#}");
            for other in ["runners", "agents", "pools", "queues"]
                .iter()
                .filter(|s| **s != section)
            {
                assert!(
                    report[other]["unavailable"].is_null(),
                    "{other}: {report:#}"
                );
            }
            assert!(text(&report).contains(&format!("{section}: unavailable: ")));
        }
        // A board that could not be read is a section too.
        let fake = Fake::new();
        let report = collect(
            &bounded(&fake, REQUEST_LIMIT, &clock),
            &settings(),
            now(),
            Some(Err(anyhow::anyhow!("no project"))),
        );
        assert_eq!(report["board"]["unavailable"], "no project");
        assert!(!complete(&report));
    }

    #[test]
    fn runs_and_snapshots_counted_once_as_the_poll_reads_them() {
        let clock = || crate::supervise::now_millis(now());
        // Run 23 started between the listings, so it is in both.
        let fake = Fake::new()
            .with(
                "/repos/example/control/actions/runs?status=queued&per_page=100&page=1",
                |v| v["workflow_runs"].as_array_mut().unwrap().push(json!({"id": 23, "path": ".github/workflows/dispatch.yml"})),
            )
            .with(
                "/repos/example/control/actions/workflows/dispatch.yml/runs?status=queued&per_page=100&page=1",
                |v| v["workflow_runs"].as_array_mut().unwrap().push(json!({"id": 23, "display_title": "implement example/code#3 (openai)"})),
            )
            // The bot's first marked comment does not parse: the poll holds
            // every run on it, so it is no snapshot here either.
            .with("/repos/example/ops/issues/7/comments?per_page=100&page=1", |v| {
                v.as_array_mut().unwrap().insert(
                    0,
                    json!({"user": {"login": "usage-bot"}, "body": "<!-- bot-usage v1 -->\nbroken"}),
                )
            });
        let report = collect(
            &bounded(&fake, REQUEST_LIMIT, &clock),
            &settings(),
            now(),
            None,
        );
        assert_eq!(
            report["runners"]["labels"]["ubuntu-26.04"]["agent"],
            json!({"queued": 2, "in_progress": 1})
        );
        // Once, as the listing that saw it first; its jobs above tell it running.
        assert_eq!(
            report["agents"]["by_kind"]["implement"],
            json!({"queued": 1})
        );
        assert_eq!(
            report["pools"]["line"],
            "pools: there is no usage snapshot, so new runs wait"
        );
        // Without `pace` there are no pools to show, and nothing is missing.
        let fake = Fake::new();
        let unpaced = Settings {
            pace: None,
            ..settings()
        };
        let report = collect(
            &bounded(&fake, REQUEST_LIMIT, &clock),
            &unpaced,
            now(),
            None,
        );
        assert!(complete(&report) && report.get("pools").is_none());
        assert!(!text(&report).contains("pools"));
    }

    #[test]
    fn stale_usage_holds_and_lists_end_within_bounds() {
        let clock = || crate::supervise::now_millis(now());
        let later = now() + time::Duration::hours(1);
        let fake = Fake::new();
        let report = pools(&bounded(&fake, REQUEST_LIMIT, &clock), &settings(), later).unwrap();
        assert_eq!(
            report["line"],
            "pools: the usage snapshot is not fresh, so new runs wait"
        );

        // A list that fills every page it may read is refused, not counted short.
        struct Full;
        impl Api for Full {
            fn get(&self, _: &str) -> Result<Value> {
                Ok(json!({"total_count": 1000, "workflow_runs": vec![json!({}); PAGE]}))
            }
            fn graphql(&self, _: &str, _: Value) -> Result<Value> {
                unreachable!()
            }
            fn download(&self, _: &str, _: u64) -> Result<Vec<u8>> {
                unreachable!()
            }
            fn send(&self, _: &str, _: &str, _: &Value) -> Result<Value> {
                unreachable!()
            }
        }
        let error = listed(&Full, "/runs", Some("workflow_runs"), RUN_PAGES).unwrap_err();
        assert_eq!(error.to_string(), "/runs: more than 300 entries");
    }

    #[test]
    fn settings_are_checked() {
        let with = |key: &str, value: Value| {
            let mut c = config();
            c[key] = value;
            c
        };
        let mut without = config();
        without.as_object_mut().unwrap().remove("dispatchWorkflow");
        for (owner, config, limit, why) in [
            ("bad owner!", config(), None, "invalid --owner"),
            ("example", without, None, "needs dispatchWorkflow"),
            (
                "example",
                with("follow", json!(["no-slash"])),
                None,
                "follow",
            ),
            (
                "example",
                with("dispatchRepo", json!("x")),
                None,
                "dispatchRepo",
            ),
            (
                "example",
                with("unknownKey", json!(1)),
                None,
                "unknown agent-board config key",
            ),
            ("example", config(), Some(0), "--job-limit"),
            ("example", config(), Some(JOB_LIMIT_MAX + 1), "--job-limit"),
        ] {
            let error = Settings::from_json(owner, &config, limit).unwrap_err();
            assert!(format!("{error:#}").contains(why), "{error:#}");
        }
    }

    #[test]
    fn kinds_pools_and_reasons() {
        for (title, kind, pool) in [
            ("triage o/r#5 (claude)", "triage", "claude"),
            ("implement o/r#5", "implement", "default"),
            ("review o/r#5 (a b)", "review", "default"),
            ("deploy o/r#5 (claude)", "other", "claude"),
        ] {
            assert_eq!(kind_and_pool(title), (kind.into(), pool.into()), "{title}");
        }
        let pull = |mergeable: &str, state: &str, rollup: &str| {
            json!({"mergeable": mergeable, "mergeStateStatus": state,
                "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": rollup}}}]}})
        };
        for (p, reason) in [
            (pull("CONFLICTING", "DIRTY", "FAILURE"), "conflicting"),
            (pull("MERGEABLE", "BEHIND", "FAILURE"), "failing"),
            (pull("MERGEABLE", "BEHIND", "SUCCESS"), "behind"),
            (pull("MERGEABLE", "BLOCKED", "PENDING"), "pending"),
            (pull("MERGEABLE", "BLOCKED", "SUCCESS"), "blocked"),
            (pull("MERGEABLE", "CLEAN", "SUCCESS"), "not queued"),
        ] {
            assert_eq!(not_queued(&p), reason, "{p}");
        }
    }

    #[test]
    fn remote_text_cannot_drive_the_terminal() {
        let report = json!({"agents": {"failed": [{"title": "fix o/r#1\u{1b}[2J\u{7}", "html_url": "u\nforged"}]}});
        let out = text(&report);
        assert!(out.contains("failed: fix o/r#1[2J uforged"), "{out}");
        assert!(!out.contains('\u{1b}'));
    }
}
