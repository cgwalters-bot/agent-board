//! The health pass: observations read with a read-only token ([`collect`]),
//! then judged by fixed thresholds ([`evaluate`]). No actor is called here;
//! in particular, never `poll --apply` to gather facts. Only links reach the
//! report, never remote titles or text, since the report becomes an agent's
//! task and a public job summary.
use super::{Api, HOUR_MS, Limits, Settings, encode, iso, millis, run_title, word_repo};
use crate::plan::DISPATCH_MARKER;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Each check, in report order, with what to do when it fails.
pub const CHECKS: &[(&str, &str)] = &[
    ("poll", "Inspect consecutive failed poll runs."),
    (
        "runs",
        "Inspect the timeout and cancel only after checking the run.",
    ),
    ("broker", "Inspect the broker probe and connectivity."),
    (
        "epics",
        "Inspect coordinator checkpoints before retrying planning.",
    ),
    (
        "dispatch",
        "Inspect the dispatch target before cancelling duplicate work.",
    ),
    ("queue", "Inspect queue removal events and failed checks."),
    (
        "approved",
        "Inspect merge requirements for the approved head.",
    ),
    (
        "infrastructure",
        "Confirm an infrastructure outage before clearing needs-human.",
    ),
    ("dependabot", "Inspect dependency queue failures."),
    ("decisions", "Read and answer the outstanding decisions."),
    (
        "release",
        "Review the release before updating the wrapper pin.",
    ),
    (
        "idle",
        "Read the scheduler passes for why nothing starts with slots free.",
    ),
    (
        "stuck",
        "Inspect plans skipped as dispatched already whose run has ended.",
    ),
    (
        "pace",
        "Compare the pool's usage with the pace of its window.",
    ),
    (
        "failures",
        "Inspect agent runs that failed for a reason that recurs.",
    ),
    (
        "held",
        "Read the reason for the needs-human hold on the bot's pull request.",
    ),
    (
        "unqueued",
        "Inspect approved, clean pull requests that are not in the merge queue.",
    ),
    (
        "unclosed",
        "Inspect issues left open after their implement pull request merged.",
    ),
    (
        "scheduled",
        "Read the failing job of the scheduled workflow that keeps failing.",
    ),
    (
        "unset",
        "Set each listed option in the caller's config, or confirm it is meant to stay off.",
    ),
];

/// The label the poll stops a pull request with.
const HOLD: &str = "needs-human";
/// Coordinator checkpoints on an epic: `<!-- coordinator/v1: {...} -->`.
const CHECKPOINT_MARKER: &str = "coordinator/v1";
/// The usage snapshot comment (tracker's usage.yml) and its JSON's schema.
pub(crate) const USAGE_MARKER: &str = "<!-- bot-usage v1 -->";
const USAGE_SCHEMA: &str = "bot-usage/v1";
/// agentic-job's status comment on a run's target ends with this.
fn status_end(run: u64) -> String {
    format!("\n\n<!-- agentic-job-run: {run} -->\n\n<!-- agentic-job-status -->")
}
const STATUS_REASON: &str = " Its reason:";
const RUNNING: &[&str] = &["in_progress", "queued", "waiting", "pending"];
/// The census of epics reads at most this many, in pages of a hundred.
const MAX_EPICS: usize = 50;
const REST_PAGES: usize = 20;
const COMMENT_PAGES: usize = 20;
/// A scheduled workflow fails the check once this many of its runs in the
/// lookback ended and every one failed: one failure may be a flake.
const SCHEDULED_FAILURES: u64 = 2;
/// Conclusions that count as a failed run; cancelled and skipped runs
/// neither pass nor fail.
const FAILED: &[&str] = &["failure", "timed_out", "startup_failure"];

/// Each check's items, or `None` where they could not be read whole.
pub type Observations = BTreeMap<String, Option<Vec<Value>>>;

/// What `collect` read, by check id: `None` where the observation was
/// unavailable or incomplete. `previous` is the last pass's report.
#[derive(Debug, Default)]
pub struct Snapshot {
    pub observations: Observations,
    pub previous: Option<Vec<Value>>,
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// A probe that is switched off leaves skipped runs: no observation, which
/// fails once it has lasted, like any other.
fn probes(items: &[Value]) -> Vec<&Value> {
    items
        .iter()
        .filter(|r| r["conclusion"] != "skipped")
        .collect()
}

/// An observation missing for an hour is a blind spot, not a transient: it
/// fails, so the agent runs and says which access it lacks. Since when is
/// carried from the previous pass's report.
fn unavailable(
    id: &str,
    action: &str,
    previous: Option<&[Value]>,
    now: i64,
    limits: &Limits,
) -> Value {
    let carried = previous
        .unwrap_or_default()
        .iter()
        .find(|c| c["id"] == id)
        .and_then(|c| millis(&c["unavailable_since"]))
        .filter(|at| *at <= now)
        .unwrap_or(now);
    let lasted = (now - carried) as f64 >= limits.unavailable_fail_hours * HOUR_MS as f64;
    json!({
        "id": id,
        "status": if lasted { "fail" } else { "warn" },
        "evidence": [],
        "unavailable_since": iso(carried),
        "suggested_action": format!("Observation unavailable or incomplete; {action}"),
    })
}

/// The report: one entry for each of [`CHECKS`], `ok`, `warn` or `fail`.
pub fn evaluate(snapshot: &Snapshot, now: i64, limits: &Limits) -> Vec<Value> {
    let age = |v: &Value| millis(v).map(|at| (now - at) as f64 / HOUR_MS as f64);
    let older = |v: &Value, hours: f64| age(v).is_some_and(|a| a > hours);
    CHECKS
        .iter()
        .map(|&(id, action)| {
            let Some(Some(items)) = snapshot.observations.get(id) else {
                return unavailable(id, action, snapshot.previous.as_deref(), now, limits);
            };
            if id == "broker" && probes(items).is_empty() {
                return unavailable(id, action, snapshot.previous.as_deref(), now, limits);
            }
            let bad: Vec<&Value> = match id {
                "poll" => {
                    let failures: Vec<&Value> = items
                        .iter()
                        .filter(|r| r["status"] == "completed")
                        .take(limits.poll_failures)
                        .collect();
                    let all = failures.len() == limits.poll_failures
                        && failures
                            .iter()
                            .all(|r| FAILED.iter().any(|c| r["conclusion"] == *c));
                    if all { failures } else { vec![] }
                }
                "runs" => items
                    .iter()
                    .filter(|r| {
                        let started = if truthy(&r["run_started_at"]) {
                            &r["run_started_at"]
                        } else {
                            &r["created_at"]
                        };
                        r["status"] != "completed"
                            && older(started, limits.run_timeout_minutes / 60.0)
                    })
                    .collect(),
                "broker" => probes(items)
                    .into_iter()
                    .take(1)
                    .filter(|r| {
                        r["conclusion"] != "success"
                            || older(&r["updated_at"], limits.probe_stale_minutes / 60.0)
                    })
                    .collect(),
                "epics" => items
                    .iter()
                    .filter(|e| {
                        truthy(&e["missing"])
                            || e["failures"]
                                .as_f64()
                                .is_some_and(|f| f >= limits.epic_failures)
                            || (truthy(&e["pending"])
                                && older(&e["pending"]["at"], limits.epic_stale_hours))
                    })
                    .collect(),
                "dispatch" => items
                    .iter()
                    .filter(|i| {
                        i["state"] == "closed" || truthy(&i["hasPR"]) || truthy(&i["umbrella"])
                    })
                    .collect(),
                "queue" | "dependabot" => items
                    .iter()
                    .filter(|e| {
                        e["event"] == "removed_from_merge_queue"
                            && age(&e["created_at"])
                                .is_some_and(|a| a <= limits.event_lookback_hours)
                    })
                    .collect(),
                "approved" => items
                    .iter()
                    .filter(|p| older(&p["approvedAt"], limits.approved_wait_hours))
                    .collect(),
                "infrastructure" => items
                    .iter()
                    .filter(|p| truthy(&p["infrastructure"]))
                    .collect(),
                "decisions" => items
                    .iter()
                    .filter(|i| older(&i["created_at"], limits.decision_wait_hours))
                    .collect(),
                "release" => items.iter().filter(|i| i["latest"] != i["pin"]).collect(),
                // The newest scheduler passes, each idle or not: all of them idle.
                "idle" => {
                    let newest: Vec<&Value> = items.iter().take(limits.idle_passes).collect();
                    if newest.len() == limits.idle_passes
                        && newest.iter().all(|p| truthy(&p["idle"]))
                    {
                        newest
                    } else {
                        vec![]
                    }
                }
                // The scheduler skips a recorded plan for ever (retries off):
                // once its run ended with its PR merged, or with none,
                // nothing will start it.
                "stuck" => items
                    .iter()
                    .filter(|s| {
                        truthy(&s["ended"]) && (s["pull"] == "merged" || s["pull"] == "none")
                    })
                    .collect(),
                // Used share of the window against the share of it elapsed, in points.
                "pace" => items
                    .iter()
                    .filter(|p| {
                        let window = p["window_minutes"].as_f64().unwrap_or(f64::NAN) * 60_000.0;
                        let left = millis(&p["resets_at"]).map_or(f64::NAN, |at| (at - now) as f64);
                        let elapsed = 100.0 * (1.0 - left / window).clamp(0.0, 1.0);
                        let used = p["used_percent"].as_f64().unwrap_or(f64::NAN);
                        window > 0.0
                            && left > 0.0
                            && (used - elapsed).abs() > limits.pace_margin_percent
                    })
                    .collect(),
                "failures" => items.iter().filter(|r| truthy(&r["reason"])).collect(),
                // A hold whose time is not known has lasted at least as long as we can see.
                "held" => items
                    .iter()
                    .filter(|p| {
                        !truthy(&p["labeledAt"]) || older(&p["labeledAt"], limits.hold_wait_hours)
                    })
                    .collect(),
                "unqueued" => items
                    .iter()
                    .filter(|p| {
                        truthy(&p["clean"])
                            && !truthy(&p["queued"])
                            && older(&p["approvedAt"], limits.unqueued_wait_hours)
                    })
                    .collect(),
                "unclosed" => items
                    .iter()
                    .filter(|p| {
                        p["issueState"] == "open"
                            && older(&p["mergedAt"], limits.unclosed_wait_hours)
                    })
                    .collect(),
                "scheduled" => items
                    .iter()
                    .filter(|w| {
                        w["runs"].as_u64().is_some_and(|n| n >= SCHEDULED_FAILURES)
                            && w["failed"] == w["runs"]
                    })
                    .collect(),
                // A feature merged off stays off until its option is named
                // (tracker#733): each one left unset is shown, and only warns.
                "unset" => items.iter().collect(),
                _ => vec![],
            };
            // Bounded, so the agent's task stays inside the wrapper's limit.
            let mut evidence: Vec<&str> = vec![];
            for item in &bad {
                for key in ["html_url", "reason_url", "issue_url", "run_url", "option"] {
                    if let Some(url) = item[key].as_str().filter(|u| !u.is_empty())
                        && !evidence.contains(&url)
                    {
                        evidence.push(url);
                    }
                }
            }
            evidence.truncate(limits.max_evidence);
            let status = if bad.is_empty() {
                "ok"
            } else if ["infrastructure", "unset"].contains(&id)
                || bad.iter().all(|i| truthy(&i["missing"]))
            {
                "warn"
            } else {
                "fail"
            };
            json!({
                "id": id,
                "status": status,
                "evidence": evidence,
                "suggested_action": if bad.is_empty() { "None." } else { action },
            })
        })
        .collect()
}

const OPEN_PULLS: &str = r#"query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) {
  pullRequests(states: OPEN, first: 100) { pageInfo { hasNextPage } nodes {
    url isInMergeQueue mergeStateStatus author { __typename login }
    labels(first: 30) { nodes { name } }
    timelineItems(itemTypes: [LABELED_EVENT], last: 50) { nodes { ... on LabeledEvent { createdAt label { name } } } }
    comments(last: 10) { nodes { url author { login } } }
  } }
} }"#;

const RECENT_PULLS: &str = r#"query($owner: String!, $name: String!, $after: String) { repository(owner: $owner, name: $name) {
  pullRequests(first: 50, after: $after, orderBy: {field: UPDATED_AT, direction: DESC}) { pageInfo { hasNextPage endCursor } nodes {
    url updatedAt mergedAt headRefName body author { __typename login }
    timelineItems(itemTypes: [REMOVED_FROM_MERGE_QUEUE_EVENT], last: 10) { nodes { ... on RemovedFromMergeQueueEvent { createdAt reason } } }
  } }
} }"#;

/// REST does not say who last edited a comment; GraphQL does.
const COMMENTS_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) { issue(number: $number) {
    comments(first: 100, after: $cursor) {
      nodes { databaseId updatedAt lastEditedAt body author { login } editor { login } }
      pageInfo { hasNextPage endCursor }
    }
  } }
}"#;

/// REST names an app `name[bot]`, GraphQL `name`.
fn login(author: &Value) -> String {
    let name = author["login"].as_str().unwrap_or_default();
    if author["__typename"] == "Bot" {
        format!("{name}[bot]")
    } else {
        name.to_owned()
    }
}

/// The comments that are `who`'s, by agent-board's rule for plans: written
/// by them and, if edited at all, last edited by them, as GitHub lets anyone
/// with write access edit a comment. `comments` are GraphQL nodes.
fn own_comments<'a>(comments: &'a [Value], who: &str) -> Vec<&'a Value> {
    let is = |actor: &Value| {
        actor["login"]
            .as_str()
            .is_some_and(|l| l.eq_ignore_ascii_case(who))
    };
    comments
        .iter()
        .filter(|c| {
            is(&c["author"])
                && !((truthy(&c["lastEditedAt"]) || truthy(&c["editor"])) && !is(&c["editor"]))
        })
        .collect()
}

/// The newest coordinator checkpoint of the bot's: its state, or `None`.
fn checkpoint(comments: &[Value], bot: &str) -> Result<Option<Value>> {
    let prefix = format!("<!-- {CHECKPOINT_MARKER}: ");
    let Some(latest) = own_comments(comments, bot)
        .into_iter()
        .rfind(|c| c["body"].as_str().is_some_and(|b| b.starts_with(&prefix)))
    else {
        return Ok(None);
    };
    let body = latest["body"].as_str().unwrap_or_default();
    let line = body
        .split_once('\n')
        .map(|(line, _)| line)
        .context("invalid coordinator record")?;
    let json = line
        .strip_prefix(&prefix)
        .and_then(|l| l.strip_suffix(" -->"))
        .filter(|j| j.starts_with('{') && j.ends_with('}'))
        .context("invalid coordinator record")?;
    let state: Value = serde_json::from_str(json).context("invalid coordinator record")?;
    ensure!(state.is_object(), "invalid coordinator record");
    Ok(Some(state))
}

/// The JSON of the usage snapshot comment, if it holds one of its schema.
pub(crate) fn usage_snapshot(body: &str) -> Option<Value> {
    let start = if body.starts_with("```json\n") {
        0
    } else {
        body.find("\n```json\n")? + 1
    };
    let rest = &body[start + "```json\n".len()..];
    let mut from = 0;
    let json = loop {
        let at = from + rest[from..].find("\n```")?;
        let after = &rest[at + 4..];
        if after.is_empty() || after.starts_with('\n') {
            break &rest[..at];
        }
        from = at + 1;
    };
    let snap: Value = serde_json::from_str(json).ok()?;
    (snap["schema"] == USAGE_SCHEMA).then_some(snap)
}

/// The scheduler's line after the runner's timestamp: `^\S+Z (\[dry-run\] )?(.*)$`.
fn log_line(line: &str) -> Option<&str> {
    let (stamp, rest) = line.split_once(' ')?;
    if stamp.len() < 2 || !stamp.ends_with('Z') || stamp.contains(char::is_whitespace) {
        return None;
    }
    let rest = rest.strip_prefix("[dry-run] ").unwrap_or(rest);
    (!rest.is_empty()).then_some(rest)
}

fn strip_colours(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("\x1b[") {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 2..];
        let len = tail
            .find(|c: char| !(c.is_ascii_digit() || c == ';'))
            .unwrap_or(tail.len());
        if tail[len..].starts_with('m') {
            rest = &tail[len + 1..];
        } else {
            out.push_str("\x1b[");
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())
}

/// `idle: N slot(s) free and nothing started...`
fn idle_line(line: &str) -> bool {
    line.strip_prefix("idle: ")
        .and_then(|rest| rest.split_once(' '))
        .is_some_and(|(n, rest)| digits(n) && rest.starts_with("slot(s) free and nothing started"))
}

/// The record a scheduler skip names: `O/R#N (epic ...) skip: plan URL was
/// dispatched already, https://github.com/O/R/issues/N#issuecomment-ID`.
fn skip_line(line: &str) -> Option<(&str, u64)> {
    let (target, rest) = line.split_once(' ')?;
    let (repo, number) = target.split_once('#')?;
    if !word_repo(repo) || !digits(number) {
        return None;
    }
    let rest = rest.strip_prefix("(epic ")?;
    let (_, rest) = rest.split_once(')')?;
    let rest = rest.strip_prefix(" skip: plan https://")?;
    let (url, rest) = rest.split_once(' ')?;
    if url.is_empty() {
        return None;
    }
    let rest = rest.strip_prefix("was dispatched already, https://github.com/")?;
    let (record_repo, rest) = rest.split_once("/issues/")?;
    let (issue, id) = rest.split_once("#issuecomment-")?;
    if !word_repo(record_repo) || !digits(issue) || !digits(id) {
        return None;
    }
    Some((record_repo, id.parse().ok()?))
}

/// The last `https://github.com/{repo}/actions/runs/ID` in `text`.
fn last_run_link(text: &str, repo: &str) -> Option<u64> {
    let needle = format!("https://github.com/{repo}/actions/runs/");
    text.match_indices(&needle)
        .filter_map(|(at, _)| {
            let tail = &text[at + needle.len()..];
            let len = tail
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(tail.len());
            tail[..len].parse().ok()
        })
        .last()
}

/// Issues and pull requests a text links: `https://github.com/O/R/(issues|pull)/N`.
fn item_links(text: &str) -> Vec<(String, u64)> {
    let needle = "https://github.com/";
    text.match_indices(needle)
        .filter_map(|(at, _)| {
            let tail = &text[at + needle.len()..];
            let end = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || "_.-/".contains(c)))
                .unwrap_or(tail.len());
            let parts: Vec<&str> = tail[..end].splitn(5, '/').collect();
            let [owner, name, kind, number, ..] = parts[..] else {
                return None;
            };
            let repo = format!("{owner}/{name}");
            (word_repo(&repo) && ["issues", "pull"].contains(&kind) && digits(number))
                .then(|| Some((repo, number.parse().ok()?)))
                .flatten()
        })
        .collect()
}

/// `\bE\d{3}\b`: an apply refusal code such as E008.
fn refusal_code(text: &str) -> bool {
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let b = text.as_bytes();
    (0..b.len()).any(|i| {
        b[i] == b'E'
            && (i == 0 || !word(b[i - 1]))
            && b.len() >= i + 4
            && b[i + 1..i + 4].iter().all(u8::is_ascii_digit)
            && (b.len() == i + 4 || !word(b[i + 4]))
    })
}

/// `^\s*part of\b`, on any line, in any case.
fn says_part_of(body: &str) -> bool {
    body.lines().any(|line| {
        let line = line.trim_start().to_ascii_lowercase();
        line.strip_prefix("part of")
            .is_some_and(|rest| !rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'))
    })
}

/// The release tags `uses: {release}/.github/workflows/(policy|check|apply).yml@vX.Y.Z` pins.
fn wrapper_pins(text: &str, release: &str) -> Vec<String> {
    let needle = format!("uses: {release}/.github/workflows/");
    text.match_indices(&needle)
        .filter_map(|(at, _)| {
            let tail = &text[at + needle.len()..];
            let tail = ["policy", "check", "apply"]
                .iter()
                .find_map(|n| tail.strip_prefix(n))?
                .strip_prefix(".yml@v")?;
            let mut end = 0;
            for part in 0..3 {
                let len = tail[end..]
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(tail.len() - end);
                if len == 0 {
                    return None;
                }
                end += len;
                if part < 2 {
                    if !tail[end..].starts_with('.') {
                        return None;
                    }
                    end += 1;
                }
            }
            Some(format!("v{}", &tail[..end]))
        })
        .collect()
}

fn base64_decode(text: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.bytes().filter(|c| !c.is_ascii_whitespace()) {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => bail!("invalid base64"),
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

struct Collector<'a> {
    api: &'a dyn Api,
    s: &'a Settings,
    branch: String,
    now: i64,
    root: String,
    /// Reads shared by checks, failures included, so one failing read is
    /// not repeated for every check that needs it.
    cache: RefCell<HashMap<String, Result<Value, String>>>,
}

impl Collector<'_> {
    fn once(&self, key: &str, read: impl FnOnce() -> Result<Value>) -> Result<Value> {
        if let Some(cached) = self.cache.borrow().get(key) {
            return cached.clone().map_err(anyhow::Error::msg);
        }
        let result = read().map_err(|e| format!("{e:#}"));
        self.cache
            .borrow_mut()
            .insert(key.to_owned(), result.clone());
        result.map_err(anyhow::Error::msg)
    }

    fn lookback(&self) -> i64 {
        self.now - (self.s.limits.event_lookback_hours * HOUR_MS as f64) as i64
    }

    /// One page of a list. Unless `sample`, a full page is a truncated
    /// snapshot, not a complete one.
    fn list(&self, path: &str, sample: bool) -> Result<Vec<Value>> {
        let page = self.s.limits.page_size;
        let sep = if path.contains('?') { '&' } else { '?' };
        let data = self.api.get(&format!("{path}{sep}per_page={page}"))?;
        let items = match &data {
            Value::Array(items) => items.clone(),
            _ => data["workflow_runs"]
                .as_array()
                .cloned()
                .context("not a list")?,
        };
        ensure!(sample || (items.len() as u64) < page, "snapshot truncated");
        Ok(items)
    }

    fn cached_list(&self, path: &str) -> Result<Vec<Value>> {
        let value = self.once(&format!("list {path}"), || {
            Ok(Value::Array(self.list(path, false)?))
        })?;
        Ok(value.as_array().cloned().unwrap_or_default())
    }

    fn cached_get(&self, path: &str) -> Result<Value> {
        self.once(&format!("get {path}"), || self.api.get(path))
    }

    /// Every page of a REST list in pages of a hundred, up to `pages`.
    fn paged(&self, path: &str, pages: usize) -> Result<Vec<Value>> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut all = vec![];
        for page in 1..=pages {
            let items = self
                .api
                .get(&format!("{path}{sep}per_page=100&page={page}"))?;
            let items = items.as_array().context("not a list")?;
            all.extend(items.iter().cloned());
            if items.len() < 100 {
                return Ok(all);
            }
        }
        bail!("{path}: more than {} entries", pages * 100)
    }

    fn query(&self, text: &str, variables: Value) -> Result<Value> {
        let data = self.api.graphql(text, variables)?;
        ensure!(data.is_object(), "GraphQL error");
        Ok(data)
    }

    fn comments(&self, repo: &str, number: u64) -> Result<Vec<Value>> {
        let (owner, name) = repo.split_once('/').context("repository")?;
        let mut all = vec![];
        let mut cursor = Value::Null;
        for _ in 0..COMMENT_PAGES {
            let data = self.query(
                COMMENTS_QUERY,
                json!({"owner": owner, "name": name, "number": number, "cursor": cursor}),
            )?;
            let comments = &data["repository"]["issue"]["comments"];
            all.extend(
                comments["nodes"]
                    .as_array()
                    .context("comments not readable")?
                    .iter()
                    .cloned(),
            );
            if comments["pageInfo"]["hasNextPage"] != true {
                return Ok(all);
            }
            cursor = comments["pageInfo"]["endCursor"].clone();
        }
        bail!("more than {} comments", COMMENT_PAGES * 100)
    }

    /// The bot's open pull requests in the followed repositories, one
    /// query a repository for their queue, merge state, labels and latest
    /// comments, instead of a request per pull request.
    fn open_pulls(&self) -> Result<Vec<Value>> {
        let value = self.once("open pulls", || {
            let mut all = vec![];
            for repo in &self.s.follow {
                let (owner, name) = repo.split_once('/').context("repository")?;
                let data = self.query(OPEN_PULLS, json!({"owner": owner, "name": name}))?;
                let pulls = &data["repository"]["pullRequests"];
                ensure!(
                    pulls["pageInfo"]["hasNextPage"] != true,
                    "open PRs truncated"
                );
                all.extend(
                    pulls["nodes"]
                        .as_array()
                        .context("open PRs")?
                        .iter()
                        .cloned(),
                );
            }
            Ok(Value::Array(all))
        })?;
        Ok(value.as_array().cloned().unwrap_or_default())
    }

    /// Pull requests updated within the lookback, newest first, read in pages
    /// until one reaches past it. More than maxPages of them is incomplete.
    fn recent_pulls(&self) -> Result<Vec<Value>> {
        let value = self.once("recent pulls", || {
            let mut all = vec![];
            for repo in &self.s.follow {
                let (owner, name) = repo.split_once('/').context("repository")?;
                let mut after = Value::Null;
                for page in 0.. {
                    ensure!(page < self.s.limits.max_pages, "recent PRs truncated");
                    let data = self.query(
                        RECENT_PULLS,
                        json!({"owner": owner, "name": name, "after": after}),
                    )?;
                    let pulls = &data["repository"]["pullRequests"];
                    let nodes = pulls["nodes"].as_array().context("recent PRs")?;
                    let recent: Vec<&Value> = nodes
                        .iter()
                        .filter(|p| millis(&p["updatedAt"]).is_some_and(|at| at >= self.lookback()))
                        .collect();
                    all.extend(recent.iter().copied().cloned());
                    if pulls["pageInfo"]["hasNextPage"] != true || recent.len() < nodes.len() {
                        break;
                    }
                    after = pulls["pageInfo"]["endCursor"].clone();
                }
            }
            Ok(Value::Array(all))
        })?;
        Ok(value.as_array().cloned().unwrap_or_default())
    }

    /// The open pull requests of the followed repositories, as REST lists them.
    fn rest_pulls(&self) -> Result<Vec<(String, Value)>> {
        let mut all = vec![];
        for repo in &self.s.follow {
            for p in self.cached_list(&format!(
                "/repos/{repo}/pulls?state=open&sort=updated&direction=desc"
            ))? {
                all.push((repo.clone(), p));
            }
        }
        Ok(all)
    }

    /// The scheduler's newest passes: the job's log of each completed poll
    /// run that ran it, colour codes taken out. A run cancelled while waiting
    /// for the previous pass ran no scheduler and is passed over.
    fn passes(&self, poll: Option<&Vec<Value>>) -> Result<Vec<Value>> {
        let poll = poll.context("poll runs unavailable")?;
        let value = self.once("passes", || {
            let mut result = vec![];
            let limits = &self.s.limits;
            for run in poll
                .iter()
                .filter(|r| r["status"] == "completed")
                .take(limits.idle_passes + 3)
            {
                let id = run["id"].as_u64().context("run id")?;
                let jobs = self.api.get(&format!(
                    "{}/actions/runs/{id}/jobs?per_page=100",
                    self.root
                ))?;
                let Some(job) = jobs["jobs"].as_array().and_then(|jobs| {
                    jobs.iter().find(|j| {
                        j["name"] == self.s.scheduler_job.as_str()
                            && j["status"] == "completed"
                            && (j["conclusion"] == "success" || j["conclusion"] == "failure")
                    })
                }) else {
                    continue;
                };
                let job = job["id"].as_u64().context("job id")?;
                let log = self.api.download(
                    &format!("{}/actions/jobs/{job}/logs", self.root),
                    limits.max_download_bytes,
                )?;
                let text = strip_colours(&String::from_utf8_lossy(&log));
                let lines: Vec<&str> = text
                    .split('\n')
                    .filter_map(|l| log_line(l.trim_end()))
                    .collect();
                result.push(json!({"html_url": run["html_url"], "lines": lines}));
                if result.len() == limits.idle_passes {
                    break;
                }
            }
            Ok(Value::Array(result))
        })?;
        Ok(value.as_array().cloned().unwrap_or_default())
    }

    /// dispatch.yml's runs started within the lookback, newest first: a
    /// hundred a day or more, so read in pages, at most maxPages of them.
    fn dispatch_runs(&self) -> Result<Vec<Value>> {
        let value = self.once("dispatch runs", || {
            let since = encode(&format!(">={}", iso(self.lookback())));
            let mut all = vec![];
            for page in 1.. {
                ensure!(page <= self.s.limits.max_pages, "dispatch runs truncated");
                let runs = self.list(
                    &format!(
                        "{}/actions/workflows/{}/runs?created={since}&page={page}",
                        self.root, self.s.dispatch_workflow
                    ),
                    true,
                )?;
                let full = runs.len() as u64 >= self.s.limits.page_size;
                all.extend(runs);
                if !full {
                    break;
                }
            }
            Ok(Value::Array(all))
        })?;
        Ok(value.as_array().cloned().unwrap_or_default())
    }

    fn removals(pulls: &[Value]) -> Vec<Value> {
        // A pull request the queue merged is removed from it too, for that reason.
        pulls
            .iter()
            .flat_map(|p| {
                p["timelineItems"]["nodes"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|e| !e["reason"].as_str().unwrap_or_default().eq_ignore_ascii_case("merged"))
                    .map(|e| json!({"event": "removed_from_merge_queue", "created_at": e["createdAt"], "html_url": p["url"]}))
            })
            .collect()
    }

    fn epics(&self) -> Result<Vec<Value>> {
        let repo = &self.s.epics_repository;
        let mut open: Vec<Value> = self
            .paged(
                &format!("/repos/{repo}/issues?state=open&labels=epic"),
                REST_PAGES,
            )?
            .into_iter()
            .filter(|i| i.get("pull_request").is_none_or(Value::is_null))
            .collect();
        ensure!(open.len() <= MAX_EPICS, "incomplete census");
        open.sort_by_key(|i| i["number"].as_u64());
        let mut result = vec![];
        for epic in open {
            let number = epic["number"].as_u64().context("epic number")?;
            // Live only if the operator applied the label, as the newest
            // labeled or unlabeled event for it says: anyone who can triage
            // can label an issue, and the coordinator plans only from live
            // epics. The census's board read is left out: a checkpoint does
            // not depend on it.
            let timeline = self.paged(
                &format!("/repos/{repo}/issues/{number}/timeline"),
                REST_PAGES,
            )?;
            let last = timeline.iter().rfind(|e| {
                (e["event"] == "labeled" || e["event"] == "unlabeled")
                    && e["label"]["name"]
                        .as_str()
                        .is_some_and(|n| n.eq_ignore_ascii_case("epic"))
            });
            let live = last.is_some_and(|e| {
                e["event"] == "labeled"
                    && e["actor"]["login"]
                        .as_str()
                        .is_some_and(|l| l.eq_ignore_ascii_case(&self.s.operator))
            });
            if !live {
                continue;
            }
            let state = checkpoint(&self.comments(repo, number)?, &self.s.bot)?;
            let mut item = state.clone().unwrap_or_else(|| json!({}));
            item["missing"] = state.is_none().into();
            item["html_url"] = format!("https://github.com/{repo}/issues/{number}").into();
            result.push(item);
        }
        Ok(result)
    }

    fn approved(&self) -> Result<Vec<Value>> {
        let mut result = vec![];
        for (repo, p) in self.rest_pulls()? {
            let number = p["number"].as_u64().context("pull number")?;
            let reviews = self.cached_list(&format!("/repos/{repo}/pulls/{number}/reviews"))?;
            // COMMENTED does not revoke approval. A newer request for changes does.
            let mut latest: BTreeMap<&str, &Value> = BTreeMap::new();
            for r in &reviews {
                let Some(user) = r["user"]["login"].as_str() else {
                    continue;
                };
                if [self.s.operator.as_str(), self.s.bot.as_str()].contains(&user)
                    && ["APPROVED", "CHANGES_REQUESTED", "DISMISSED"]
                        .iter()
                        .any(|s| r["state"] == *s)
                {
                    latest.insert(user, r);
                }
            }
            let approval = latest
                .values()
                .find(|r| r["state"] == "APPROVED" && r["commit_id"] == p["head"]["sha"]);
            result.push(json!({
                "html_url": p["html_url"],
                "approvedAt": approval.map_or(Value::Null, |r| r["submitted_at"].clone()),
            }));
        }
        Ok(result)
    }

    /// Temporal correlation is evidence to inspect, not proof of causation.
    /// Never classify an outage from issue or log prose someone can edit.
    fn infrastructure(&self, broker: Option<&Vec<Value>>) -> Result<Vec<Value>> {
        let broker = broker.context("probe unavailable")?;
        let mut result = vec![];
        for (repo, p) in self.rest_pulls()? {
            let held = p["labels"]
                .as_array()
                .is_some_and(|l| l.iter().any(|l| l["name"] == HOLD));
            if !held {
                continue;
            }
            let number = p["number"].as_u64().context("pull number")?;
            let events = self.cached_list(&format!("/repos/{repo}/issues/{number}/timeline"))?;
            let infrastructure = events.iter().any(|e| {
                let Some(at) = millis(&e["created_at"]) else {
                    return false;
                };
                e["event"] == "labeled"
                    && e["label"]["name"] == HOLD
                    && broker.iter().any(|r| {
                        r["conclusion"] == "failure"
                            && millis(&r["created_at"]).is_some_and(|c| c <= at)
                            && millis(&r["updated_at"]).is_some_and(|u| u >= at)
                    })
            });
            result.push(json!({"html_url": p["html_url"], "infrastructure": infrastructure}));
        }
        Ok(result)
    }

    fn dispatch(&self) -> Result<Vec<Value>> {
        let runs = self.list(
            &format!(
                "{}/actions/workflows/{}/runs?status=in_progress",
                self.root, self.s.dispatch_workflow
            ),
            false,
        )?;
        let mut result = vec![];
        for run in runs {
            let Some(("implement", repo, number)) =
                run_title(run["display_title"].as_str().unwrap_or_default())
            else {
                continue;
            };
            if !self.s.schedule_repositories.iter().any(|r| r == repo) {
                continue;
            }
            let issue = self.api.get(&format!("/repos/{repo}/issues/{number}"))?;
            let subs = self.list(&format!("/repos/{repo}/issues/{number}/sub_issues"), false)?;
            let timeline = self.list(&format!("/repos/{repo}/issues/{number}/timeline"), false)?;
            let labelled = issue["labels"].as_array().is_some_and(|l| {
                l.iter()
                    .any(|l| l["name"] == "epic" || l["name"] == "question")
            });
            let has_pr = timeline.iter().any(|e| {
                truthy(&e["source"]["issue"]["pull_request"])
                    && e["source"]["issue"]["state"] == "open"
            });
            result.push(json!({
                "state": issue["state"],
                "umbrella": !subs.is_empty() || labelled,
                "hasPR": has_pr,
                "html_url": run["html_url"],
            }));
        }
        Ok(result)
    }

    fn decisions(&self) -> Result<Vec<Value>> {
        let repo = &self.s.epics_repository;
        let issues = self.list(
            &format!(
                "/repos/{repo}/issues?state=open&labels=question&assignee={}",
                encode(&self.s.operator)
            ),
            false,
        )?;
        let mut unanswered = vec![];
        for issue in issues
            .iter()
            .filter(|i| i.get("pull_request").is_none_or(Value::is_null))
        {
            let number = issue["number"].as_u64().context("issue number")?;
            if own_comments(&self.comments(repo, number)?, &self.s.operator).is_empty() {
                unanswered.push(
                    json!({"created_at": issue["created_at"], "html_url": issue["html_url"]}),
                );
            }
        }
        Ok(unanswered)
    }

    fn release(&self) -> Result<Vec<Value>> {
        let s = self.s;
        let latest = self
            .api
            .get(&format!("/repos/{}/releases/latest", s.release_repository))?;
        let file = self.api.get(&format!(
            "/repos/{}/contents/{}",
            s.wrapper_repository, s.wrapper_path
        ))?;
        let text = base64_decode(file["content"].as_str().context("wrapper content")?)?;
        let pins = wrapper_pins(&String::from_utf8_lossy(&text), &s.release_repository);
        ensure!(
            pins.len() == 3 && pins.iter().all(|p| *p == pins[0]),
            "unrecognized wrapper pin"
        );
        Ok(vec![
            json!({"pin": pins[0], "latest": latest["tag_name"], "html_url": latest["html_url"]}),
        ])
    }

    /// A recorded plan's run, as the scheduler finds it: the run the bot's
    /// record links last; its pull request is the branch agentic-job names
    /// after it.
    fn stuck(&self, poll: Option<&Vec<Value>>) -> Result<Vec<Value>> {
        let passes = self.passes(poll)?;
        let pass = passes.first().context("no scheduler pass")?;
        let lines: Vec<&str> = pass["lines"]
            .as_array()
            .map(|l| l.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let skips: Vec<(&str, u64)> = lines.iter().filter_map(|l| skip_line(l)).collect();
        ensure!(skips.len() <= self.s.limits.max_items, "too many skips");
        let mut result = vec![];
        let record = format!("<!-- {DISPATCH_MARKER}: ");
        for (repo, id) in skips {
            let comment = self
                .api
                .get(&format!("/repos/{repo}/issues/comments/{id}"))?;
            let body = comment["body"].as_str().unwrap_or_default();
            if comment["user"]["login"] != self.s.bot.as_str() || !body.starts_with(&record) {
                continue;
            }
            let Some(run_id) = last_run_link(body, &self.s.dispatch_repo) else {
                continue;
            };
            let run = self.cached_get(&format!("{}/actions/runs/{run_id}", self.root))?;
            let Some(("implement", target, _)) =
                run_title(run["display_title"].as_str().unwrap_or_default())
            else {
                continue;
            };
            let ended = run["status"] == "completed";
            let pull = if ended {
                let owner = target.split('/').next().unwrap_or_default();
                let head = encode(&format!(
                    "{owner}:{}{run_id}",
                    self.s.implement_branch_prefix
                ));
                let pulls = self
                    .api
                    .get(&format!("/repos/{target}/pulls?state=all&head={head}"))?;
                let first = pulls.as_array().and_then(|p| p.first()).cloned();
                match first {
                    None => "none".into(),
                    Some(p) if truthy(&p["merged_at"]) => "merged".into(),
                    Some(p) => p["state"].clone(),
                }
            } else {
                Value::Null
            };
            result.push(json!({"ended": ended, "pull": pull, "html_url": comment["html_url"], "run_url": run["html_url"]}));
        }
        Ok(result)
    }

    /// The usage snapshot from the private repository its config names.
    /// Only links reach the report, never the numbers.
    fn pace(&self) -> Result<Vec<Value>> {
        let usage = self
            .s
            .usage
            .as_ref()
            .context("no usage snapshot configured")?;
        let comments = self.list(
            &format!(
                "/repos/{}/issues/{}/comments",
                usage.issue.repository, usage.issue.issue
            ),
            true,
        )?;
        let comment = comments.iter().find(|c| {
            c["user"]["login"] == usage.bot.as_str()
                && c["body"]
                    .as_str()
                    .is_some_and(|b| b.starts_with(USAGE_MARKER))
        });
        let snap = comment.and_then(|c| usage_snapshot(c["body"].as_str()?));
        let fresh = self.now - (self.s.limits.usage_stale_minutes * 60_000.0) as i64;
        let Some(snap) = snap.filter(|s| millis(&s["updated_at"]).is_some_and(|at| at >= fresh))
        else {
            bail!("no fresh usage snapshot");
        };
        let url = comment.map_or(Value::Null, |c| c["html_url"].clone());
        Ok(snap["pools"]
            .as_object()
            .map(|pools| {
                pools
                    .values()
                    .filter(|p| truthy(p))
                    .map(|p| json!({"used_percent": p["used_percent"], "window_minutes": p["window_minutes"], "resets_at": p["resets_at"], "html_url": url}))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// The newest run of each kind and target that failed, when its failed
    /// jobs' annotations say why in a way that recurs on a run started again
    /// unchanged. A newer run of the same kind and target supersedes it.
    fn failures(&self) -> Result<Vec<Value>> {
        let runs = self.dispatch_runs()?;
        let mut seen = std::collections::HashSet::new();
        let mut failed = vec![];
        for run in &runs {
            // By kind and target: the run name has a pool suffix only when one was given.
            let Some((kind, repo, number)) =
                run_title(run["display_title"].as_str().unwrap_or_default())
            else {
                continue;
            };
            if seen.insert(format!("{kind} {repo}#{number}")) && run["conclusion"] == "failure" {
                failed.push((run, repo, number));
            }
        }
        ensure!(
            failed.len() <= self.s.limits.max_items,
            "too many failed runs"
        );
        let mut result = vec![];
        for (run, repo, number) in failed {
            let id = run["id"].as_u64().context("run id")?;
            let jobs = self.api.get(&format!(
                "{}/actions/runs/{id}/jobs?per_page=100",
                self.root
            ))?;
            let mut messages = vec![];
            for job in jobs["jobs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|j| j["conclusion"] == "failure")
            {
                let job = job["id"].as_u64().context("job id")?;
                for a in self.list(
                    &format!("{}/check-runs/{job}/annotations", self.root),
                    false,
                )? {
                    messages.push(match &a["message"] {
                        Value::String(m) => m.clone(),
                        other => other.to_string(),
                    });
                }
            }
            let text = messages.join("\n");
            let reason = if text.to_ascii_lowercase().contains("secret-shaped") {
                Some("secret")
            } else if refusal_code(&text) {
                Some("refused")
            } else if text.contains("required data was missing")
                && self.closed_blocker(run, id, repo, number)?
            {
                Some("blocker")
            } else {
                None
            };
            result.push(json!({"reason": reason, "html_url": run["html_url"]}));
        }
        Ok(result)
    }

    /// Whether the reason of a run that handed back missing data names an
    /// issue or pull request in a followed repository that is closed now:
    /// the agentic-job status comment for the run, by who could have written it.
    fn closed_blocker(&self, run: &Value, id: u64, repo: &str, number: u64) -> Result<bool> {
        let since = encode(run["created_at"].as_str().unwrap_or_default());
        let comments = self.list(
            &format!("/repos/{repo}/issues/{number}/comments?since={since}"),
            false,
        )?;
        let end = status_end(id);
        let status = comments.iter().find(|c| {
            (c["user"]["login"] == self.s.bot.as_str()
                || c["user"]["login"] == "github-actions[bot]")
                && c["body"].as_str().is_some_and(|b| b.ends_with(&end))
        });
        let body = status.and_then(|c| c["body"].as_str()).unwrap_or_default();
        let Some(cut) = body.find(STATUS_REASON) else {
            return Ok(false);
        };
        let named: Vec<(String, u64)> = item_links(&body[cut..])
            .into_iter()
            .filter(|(r, n)| self.s.follow.contains(r) && !(r == repo && *n == number))
            .take(3)
            .collect();
        for (r, n) in named {
            if self.cached_get(&format!("/repos/{r}/issues/{n}"))?["state"] == "closed" {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn held(&self) -> Result<Vec<Value>> {
        Ok(self
            .open_pulls()?
            .iter()
            .filter(|p| {
                login(&p["author"]) == self.s.bot
                    && p["labels"]["nodes"]
                        .as_array()
                        .is_some_and(|l| l.iter().any(|l| l["name"] == HOLD))
            })
            .map(|p| {
                let labelled = p["timelineItems"]["nodes"]
                    .as_array()
                    .and_then(|e| e.iter().rfind(|e| e["label"]["name"] == HOLD))
                    .map_or(Value::Null, |e| e["createdAt"].clone());
                let comments = p["comments"]["nodes"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let reason = comments
                    .iter()
                    .rfind(|c| c["author"]["login"] == self.s.bot.as_str())
                    .or(comments.last())
                    .map_or(Value::Null, |c| c["url"].clone());
                json!({"labeledAt": labelled, "html_url": p["url"], "reason_url": reason})
            })
            .collect())
    }

    fn unqueued(&self, approved: Option<&Vec<Value>>) -> Result<Vec<Value>> {
        let approved = approved.context("approvals unavailable")?;
        let open: HashMap<String, Value> = self
            .open_pulls()?
            .into_iter()
            .filter_map(|p| Some((p["url"].as_str()?.to_owned(), p)))
            .collect();
        Ok(approved
            .iter()
            .filter_map(|p| {
                let pull = open.get(p["html_url"].as_str()?)?;
                truthy(&p["approvedAt"]).then(|| {
                    json!({
                        "approvedAt": p["approvedAt"],
                        "clean": pull["mergeStateStatus"] == "CLEAN",
                        "queued": pull["isInMergeQueue"],
                        "html_url": p["html_url"],
                    })
                })
            })
            .collect())
    }

    /// The bot's implement pull requests merged within the lookback, by the
    /// branch agentic-job names after the run, whose run names the issue.
    /// One that says it is part of the issue may leave it open; the
    /// scheduler asks a triage run about those.
    fn unclosed(&self) -> Result<Vec<Value>> {
        let prefix = &self.s.implement_branch_prefix;
        let run_of = |head: &str| {
            head.strip_prefix(prefix.as_str())
                .filter(|id| digits(id))
                .map(str::to_owned)
        };
        let merged: Vec<Value> = self
            .recent_pulls()?
            .into_iter()
            .filter(|p| {
                login(&p["author"]) == self.s.bot
                    && truthy(&p["mergedAt"])
                    && p["headRefName"].as_str().and_then(run_of).is_some()
                    && !says_part_of(p["body"].as_str().unwrap_or_default())
            })
            .collect();
        let runs: HashMap<String, Value> = self
            .dispatch_runs()?
            .into_iter()
            .filter_map(|r| Some((r["id"].as_u64()?.to_string(), r)))
            .collect();
        let mut found = vec![];
        for p in merged {
            let id = p["headRefName"]
                .as_str()
                .and_then(run_of)
                .unwrap_or_default();
            // A run started before the lookback is read by itself.
            let run = match runs.get(&id) {
                Some(run) => run.clone(),
                None => self.cached_get(&format!("{}/actions/runs/{id}", self.root))?,
            };
            if let Some(("implement", repo, number)) =
                run_title(run["display_title"].as_str().unwrap_or_default())
            {
                found.push((p, repo.to_owned(), number));
            }
        }
        // The issues' states, fifty to a query rather than a request each.
        let mut result = vec![];
        for chunk in found.chunks(50) {
            let params: Vec<String> = (0..chunk.len())
                .map(|k| format!("$o{k}: String!, $n{k}: String!, $i{k}: Int!"))
                .collect();
            let fields: String = (0..chunk.len())
                .map(|k| format!(" r{k}: repository(owner: $o{k}, name: $n{k}) {{ issue(number: $i{k}) {{ state url }} }}"))
                .collect();
            let mut variables = serde_json::Map::new();
            for (k, (_, repo, number)) in chunk.iter().enumerate() {
                let (owner, name) = repo.split_once('/').context("repository")?;
                variables.insert(format!("o{k}"), owner.into());
                variables.insert(format!("n{k}"), name.into());
                variables.insert(format!("i{k}"), (*number).into());
            }
            let data = self.query(
                &format!("query({}) {{{} }}", params.join(", "), fields),
                Value::Object(variables),
            )?;
            for (k, (p, _, _)) in chunk.iter().enumerate() {
                let issue = &data[format!("r{k}")]["issue"];
                if issue.is_object() {
                    result.push(json!({
                        "mergedAt": p["mergedAt"],
                        "issueState": issue["state"].as_str().unwrap_or_default().to_ascii_lowercase(),
                        "html_url": p["url"],
                        "issue_url": issue["url"],
                    }));
                }
            }
        }
        Ok(result)
    }

    /// Each workflow's runs started by `schedule` within the lookback, in the
    /// followed repositories and the dispatch repository, read in pages of at
    /// most maxPages a repository. GitHub runs a schedule only on the default
    /// branch, so the event alone selects it. The report's links are built
    /// from the configured repository, the run's numeric id and a checked
    /// workflow file name, never from a run's title.
    fn scheduled(&self) -> Result<Vec<Value>> {
        let since = encode(&format!(">={}", iso(self.lookback())));
        let repos: BTreeSet<&String> = self
            .s
            .follow
            .iter()
            .chain([&self.s.dispatch_repo])
            .collect();
        let mut result = vec![];
        for repo in repos {
            // By workflow id: runs ended, runs failed, newest failed run, file.
            let mut workflows: BTreeMap<u64, (u64, u64, Option<u64>, Option<String>)> =
                BTreeMap::new();
            for page in 1.. {
                ensure!(page <= self.s.limits.max_pages, "scheduled runs truncated");
                let runs = self.list(
                    &format!(
                        "/repos/{repo}/actions/runs?event=schedule&created={since}&page={page}"
                    ),
                    true,
                )?;
                let full = runs.len() as u64 >= self.s.limits.page_size;
                for run in &runs {
                    let (Some(workflow), Some(id)) =
                        (run["workflow_id"].as_u64(), run["id"].as_u64())
                    else {
                        continue;
                    };
                    let entry = workflows.entry(workflow).or_default();
                    if entry.3.is_none() {
                        entry.3 = run["path"]
                            .as_str()
                            .and_then(|p| p.strip_prefix(".github/workflows/"))
                            .filter(|f| {
                                !f.is_empty()
                                    && f.bytes()
                                        .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
                            })
                            .map(str::to_owned);
                    }
                    if run["status"] != "completed"
                        || run["conclusion"] == "cancelled"
                        || run["conclusion"] == "skipped"
                    {
                        continue;
                    }
                    entry.0 += 1;
                    if FAILED.iter().any(|c| run["conclusion"] == *c) {
                        entry.1 += 1;
                        entry.2 = entry.2.max(Some(id));
                    }
                }
                if !full {
                    break;
                }
            }
            for (ended, failed, newest, file) in workflows.into_values() {
                let page = match file {
                    Some(file) => format!("https://github.com/{repo}/actions/workflows/{file}"),
                    None => format!("https://github.com/{repo}/actions"),
                };
                let run = newest.map(|id| format!("https://github.com/{repo}/actions/runs/{id}"));
                result.push(
                    json!({"runs": ended, "failed": failed, "html_url": page, "run_url": run}),
                );
            }
        }
        Ok(result)
    }
}

/// Reads every observation. A read that fails, or that would be incomplete,
/// leaves its observation unavailable rather than failing the pass. `api`
/// is expected to be [`super::Bounded`]; `branch` is the caller's default
/// branch, whose poll and probe runs count.
pub fn collect(
    api: &dyn Api,
    s: &Settings,
    branch: &str,
    now: i64,
    previous: Option<Vec<Value>>,
) -> Snapshot {
    let c = Collector {
        api,
        s,
        branch: encode(branch),
        now,
        root: format!("/repos/{}", s.dispatch_repo),
        cache: RefCell::new(HashMap::new()),
    };
    let mut snapshot = Snapshot {
        observations: BTreeMap::new(),
        previous,
    };
    let mut capture = |id: &str, read: &dyn Fn(&Observations) -> Result<Vec<Value>>| {
        let items = read(&snapshot.observations).ok();
        snapshot.observations.insert(id.to_owned(), items);
    };
    let root = &c.root;
    let got = |o: &Observations, id: &str| o.get(id).cloned().flatten();
    capture("poll", &|_| {
        c.list(
            &format!(
                "{root}/actions/workflows/{}/runs?branch={}",
                s.poll_workflow, c.branch
            ),
            true,
        )
    });
    capture("runs", &|_| {
        let mut all = vec![];
        for status in RUNNING {
            all.extend(c.list(&format!("{root}/actions/runs?status={status}"), false)?);
        }
        Ok(all)
    });
    capture("broker", &|_| {
        c.list(
            &format!(
                "{root}/actions/workflows/{}/runs?branch={}",
                s.probe_workflow, c.branch
            ),
            true,
        )
    });
    capture("epics", &|_| c.epics());
    capture("queue", &|_| Ok(Collector::removals(&c.recent_pulls()?)));
    capture("dependabot", &|_| {
        let pulls: Vec<Value> = c
            .recent_pulls()?
            .into_iter()
            .filter(|p| s.merge_authors.contains(&login(&p["author"])))
            .collect();
        Ok(Collector::removals(&pulls))
    });
    capture("approved", &|_| c.approved());
    capture("infrastructure", &|o| {
        c.infrastructure(got(o, "broker").as_ref())
    });
    capture("dispatch", &|_| c.dispatch());
    capture("decisions", &|_| c.decisions());
    capture("release", &|_| c.release());
    capture("idle", &|o| {
        Ok(c.passes(got(o, "poll").as_ref())?
            .iter()
            .map(|p| {
                let idle = p["lines"]
                    .as_array()
                    .is_some_and(|l| l.iter().filter_map(Value::as_str).any(idle_line));
                json!({"idle": idle, "html_url": p["html_url"]})
            })
            .collect())
    });
    capture("stuck", &|o| c.stuck(got(o, "poll").as_ref()));
    capture("pace", &|_| c.pace());
    capture("failures", &|_| c.failures());
    capture("held", &|_| c.held());
    capture("unqueued", &|o| c.unqueued(got(o, "approved").as_ref()));
    capture("unclosed", &|_| c.unclosed());
    capture("scheduled", &|_| c.scheduled());
    // From the settings already read: no request.
    capture("unset", &|_| {
        Ok(s.unset.iter().map(|o| json!({"option": o})).collect())
    });
    snapshot
}

#[cfg(test)]
mod tests;
