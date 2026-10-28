//! The agent's requests handed back as its run's safe outputs, an artifact of
//! the supervisor's own run, rather than posted as a comment on the requests
//! issue (agent-board#398). With them, the day's writes are counted from a
//! ledger each executor run hands to the next, not from the log issue, and the
//! log issue holds one status comment replaced on each run instead of an entry
//! per run.
//!
//! The ledger is an artifact too, and an artifact can be lost: a run killed
//! before its upload. So each run also reads its own workflow's other runs
//! since the ledger's. One whose execute job ran may have written what the
//! ledger does not count, and no write starts until the window it could have
//! written in has passed, which the ledger then carries forward. A lost
//! ledger costs a day and a half of writes, never the caps.
use super::{DAY_MS, Outcome, Policy, RUN_PAGES, Run, judgment::Checked, log_body};
use crate::supervise::{Api, HOUR_MS, encode, iso, millis};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

/// The safe output that holds the agent's report and its requests block.
const REPORT_TYPE: &str = "add_comment";
/// agentic-job's bound on a run's safe outputs.
pub const MAX_OUTPUTS_BYTES: usize = 1024 * 1024;
/// Every entry at its longest, and room to spare.
pub const MAX_LEDGER_BYTES: usize = 2 * 1024 * 1024;
const LEDGER_SCHEMA: &str = "agent-board/supervisor-ledger/v1";
const LEDGER_FIELDS: &[&str] = &["schema", "runNumber", "blockedUntil", "entries"];
const ENTRY_FIELDS: &[&str] = &["at", "action", "run", "line"];
/// Far more than every daily cap together.
const MAX_ENTRIES: usize = 1000;
const MAX_LINE: usize = 1000;
/// The caller's job that runs `supervise execute` (or, in a reusable
/// workflow, whose jobs are `execute / ...`); a run where it ran may have
/// written.
const EXECUTE_JOB: &str = "execute";
/// The events the supervisor's workflow runs on: its crons, a dispatch and
/// the operator's comment, each with the default branch's workflow file. Only
/// these write a ledger the executor accepts.
const EVENTS: &[&str] = &["schedule", "workflow_dispatch", "issue_comment"];
/// A pull request's run may have changed the workflow file and has no token
/// of the bot's, so it does not count as a lost ledger either; every other
/// run of the workflow may.
const PULL_REQUEST_EVENT: &str = "pull_request";
/// An executor writes only this soon after its run was created, so that the
/// runs that may have written in the last day are the ones created in it and
/// this margin: this, and GitHub's six-hour job limit for its writes.
const MAX_START_MS: i64 = 6 * HOUR_MS;
const RUN_MARGIN_MS: i64 = MAX_START_MS + 6 * HOUR_MS;
/// The first line of the status comment on the log issue.
const STATUS_MARKER: &str = "<!-- agent-board-supervisor-status -->";
/// How far back the status comment is looked for, by its last update: it is
/// replaced on every run, and a week without one starts another.
const STATUS_LOOKBACK_MS: i64 = 7 * DAY_MS;
const STATUS_PAGES: usize = 15;
/// The earlier entries the status comment lists, within a comment's size.
const STATUS_EARLIER: usize = 30;

/// What the caller hands the executor in place of a comment's URL.
pub struct HandBack {
    /// The agent run's safe outputs, JSON Lines as gh-aw defines them.
    pub outputs: String,
    /// The previous ledger, as `(run id, ledger)`, when the caller found one.
    pub previous: Option<(u64, Value)>,
    /// This run: its repository, id and number.
    pub repository: String,
    pub run_id: u64,
    pub run_number: u64,
}

/// The one comment in the agent's safe outputs; none asks for nothing.
pub fn report(outputs: &str) -> Result<String> {
    ensure!(
        outputs.len() <= MAX_OUTPUTS_BYTES,
        "The agent's safe outputs exceed 1 MiB"
    );
    let mut bodies = vec![];
    for line in outputs.lines().filter(|l| !l.trim().is_empty()) {
        // A parse error would quote its input into the public log.
        let output: Value = serde_json::from_str(line)
            .ok()
            .filter(Value::is_object)
            .context("Malformed safe outputs")?;
        if output["type"] == REPORT_TYPE {
            let body = output["body"].as_str().context("Malformed safe outputs")?;
            bodies.push(body.to_owned());
        }
    }
    ensure!(
        bodies.len() <= 1,
        "More than one comment in the agent's safe outputs"
    );
    Ok(bodies.pop().unwrap_or_default())
}

/// The executor's record of its writes in the last day, and how long a lost
/// record blocks them.
#[derive(Clone, Debug, PartialEq)]
pub struct Ledger {
    pub run_number: u64,
    pub blocked_until: Option<i64>,
    pub entries: Vec<Entry>,
}

/// One write: reserved before it starts, then its outcome.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub at: i64,
    pub action: String,
    pub run: String,
    pub line: String,
}

/// Whether `value` is an object with exactly these fields.
fn exactly(value: &Value, fields: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|o| o.len() == fields.len() && fields.iter().all(|f| o.contains_key(*f)))
}

impl Ledger {
    pub fn parse(value: &Value) -> Result<Self> {
        let refuse = || anyhow::anyhow!("The previous ledger is malformed");
        ensure!(
            exactly(value, LEDGER_FIELDS) && value["schema"] == LEDGER_SCHEMA,
            refuse()
        );
        let blocked_until = match &value["blockedUntil"] {
            Value::Null => None,
            at => Some(millis(at).ok_or_else(refuse)?),
        };
        let entries = value["entries"].as_array().ok_or_else(refuse)?;
        ensure!(entries.len() <= MAX_ENTRIES, refuse());
        let entries = entries
            .iter()
            .map(|e| {
                let text = |field: &str| e[field].as_str().ok_or_else(refuse);
                ensure!(exactly(e, ENTRY_FIELDS), refuse());
                let line = text("line")?;
                // The status comment shows it as one list item.
                ensure!(
                    line.len() <= MAX_LINE && !line.contains(['\n', '\r']),
                    refuse()
                );
                let run = text("run")?;
                ensure!(
                    run.starts_with("https://")
                        && !run.contains(|c: char| c.is_whitespace() || "()[]<>".contains(c)),
                    refuse()
                );
                Ok(Entry {
                    at: millis(&e["at"]).ok_or_else(refuse)?,
                    action: text("action")?.into(),
                    run: run.into(),
                    line: line.into(),
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            run_number: value["runNumber"].as_u64().ok_or_else(refuse)?,
            blocked_until,
            entries,
        })
    }

    pub fn to_json(&self) -> Value {
        json!({
            "schema": LEDGER_SCHEMA,
            "runNumber": self.run_number,
            "blockedUntil": self.blocked_until.map(iso),
            "entries": self.entries.iter().map(|e| json!({
                "at": iso(e.at), "action": e.action, "run": e.run, "line": e.line,
            })).collect::<Vec<_>>(),
        })
    }

    /// The writes of `action` it holds, all within the last day.
    pub(super) fn used(&self, action: &str) -> usize {
        self.entries.iter().filter(|e| e.action == action).count()
    }

    /// Reserves every request before the first write: one that may have
    /// reached GitHub counts even if its outcome is never recorded.
    pub(super) fn reserve(&mut self, requests: &[Checked], ctx: &Run) {
        self.entries.extend(requests.iter().map(|r| Entry {
            at: ctx.now,
            action: r.action().into(),
            run: ctx.run_url.clone(),
            line: one_line(&format!("{}: starting", r.describe())),
        }));
    }

    /// Replaces the last reservations with the outcome lines of the same
    /// requests, in order; one not attempted started nothing.
    pub(super) fn settle(&mut self, lines: &[String]) {
        let start = self.entries.len().saturating_sub(lines.len());
        for (entry, line) in self.entries[start..].iter_mut().zip(lines) {
            entry.line = one_line(line.strip_prefix("- ").unwrap_or(line));
        }
        self.entries
            .retain(|e| !e.line.ends_with(": not attempted"));
    }
}

/// A line the next run's [`Ledger::parse`] takes: an API error's text may
/// hold line breaks or run long.
fn one_line(text: &str) -> String {
    let mut line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut end = line.len().min(MAX_LINE);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line.truncate(end);
    line
}

/// Whether the run is one of the supervisor's own, from this repository's
/// default-branch workflow file.
fn own(run: &Value, repository: &str, workflow: &str) -> bool {
    run["path"] == format!(".github/workflows/{workflow}").as_str()
        && !foreign(run, repository)
        && EVENTS.iter().any(|e| run["event"] == *e)
}

/// Whether the run is a fork's or a pull request's (`pull_request`,
/// `pull_request_target`, `pull_request_review`...), which cannot have
/// written as the bot.
fn foreign(run: &Value, repository: &str) -> bool {
    !(run["head_repository"]["full_name"].as_str())
        .is_some_and(|r| r.eq_ignore_ascii_case(repository))
        || (run["event"].as_str()).is_some_and(|e| e.starts_with(PULL_REQUEST_EVENT))
}

/// The ledger this run starts from: the previous run's entries of the last
/// day, with writes blocked until a day after any other run of this workflow
/// since the previous one whose execute job ran, since that run's ledger is
/// not the one read. Also when this run was created, if it is listed.
pub fn books(
    api: &dyn Api,
    policy: &Policy,
    hand_back: &HandBack,
    ctx: &Run,
) -> Result<(Ledger, Option<i64>)> {
    let HandBack {
        repository,
        run_id,
        run_number,
        ..
    } = hand_back;
    let workflow = &policy.settings.supervisor_workflow;
    let previous = match &hand_back.previous {
        Some((id, value)) => {
            let ledger = Ledger::parse(value)?;
            // Any workflow, a pull request's included, can upload an
            // artifact by the ledger's name.
            let run = api.get(&format!("/repos/{repository}/actions/runs/{id}"))?;
            ensure!(
                own(&run, repository, workflow)
                    && run["head_branch"] == ctx.default_branch.as_str()
                    && run["run_number"].as_u64() == Some(ledger.run_number)
                    && ledger.run_number < *run_number,
                "The previous ledger is not an earlier run of the supervisor's workflow"
            );
            Some(ledger)
        }
        None => None,
    };
    let after = previous.as_ref().map_or(0, |l| l.run_number);
    let since = encode(&format!(">={}", iso(ctx.now - DAY_MS - RUN_MARGIN_MS)));
    let base = format!(
        "/repos/{repository}/actions/workflows/{workflow}/runs?created={since}&per_page=100"
    );
    let (mut lost, mut created) = (None, None);
    for page in 1.. {
        ensure!(
            page <= RUN_PAGES,
            "The supervisor's runs are too many to check its ledger"
        );
        let runs = api.get(&format!("{base}&page={page}"))?;
        let runs = runs["workflow_runs"].as_array().context("workflow runs")?;
        for run in runs {
            let id = run["id"].as_u64().context("run id")?;
            let at = millis(&run["created_at"]).context("run time")?;
            if id == *run_id {
                created = Some(at);
                continue;
            }
            // A later run counts too: only the execute jobs are serialized,
            // so one can finish before an earlier run's.
            let number = run["run_number"].as_u64().context("run number")?;
            if number <= after || foreign(run, repository) {
                continue;
            }
            if executed(api, repository, run, id)? {
                lost = lost.max(Some(at + DAY_MS + RUN_MARGIN_MS));
            }
        }
        if runs.len() < 100 {
            break;
        }
    }
    let mut ledger = previous.unwrap_or(Ledger {
        run_number: 0,
        blocked_until: None,
        entries: vec![],
    });
    ledger.run_number = *run_number;
    ledger.entries.retain(|e| e.at >= ctx.now - DAY_MS);
    ledger.blocked_until = ledger.blocked_until.max(lost).filter(|t| *t > ctx.now);
    Ok((ledger, created))
}

/// Refuses a run created longer ago than the margin [`books`] relies on.
pub(super) fn fresh(created: Option<i64>, now: i64) -> Result<()> {
    ensure!(
        created.is_some_and(|at| now - at <= MAX_START_MS),
        "This run was created too long ago to write"
    );
    Ok(())
}

/// Whether the run's execute job started, in any attempt. A run whose jobs
/// include none by that name counts as one that did, so that renaming the
/// job blocks writes rather than hides them.
fn executed(api: &dyn Api, repository: &str, run: &Value, id: u64) -> Result<bool> {
    let jobs = api.get(&format!(
        "/repos/{repository}/actions/runs/{id}/jobs?filter=all&per_page=100"
    ))?;
    ensure!(
        jobs["total_count"].as_u64().is_some_and(|n| n <= 100),
        "A supervisor run has too many jobs to check its ledger"
    );
    let jobs = jobs["jobs"].as_array().context("jobs")?;
    let execute: Vec<&Value> = (jobs.iter())
        .filter(|j| {
            j["name"]
                .as_str()
                .is_some_and(|n| n == EXECUTE_JOB || n.starts_with(&format!("{EXECUTE_JOB} / ")))
        })
        .collect();
    if execute.is_empty() {
        return Ok(!jobs.is_empty() && run["status"] == "completed");
    }
    Ok(execute.iter().any(|j| {
        j["status"] == "in_progress" || (j["status"] == "completed" && j["conclusion"] != "skipped")
    }))
}

/// Replaces the status comment on the log issue with this run's entry and
/// the ledger's earlier ones, or posts it if there is none. Returns the body.
pub fn status(api: &dyn Api, policy: &Policy, ctx: &Run, outcome: &Outcome) -> Result<String> {
    let mut body = format!(
        "{STATUS_MARKER}\nThe supervisor's executor replaces this comment on each run.\n\n{}",
        log_body(policy, ctx, &outcome.lines)
    );
    let mut earlier: Vec<&Entry> = (outcome.ledger.iter())
        .flat_map(|l| &l.entries)
        .filter(|e| e.run != ctx.run_url)
        .collect();
    earlier.sort_by_key(|e| std::cmp::Reverse(e.at));
    if !earlier.is_empty() {
        body.push_str("\n\nEarlier in the last 24 hours:");
        for e in earlier.iter().take(STATUS_EARLIER) {
            body.push_str(&format!("\n- {} [run]({}): {}", iso(e.at), e.run, e.line));
        }
        if earlier.len() > STATUS_EARLIER {
            body.push_str(&format!("\n- and {} more", earlier.len() - STATUS_EARLIER));
        }
    }
    let log = &policy.agent.log;
    match status_comment(api, policy, ctx.now)? {
        Some(id) => api.send(
            "PATCH",
            &format!("/repos/{}/issues/comments/{id}", log.repository),
            &json!({"body": body}),
        )?,
        None => api.send(
            "POST",
            &format!("/repos/{}/issues/{}/comments", log.repository, log.issue),
            &json!({"body": body}),
        )?,
    };
    Ok(body)
}

/// The bot's newest status comment on the log issue, among the comments
/// updated within the lookback.
fn status_comment(api: &dyn Api, policy: &Policy, now: i64) -> Result<Option<u64>> {
    let log = &policy.agent.log;
    let since = encode(&iso(now - STATUS_LOOKBACK_MS));
    let mut found = None;
    for page in 1..=STATUS_PAGES {
        let comments = api.get(&format!(
            "/repos/{}/issues/{}/comments?since={since}&per_page=100&page={page}",
            log.repository, log.issue
        ))?;
        let comments = comments.as_array().context("log comments")?;
        for c in comments {
            if c["user"]["login"] == policy.settings.bot.as_str()
                && c["body"]
                    .as_str()
                    .is_some_and(|b| b.starts_with(STATUS_MARKER))
            {
                found = c["id"].as_u64().or(found);
            }
        }
        if comments.len() < 100 {
            return Ok(found);
        }
    }
    anyhow::bail!("The supervisor log is too long to find its status comment")
}
