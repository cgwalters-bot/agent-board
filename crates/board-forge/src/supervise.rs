//! The controller's supervisor, moved from tracker's `.github/supervisor`: a
//! read-only health pass over the poll, scheduler and dispatch runs
//! ([`health`]), whether an agent reads its report ([`agent::decide`]), and
//! an executor that performs only configured, bounded request kinds
//! ([`agent::execute`]). Its settings are the poll's agent-board.json plus
//! the supervisor's own file, both from the caller's default branch.
use crate::github::{API_ROOT, Client, Transport};
use crate::poll::{Rest, validate_repository, validate_workflow};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::cell::Cell;
use std::collections::BTreeMap;

pub mod agent;
pub mod health;

const HOUR_MS: i64 = 3_600_000;

/// What the supervisor reads and, in the executor only, writes. Paths are
/// API paths such as `/repos/O/R/issues/1`.
pub trait Api {
    fn get(&self, path: &str) -> Result<Value>;
    fn graphql(&self, query: &str, variables: Value) -> Result<Value>;
    /// The body of a GET the API redirects off itself (a job log), bounded.
    fn download(&self, path: &str, limit: u64) -> Result<Vec<u8>>;
    /// A write; anything but a 2xx status is an error naming only the
    /// status, as a response body could echo text into the public log.
    fn send(&self, method: &str, path: &str, body: &Value) -> Result<Value>;
}

fn api_url(path: &str) -> String {
    format!("{API_ROOT}{}", path.trim_start_matches('/'))
}

impl Api for Client {
    fn get(&self, path: &str) -> Result<Value> {
        Ok(Rest::get(self, &api_url(path), None)?.body)
    }

    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        Transport::graphql(self, query, variables)
    }

    fn download(&self, path: &str, limit: u64) -> Result<Vec<u8>> {
        Client::download(self, &api_url(path), limit)
    }

    fn send(&self, method: &str, path: &str, body: &Value) -> Result<Value> {
        let (status, body) = Rest::send(self, method, &api_url(path), body)?;
        let path = path.split('?').next().unwrap_or_default();
        ensure!(
            (200..300).contains(&status),
            "{method} {path}: HTTP {status}"
        );
        Ok(body)
    }
}

/// The health pass's bounds: a census too large or an API call that stalls
/// must not exhaust the runner or the token's rate limit. It writes nothing;
/// a write or a GraphQL mutation through it is refused.
pub struct Bounded<'a> {
    api: &'a dyn Api,
    remaining: Cell<u32>,
    deadline_ms: i64,
    clock: &'a dyn Fn() -> i64,
}

impl<'a> Bounded<'a> {
    pub fn new(api: &'a dyn Api, limits: &Limits, clock: &'a dyn Fn() -> i64) -> Self {
        Self {
            api,
            remaining: Cell::new(limits.max_requests),
            deadline_ms: clock() + limits.collection_timeout_seconds as i64 * 1000,
            clock,
        }
    }

    fn spend(&self) -> Result<()> {
        let remaining = self.remaining.get();
        ensure!(
            remaining > 0 && (self.clock)() < self.deadline_ms,
            "collection budget exhausted"
        );
        self.remaining.set(remaining - 1);
        Ok(())
    }
}

impl Api for Bounded<'_> {
    fn get(&self, path: &str) -> Result<Value> {
        self.spend()?;
        self.api.get(path)
    }

    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        ensure!(
            !query.trim_start().starts_with("mutation"),
            "the health pass sends no mutation"
        );
        self.spend()?;
        self.api.graphql(query, variables)
    }

    fn download(&self, path: &str, limit: u64) -> Result<Vec<u8>> {
        self.spend()?;
        self.api.download(path, limit)
    }

    fn send(&self, method: &str, path: &str, _: &Value) -> Result<Value> {
        bail!("the health pass writes nothing ({method} {path})")
    }
}

/// Thresholds and bounds, from the supervisor's file; each defaults to the
/// value tracker's config.json had when the supervisor moved here.
#[derive(Clone, Debug)]
pub struct Limits {
    pub page_size: u64,
    pub max_requests: u32,
    pub collection_timeout_seconds: u64,
    pub poll_failures: usize,
    pub run_timeout_minutes: f64,
    pub probe_stale_minutes: f64,
    pub epic_stale_hours: f64,
    pub epic_failures: f64,
    pub approved_wait_hours: f64,
    pub decision_wait_hours: f64,
    pub event_lookback_hours: f64,
    pub max_pages: usize,
    pub max_items: usize,
    pub max_evidence: usize,
    pub max_download_bytes: u64,
    pub unavailable_fail_hours: f64,
    pub idle_passes: usize,
    pub hold_wait_hours: f64,
    pub unqueued_wait_hours: f64,
    pub unclosed_wait_hours: f64,
    pub pace_margin_percent: f64,
    pub usage_stale_minutes: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            page_size: 100,
            max_requests: 500,
            collection_timeout_seconds: 420,
            poll_failures: 3,
            run_timeout_minutes: 360.0,
            probe_stale_minutes: 45.0,
            epic_stale_hours: 48.0,
            epic_failures: 3.0,
            approved_wait_hours: 2.0,
            decision_wait_hours: 24.0,
            event_lookback_hours: 24.0,
            max_pages: 6,
            max_items: 25,
            max_evidence: 10,
            max_download_bytes: 8 * 1024 * 1024,
            unavailable_fail_hours: 1.0,
            idle_passes: 2,
            hold_wait_hours: 12.0,
            unqueued_wait_hours: 1.0,
            unclosed_wait_hours: 1.0,
            pace_margin_percent: 25.0,
            usage_stale_minutes: 45.0,
        }
    }
}

impl Limits {
    fn from_json(value: &Value) -> Result<Self> {
        let d = Self::default();
        let count = |key: &str, max: u64, default: u64| -> Result<u64> {
            value.get(key).map_or(Ok(default), |v| {
                v.as_u64()
                    .filter(|n| (1..=max).contains(n))
                    .with_context(|| format!("supervisor config {key}: 1 to {max}"))
            })
        };
        let small = |key: &str, default: usize| -> Result<usize> {
            Ok(count(key, 10_000, default as u64)? as usize)
        };
        let number = |key: &str, default: f64| -> Result<f64> {
            value.get(key).map_or(Ok(default), |v| {
                v.as_f64()
                    .filter(|n| n.is_finite() && *n >= 0.0)
                    .with_context(|| format!("supervisor config {key}: a number"))
            })
        };
        Ok(Self {
            page_size: count("pageSize", 100, d.page_size)?,
            max_requests: count("maxRequests", 10_000, u64::from(d.max_requests))? as u32,
            collection_timeout_seconds: count(
                "collectionTimeoutSeconds",
                3600,
                d.collection_timeout_seconds,
            )?,
            poll_failures: small("pollFailures", d.poll_failures)?,
            run_timeout_minutes: number("runTimeoutMinutes", d.run_timeout_minutes)?,
            probe_stale_minutes: number("probeStaleMinutes", d.probe_stale_minutes)?,
            epic_stale_hours: number("epicStaleHours", d.epic_stale_hours)?,
            epic_failures: number("epicFailures", d.epic_failures)?,
            approved_wait_hours: number("approvedWaitHours", d.approved_wait_hours)?,
            decision_wait_hours: number("decisionWaitHours", d.decision_wait_hours)?,
            event_lookback_hours: number("eventLookbackHours", d.event_lookback_hours)?,
            max_pages: small("maxPages", d.max_pages)?,
            max_items: small("maxItems", d.max_items)?,
            max_evidence: small("maxEvidence", d.max_evidence)?,
            max_download_bytes: count("maxDownloadBytes", 64 << 20, d.max_download_bytes)?,
            unavailable_fail_hours: number("unavailableFailHours", d.unavailable_fail_hours)?,
            idle_passes: small("idlePasses", d.idle_passes)?,
            hold_wait_hours: number("holdWaitHours", d.hold_wait_hours)?,
            unqueued_wait_hours: number("unqueuedWaitHours", d.unqueued_wait_hours)?,
            unclosed_wait_hours: number("unclosedWaitHours", d.unclosed_wait_hours)?,
            pace_margin_percent: number("paceMarginPercent", d.pace_margin_percent)?,
            usage_stale_minutes: number("usageStaleMinutes", d.usage_stale_minutes)?,
        })
    }
}

/// An issue as `OWNER/REPO` and number, from configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct IssueRef {
    pub repository: String,
    pub issue: u64,
}

impl IssueRef {
    fn from_json(value: Option<&Value>, what: &str) -> Result<Option<Self>> {
        let Some(value) = value.filter(|v| !v.is_null()) else {
            return Ok(None);
        };
        let repository = value["repository"]
            .as_str()
            .or_else(|| value["repo"].as_str())
            .with_context(|| format!("{what}: repository"))?
            .to_owned();
        validate_repository(&repository).with_context(|| what.to_owned())?;
        let issue = value["issue"]
            .as_u64()
            .filter(|n| *n > 0)
            .with_context(|| format!("{what}: issue"))?;
        Ok(Some(Self { repository, issue }))
    }
}

/// The usage snapshot the pace check reads (tracker's usage/config.json).
#[derive(Clone, Debug)]
pub struct Usage {
    pub issue: IssueRef,
    pub bot: String,
}

/// The agent and the executor; absent, neither runs.
#[derive(Clone, Debug)]
pub struct AgentSettings {
    /// The pool, of the pools file, that the agent's run uses.
    pub pool: String,
    pub cooldown_hours: f64,
    /// Where the agent's one comment, its requests, is posted.
    pub requests: IssueRef,
    /// The operator's log, where the executor records every pass.
    pub log: IssueRef,
    pub kinds: Vec<String>,
    /// Dispatches admitted in one pass and in a day; zero, or left out,
    /// admits none.
    pub max_dispatches_per_pass: u64,
    pub max_dispatches_per_day: u64,
    /// Judgment writes are disabled unless their daily cap is configured.
    pub daily_caps: BTreeMap<String, u64>,
    pub state: Option<IssueRef>,
    pub tracker: Option<String>,
    pub project: Option<String>,
}

/// Everything the supervisor is configured with. None of it may come from a
/// workflow input: account and repository names are the caller's files.
#[derive(Clone, Debug)]
pub struct Settings {
    pub operator: String,
    pub bot: String,
    pub dispatch_repo: String,
    pub dispatch_workflow: String,
    /// Repositories whose pull requests the poll follows; an agent may ask
    /// for a dispatch on an issue in one of them and nowhere else.
    pub follow: Vec<String>,
    /// The scheduler's repositories, whose implement runs the dispatch check reads.
    pub schedule_repositories: Vec<String>,
    pub epics_repository: String,
    pub merge_authors: Vec<String>,
    /// A kind with a configured pool runs only there (agent-board.json's kindPools).
    pub kind_pools: BTreeMap<String, String>,
    pub limits: Limits,
    pub poll_workflow: String,
    pub probe_workflow: String,
    pub supervisor_workflow: String,
    pub scheduler_job: String,
    pub implement_branch_prefix: String,
    pub release_repository: String,
    pub wrapper_repository: String,
    pub wrapper_path: String,
    pub usage: Option<Usage>,
    pub agent: Option<AgentSettings>,
}

fn string(value: &Value, key: &str, what: &str) -> Result<String> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .with_context(|| format!("{what} missing {key}"))
}

fn strings(value: Option<&Value>, what: &str) -> Result<Vec<String>> {
    value
        .map(|v| serde_json::from_value(v.clone()).with_context(|| what.to_owned()))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn repositories(value: Option<&Value>, what: &str) -> Result<Vec<String>> {
    let list = strings(value, what)?;
    for repo in &list {
        validate_repository(repo).with_context(|| what.to_owned())?;
    }
    Ok(list)
}

/// A repository file path for the contents API: no query, no `..`.
fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= 200
            && !path.starts_with('/')
            && !path
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
            && path
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-/".contains(&c)),
        "invalid repository path {path:?}"
    );
    Ok(())
}

impl Settings {
    /// `board` is the poll's agent-board.json, `supervisor` the supervisor's
    /// own file and `usage` the usage snapshot's settings, if any.
    pub fn from_json(board: &Value, supervisor: &Value, usage: Option<&Value>) -> Result<Self> {
        let what = "agent-board config";
        let operator = string(board, "operator", what)?;
        let bot = string(board, "bot", what)?;
        let dispatch_repo = string(board, "dispatchRepo", what)?;
        let dispatch_workflow = string(board, "dispatchWorkflow", what)?;
        validate_repository(&dispatch_repo)?;
        validate_workflow(&dispatch_workflow)?;
        for login in [&operator, &bot] {
            ensure!(
                crate::is_github_login(login),
                "{what}: invalid login {login:?}"
            );
        }
        let epics_repository = board["epics"]["repository"]
            .as_str()
            .context("agent-board config missing epics.repository")?
            .to_owned();
        validate_repository(&epics_repository)?;
        let kind_pools = board
            .get("kindPools")
            .map(|v| serde_json::from_value(v.clone()).context("config kindPools"))
            .transpose()?
            .unwrap_or_default();

        let what = "supervisor config";
        let mut names = BTreeMap::new();
        for key in [
            "pollWorkflow",
            "probeWorkflow",
            "supervisorWorkflow",
            "schedulerJob",
            "implementBranchPrefix",
            "releaseRepository",
            "wrapperRepository",
            "wrapperPath",
        ] {
            names.insert(key, string(supervisor, key, what)?);
        }
        for key in ["pollWorkflow", "probeWorkflow", "supervisorWorkflow"] {
            validate_workflow(&names[key])?;
        }
        for key in ["releaseRepository", "wrapperRepository"] {
            validate_repository(&names[key])?;
        }
        validate_path(&names["wrapperPath"])?;
        // Compared with branch names and sent, encoded, as a query value.
        let prefix = &names["implementBranchPrefix"];
        ensure!(
            prefix.len() <= 100
                && prefix
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-/".contains(&c)),
            "{what}: invalid implementBranchPrefix"
        );
        let usage = usage
            .map(|u| -> Result<Usage> {
                let issue =
                    IssueRef::from_json(Some(u), "usage config")?.context("usage config")?;
                let bot = string(u, "bot", "usage config")?;
                ensure!(crate::is_github_login(&bot), "usage config: invalid bot");
                Ok(Usage { issue, bot })
            })
            .transpose()?;
        Ok(Self {
            follow: repositories(board.get("follow"), "config follow")?,
            schedule_repositories: repositories(
                board.pointer("/schedule/repositories"),
                "config schedule.repositories",
            )?,
            merge_authors: strings(board.get("mergeAuthors"), "config mergeAuthors")?,
            operator,
            bot,
            dispatch_repo,
            dispatch_workflow,
            epics_repository,
            kind_pools,
            limits: Limits::from_json(supervisor)?,
            poll_workflow: names["pollWorkflow"].clone(),
            probe_workflow: names["probeWorkflow"].clone(),
            supervisor_workflow: names["supervisorWorkflow"].clone(),
            scheduler_job: names["schedulerJob"].clone(),
            implement_branch_prefix: names["implementBranchPrefix"].clone(),
            release_repository: names["releaseRepository"].clone(),
            wrapper_repository: names["wrapperRepository"].clone(),
            wrapper_path: names["wrapperPath"].clone(),
            usage,
            agent: agent_settings(supervisor.get("agent"))?,
        })
    }
}

fn agent_settings(value: Option<&Value>) -> Result<Option<AgentSettings>> {
    let Some(agent) = value.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let (Some(requests), Some(log)) = (
        IssueRef::from_json(agent.get("requests"), "supervisor agent.requests")?,
        IssueRef::from_json(agent.get("log"), "supervisor agent.log")?,
    ) else {
        return Ok(None);
    };
    let cap = |key: &str| -> Result<u64> {
        agent
            .get(key)
            .map(|v| v.as_u64().filter(|n| *n <= 100))
            .unwrap_or(Some(0))
            .with_context(|| format!("supervisor agent.{key}: 0 to 100"))
    };
    let daily_caps: BTreeMap<String, u64> = agent
        .get("dailyCaps")
        .map(|v| serde_json::from_value(v.clone()).context("supervisor agent.dailyCaps"))
        .transpose()?
        .unwrap_or_default();
    for (kind, cap) in &daily_caps {
        ensure!(
            agent::judgment::ACTIONS.contains(&kind.as_str()) && *cap <= 100,
            "supervisor agent.dailyCaps: unknown action or cap over 100"
        );
        ensure!(
            kind != "status_update" || *cap <= 1,
            "status updates are capped at one per day"
        );
    }
    let tracker = agent
        .get("tracker")
        .map(|v| -> Result<String> {
            let repo = v.as_str().context("supervisor agent.tracker")?;
            validate_repository(repo)?;
            Ok(repo.to_owned())
        })
        .transpose()?;
    let project = agent
        .get("project")
        .map(|v| -> Result<String> {
            let id = v.as_str().context("supervisor agent.project")?;
            ensure!(
                !id.is_empty()
                    && id.len() <= 200
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
                "invalid supervisor project node ID"
            );
            Ok(id.to_owned())
        })
        .transpose()?;
    let state = IssueRef::from_json(agent.get("state"), "supervisor agent.state")?;
    if let Some(state) = &state {
        ensure!(
            [&requests, &log]
                .iter()
                .all(|i| !i.repository.eq_ignore_ascii_case(&state.repository)
                    || i.issue != state.issue),
            "state must differ from requests and log"
        );
    }
    Ok(Some(AgentSettings {
        pool: string(agent, "pool", "supervisor agent")?,
        cooldown_hours: agent["cooldownHours"]
            .as_f64()
            .filter(|h| h.is_finite() && *h > 0.0)
            .context("supervisor agent.cooldownHours: a positive number")?,
        requests,
        log,
        kinds: strings(agent.get("kinds"), "supervisor agent.kinds")?,
        max_dispatches_per_pass: cap("maxDispatchesPerPass")?,
        max_dispatches_per_day: cap("maxDispatchesPerDay")?,
        daily_caps,
        state,
        tracker,
        project,
    }))
}

/// Milliseconds since the epoch of an RFC 3339 time, as `Date.parse` was used.
pub(crate) fn millis(value: &Value) -> Option<i64> {
    let parsed = time::OffsetDateTime::parse(
        value.as_str()?,
        &time::format_description::well_known::Rfc3339,
    )
    .ok()?;
    i64::try_from(parsed.unix_timestamp_nanos() / 1_000_000).ok()
}

/// `2026-10-15T10:00:00.000Z`, as `toISOString` wrote the report's times.
pub(crate) fn iso(ms: i64) -> String {
    let at = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
        at.millisecond()
    )
}

pub fn now_millis(now: time::OffsetDateTime) -> i64 {
    i64::try_from(now.unix_timestamp_nanos() / 1_000_000).unwrap_or_default()
}

/// A query-string value.
pub(crate) fn encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// `kind OWNER/REPO#N`, the head of agentic-job's run names
/// (`implement o/r#5 (claude)`): the kind, repository and number.
pub(crate) fn run_title(title: &str) -> Option<(&str, &str, u64)> {
    let (kind, rest) = title.split_once(' ')?;
    if !["implement", "triage", "research", "review", "fix"].contains(&kind) {
        return None;
    }
    let target = rest.split(' ').next()?;
    let (repo, number) = target.split_once('#')?;
    if !word_repo(repo) || number.is_empty() || !number.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((kind, repo, number.parse().ok()?))
}

/// `[\w.-]+/[\w.-]+`.
pub(crate) fn word_repo(repo: &str) -> bool {
    let part = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
    };
    repo.split_once('/')
        .is_some_and(|(owner, name)| part(owner) && part(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn board() -> Value {
        json!({
            "operator": "owner", "bot": "worker",
            "dispatchRepo": "example/control", "dispatchWorkflow": "dispatch.yml",
            "follow": ["example/control", "example/project"],
            "mergeAuthors": ["dependabot[bot]"],
            "kindPools": {"triage": "small"},
            "epics": {"repository": "example/control"},
            "schedule": {"repositories": ["example/control", "example/project"]},
        })
    }

    pub(crate) fn supervisor() -> Value {
        json!({
            "pollWorkflow": "poll.yml", "probeWorkflow": "broker-probe.yml",
            "supervisorWorkflow": "supervisor.yml", "schedulerJob": "board",
            "implementBranchPrefix": "dispatch/implement/agent-run-",
            "releaseRepository": "example/agentic-job", "wrapperRepository": "example/actions",
            "wrapperPath": ".github/workflows/wrapper.yml",
            "agent": {
                "pool": "large", "cooldownHours": 1,
                "requests": {"repository": "example/control", "issue": 5},
                "log": {"repository": "example/operator", "issue": 7},
                "kinds": ["triage", "research"],
                "maxDispatchesPerPass": 2, "maxDispatchesPerDay": 3,
            },
        })
    }

    pub(crate) fn settings() -> Settings {
        Settings::from_json(
            &board(),
            &supervisor(),
            Some(&json!({"repo": "example/ops", "issue": 1, "bot": "worker"})),
        )
        .unwrap()
    }

    #[test]
    fn settings_come_from_the_files_and_stay_off_without_the_agent_issues() {
        let s = settings();
        assert_eq!(s.limits.max_requests, 500);
        assert_eq!(s.usage.as_ref().unwrap().issue.repository, "example/ops");
        let agent = s.agent.unwrap();
        assert_eq!(agent.max_dispatches_per_pass, 2);
        for key in ["requests", "log"] {
            let mut cfg = supervisor();
            cfg["agent"][key] = Value::Null;
            assert!(
                Settings::from_json(&board(), &cfg, None)
                    .unwrap()
                    .agent
                    .is_none(),
                "{key}"
            );
        }
        // A cap left out admits no dispatch.
        let mut cfg = supervisor();
        cfg["agent"]
            .as_object_mut()
            .unwrap()
            .remove("maxDispatchesPerDay");
        let agent = Settings::from_json(&board(), &cfg, None)
            .unwrap()
            .agent
            .unwrap();
        assert_eq!(agent.max_dispatches_per_day, 0);
    }

    #[test]
    fn settings_refuse_names_that_could_redirect_a_request() {
        let cases: &[(&str, Value)] = &[
            ("releaseRepository", json!("../x")),
            ("wrapperRepository", json!("a/b/c")),
            ("wrapperPath", json!("../secrets")),
            ("wrapperPath", json!("a?ref=x")),
            ("pollWorkflow", json!("poll")),
            ("implementBranchPrefix", json!("a b")),
            ("pageSize", json!(500)),
            ("maxRequests", json!(0)),
            ("holdWaitHours", json!("12")),
        ];
        for (key, value) in cases {
            let mut cfg = supervisor();
            cfg[*key] = value.clone();
            assert!(
                Settings::from_json(&board(), &cfg, None).is_err(),
                "{key}={value}"
            );
        }
        let mut cfg = supervisor();
        cfg["agent"]["log"]["repository"] = json!("evil/../x");
        assert!(Settings::from_json(&board(), &cfg, None).is_err());
        let mut cfg = supervisor();
        cfg["agent"]["maxDispatchesPerPass"] = json!(-1);
        assert!(Settings::from_json(&board(), &cfg, None).is_err());
        let mut b = board();
        b["follow"] = json!(["example/project", "../../orgs"]);
        assert!(Settings::from_json(&b, &supervisor(), None).is_err());
    }

    #[test]
    fn run_titles_and_times() {
        assert_eq!(
            run_title("implement example/project#6 (claude)"),
            Some(("implement", "example/project", 6))
        );
        assert_eq!(run_title("research o/r#4"), Some(("research", "o/r", 4)));
        for title in [
            "poll",
            "implement o/r",
            "deploy o/r#1",
            "implement o/r#1x",
            "implement o#1",
        ] {
            assert_eq!(run_title(title), None, "{title}");
        }
        let at = millis(&json!("2026-10-15T10:00:00Z")).unwrap();
        assert_eq!(iso(at), "2026-10-15T10:00:00.000Z");
        assert_eq!(millis(&json!("soon")), None);
    }
}
