//! `supervise restart` (docs/supervisor-requests.md#pass-restart): start the
//! poll workflow again once its chain has stopped, as after the GitHub
//! incident of agent-board#715, and tell the operator once when that does not
//! take. Its record is its own marker comments on the log issue, so a lost
//! artifact cannot make it restart in a loop. Off unless the supervisor's
//! file sets `restartPass`.
use super::{Api, HOUR_MS, Settings, encode, iso, millis};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// The supervisor file's key.
pub const OPTION: &str = "restartPass";

/// The bounds of `staleMinutes`: below two of tracker's ten-minute cycles a
/// slow GitHub could look like a stall; above a day nothing is restarted.
const STALE_MINUTES: std::ops::RangeInclusive<u64> = 20..=24 * 60;

/// The most dispatch inputs, and the longest value, `restartPass` may give.
const MAX_INPUTS: usize = 10;
const MAX_INPUT_BYTES: usize = 256;

/// The start of each record's first line; the rest is `KIND after=ID -->`.
const MARKER: &str = "<!-- agent-board-pass-restart ";
const RESTARTED: &str = "restarted";
const ESCALATED: &str = "escalated";

/// A record older than this is forgotten: a stall left for a day is
/// restarted again, once.
const RECORD_LOOKBACK_MS: i64 = 24 * HOUR_MS;

/// Pages of the log issue's comments of the last day read for the record.
const COMMENT_PAGES: usize = 10;

/// The poll workflow's newest runs read, one page: a stall leaves few.
const RUN_PAGE: usize = 100;

/// `restartPass`: how long the poll workflow may go without a run, and the
/// inputs of the dispatch that starts it again.
#[derive(Clone, Debug, PartialEq)]
pub struct RestartPass {
    pub stale_minutes: u64,
    pub inputs: BTreeMap<String, String>,
}

impl RestartPass {
    /// `value` is the supervisor file's `restartPass`; absent or null is off.
    pub fn from_json(value: Option<&Value>) -> Result<Option<Self>> {
        let Some(value) = value.filter(|v| !v.is_null()) else {
            return Ok(None);
        };
        let what = format!("supervisor config {OPTION}");
        let object = value
            .as_object()
            .with_context(|| format!("{what} is not an object"))?;
        if let Some(key) = object
            .keys()
            .find(|k| !["staleMinutes", "inputs"].contains(&k.as_str()))
        {
            bail!("{what}: unknown key {key:?}");
        }
        let stale_minutes = object
            .get("staleMinutes")
            .and_then(Value::as_u64)
            .filter(|m| STALE_MINUTES.contains(m))
            .with_context(|| {
                format!(
                    "{what}.staleMinutes must be an integer from {} to {}",
                    STALE_MINUTES.start(),
                    STALE_MINUTES.end()
                )
            })?;
        let inputs = match object.get("inputs") {
            None => Map::new(),
            Some(inputs) => inputs
                .as_object()
                .cloned()
                .with_context(|| format!("{what}.inputs is not an object"))?,
        };
        ensure!(
            inputs.len() <= MAX_INPUTS,
            "{what}.inputs: more than {MAX_INPUTS}"
        );
        let inputs = inputs
            .into_iter()
            .map(|(name, value)| {
                ensure!(
                    !name.is_empty()
                        && name
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)),
                    "{what}.inputs: invalid input name {name:?}"
                );
                let value = value
                    .as_str()
                    .filter(|v| v.len() <= MAX_INPUT_BYTES)
                    .with_context(|| {
                        format!(
                            "{what}.inputs.{name} must be a string of at most {MAX_INPUT_BYTES} bytes"
                        )
                    })?;
                Ok((name, value.to_owned()))
            })
            .collect::<Result<_>>()?;
        Ok(Some(Self {
            stale_minutes,
            inputs,
        }))
    }
}

/// The newest record on the log issue: the restart after run `after`, and
/// whether the operator has been told it did not take.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Record {
    pub after: u64,
    pub escalated: bool,
}

impl Record {
    fn parse(body: &str) -> Option<Self> {
        let rest = body.lines().next()?.strip_prefix(MARKER)?;
        let (kind, rest) = rest.split_once(" after=")?;
        let after = rest.strip_suffix(" -->")?.parse().ok()?;
        let escalated = match kind {
            RESTARTED => false,
            ESCALATED => true,
            _ => return None,
        };
        Some(Self { after, escalated })
    }

    fn line(self) -> String {
        let kind = if self.escalated { ESCALATED } else { RESTARTED };
        format!("{MARKER}{kind} after={} -->", self.after)
    }
}

/// What one `supervise restart` does.
#[derive(Debug, PartialEq)]
pub enum Decision {
    /// Nothing, and why.
    Wait(String),
    /// Start the poll workflow; the chain stopped at run `after`, `age`
    /// minutes old.
    Restart { after: u64, age: i64 },
    /// Tell the operator the restart after run `after` did not take.
    Escalate { after: u64, error: String },
}

fn id(run: &Value) -> Result<u64> {
    run["id"].as_u64().context("a poll run without an id")
}

/// The decision from the poll workflow's newest runs, newest first, and the
/// newest record. A restart is recorded before it is tried, and a recorded
/// one is never tried again: until a pass after it succeeds and another
/// starts after that one, the chain is not back, and the operator is told
/// once instead.
pub fn decide(
    runs: &[Value],
    record: Option<Record>,
    now: i64,
    stale_minutes: u64,
) -> Result<Decision> {
    let Some(newest) = runs.first() else {
        return Ok(Decision::Wait(
            "no poll run is listed; nothing to restart".into(),
        ));
    };
    if let Some(run) = runs.iter().find(|r| r["status"] != "completed") {
        return Ok(Decision::Wait(format!(
            "poll run {} is {}; not restarting",
            id(run)?,
            run["status"].as_str().unwrap_or("unfinished")
        )));
    }
    let newest_id = id(newest)?;
    let created = millis(&newest["created_at"])
        .with_context(|| format!("poll run {newest_id}: no created_at"))?;
    let age = (now - created) / 60_000;
    if age < stale_minutes as i64 {
        return Ok(Decision::Wait(format!(
            "poll run {newest_id} started {age} minutes ago; not restarting"
        )));
    }
    let mut after_record = Vec::new();
    if let Some(r) = record {
        for run in runs {
            if id(run)? > r.after {
                after_record.push(run);
            }
        }
    }
    // Newest first: a success with a run after it is a chain that went on.
    let recovered = after_record
        .iter()
        .skip(1)
        .any(|run| run["conclusion"] == "success");
    match record.filter(|_| !recovered) {
        None => Ok(Decision::Restart {
            after: newest_id,
            age,
        }),
        Some(Record {
            after,
            escalated: true,
        }) => Ok(Decision::Wait(format!(
            "the operator was told the restart after poll run {after} did not take; not restarting"
        ))),
        Some(Record { after, .. }) => Ok(Decision::Escalate {
            after,
            error: match after_record.first() {
                None => format!("no poll run started after the restart, {age} minutes ago or more"),
                Some(run) => format!(
                    "poll run {} after the restart ended {} and none followed it",
                    id(run)?,
                    run["conclusion"].as_str().unwrap_or("without a conclusion")
                ),
            },
        }),
    }
}

/// One `supervise restart` as the bot: reads the poll workflow's runs on
/// `branch` and the log issue's record, then records and starts a restart or
/// escalates, as [`decide`] says. Prints what it did.
pub fn run(api: &dyn Api, s: &Settings, branch: &str, run_url: &str, now: i64) -> Result<String> {
    let Some(restart) = &s.restart_pass else {
        return Ok(format!("{OPTION} is not configured; nothing to do"));
    };
    let agent = s
        .agent
        .as_ref()
        .with_context(|| format!("{OPTION} needs the agent's log issue"))?;
    let workflow = format!(
        "/repos/{}/actions/workflows/{}",
        s.dispatch_repo, s.poll_workflow
    );
    let listing = api.get(&format!(
        "{workflow}/runs?branch={}&exclude_pull_requests=true&per_page={RUN_PAGE}",
        encode(branch)
    ))?;
    let runs = listing["workflow_runs"]
        .as_array()
        .context("poll runs: no workflow_runs")?;
    let record = newest_record(api, s, now)?;
    let log = format!(
        "/repos/{}/issues/{}/comments",
        agent.log.repository, agent.log.issue
    );
    let poll = &s.poll_workflow;
    let escalate = |after: u64, error: &str| -> Result<String> {
        let line = Record {
            after,
            escalated: true,
        }
        .line();
        let body = format!(
            "{line}\n@{} the supervisor's [restart]({run_url}) of {poll} after run {after} did not take: {error}. It will not restart {poll} again until a pass succeeds and another follows it, or a day has passed.",
            s.operator
        );
        api.send("POST", &log, &json!({ "body": body }))?;
        Ok(format!("escalated: {error}"))
    };
    match decide(runs, record, now, restart.stale_minutes)? {
        Decision::Wait(why) => Ok(why),
        Decision::Escalate { after, error } => escalate(after, &error),
        Decision::Restart { after, age } => {
            // Recorded first: a dispatch whose answer is lost may still have
            // started a run, and must not be tried again.
            let line = Record {
                after,
                escalated: false,
            }
            .line();
            let body = format!(
                "{line}\nThe pass stopped: {poll}'s newest run, {after}, started {age} minutes ago and none is queued or running. The supervisor's [run]({run_url}) starts it once."
            );
            api.send("POST", &log, &json!({ "body": body }))?;
            let dispatch = json!({ "ref": branch, "inputs": restart.inputs });
            match api.send("POST", &format!("{workflow}/dispatches"), &dispatch) {
                Ok(_) => Ok(format!("started {poll} after poll run {after}")),
                Err(e) => escalate(after, &format!("starting it failed: {e:#}")),
            }
        }
    }
}

/// The bot's newest record on the log issue within [`RECORD_LOOKBACK_MS`].
fn newest_record(api: &dyn Api, s: &Settings, now: i64) -> Result<Option<Record>> {
    let agent = s
        .agent
        .as_ref()
        .with_context(|| format!("{OPTION} needs the agent's log issue"))?;
    let since = now - RECORD_LOOKBACK_MS;
    let mut found = None;
    for page in 1..=COMMENT_PAGES {
        let comments = api.get(&format!(
            "/repos/{}/issues/{}/comments?since={}&per_page=100&page={page}",
            agent.log.repository,
            agent.log.issue,
            encode(&iso(since))
        ))?;
        let comments = comments.as_array().context("log comments")?;
        // Oldest first; since= filters on updated_at, a record counts from
        // when it was written.
        for c in comments {
            if c["user"]["login"] == s.bot.as_str()
                && millis(&c["created_at"]).is_some_and(|at| at >= since)
                && let Some(record) = c["body"].as_str().and_then(Record::parse)
            {
                found = Some(record);
            }
        }
        if comments.len() < 100 {
            return Ok(found);
        }
    }
    bail!("the supervisor log is too long to find the pass restart's record")
}

#[cfg(test)]
mod tests;
