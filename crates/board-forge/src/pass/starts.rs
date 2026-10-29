//! Runs that never started (agent-board#440, docs/pass.md#start-failures):
//! a run that failed before its agent session started, or whose inference
//! token was lost mid-run (agent-board#449), says nothing of the work, so it
//! is re-run once and then leaves its plan unspent; a run of such
//! failures with the same first error line stops stage 4's starts until a
//! run starts cleanly; and stage 4 raises an alarm when it starts nothing
//! for a while with slots free and candidates held for no reason it expects.
//! Both alarms are one marker issue each in the epics repository, labelled
//! `needs-human`, which the pass updates and closes itself.
use super::*;
use crate::poll::{before_own_steps, check_run_id};

/// The hidden first line of an alarm issue's body names its kind:
/// `<!-- agent-board-alarm: KIND -->`.
const ALARM_MARKER: &str = "agent-board-alarm";
const BREAKER: &str = "breaker";
const STALL: &str = "stall";
/// The step of agentic-job's agent job (`run / agent` in the caller's run)
/// that starts the agent session.
pub(super) const AGENT_STEP: &str = "Run ./.agentic-job-source/run";
/// What agentic-job's `run` says where the agent session did not start, its
/// exit state 4.
const NOT_STARTED: &str = "the run did not start";
const EXIT_STATE: &str = "agentic-job run ended with exit state ";
const NOT_STARTED_STATE: u8 = 4;
/// The exit states of a run that ended once its agent had started, a
/// failure and a stop at a limit, after which its log is read for a lost
/// inference token ([`token_lost`]).
const ENDED_STATES: [u8; 2] = [1, 3];
/// What agentic-job's `run` logs, on its own line, where the inference proxy
/// no longer knew the run's token, as when its broker restarted: the agent's
/// requests were refused from then on (agent-board#449). It has no exit
/// state of its own.
const PROXY_WARNING: &str = "warning: the inference proxy at ";
const TOKEN_LOST: &str = "no longer knows this run's token (HTTP 401; did it restart?)";
/// The bytes of an agent job's log read for it: agent-board#449's was
/// 126 KB, its agent's output condensed.
const LOG_BYTES: u64 = 4 << 20;
/// How a run failed on infrastructure ([`Infra::how`]).
const BEFORE_AGENT: &str = "failed before the agent started";
const LOST_TOKEN: &str = "lost its inference token mid-run";
/// The first error line of a run that lost its inference token, the same
/// for every such run, so that the breaker counts them together.
const LOST_TOKEN_LINE: &str =
    "the inference proxy no longer knew the run's token (HTTP 401; did it restart?)";
/// The caller's newest runs read for the breaker and the stall alarm.
const RECENT_RUNS: usize = 20;
/// `starts.breakerRuns` when left out, and its bounds: at most half of
/// [`RECENT_RUNS`].
const BREAKER_RUNS: usize = 3;
const BREAKER_RUNS_MIN: usize = 2;
const BREAKER_RUNS_LIMIT: usize = 10;
/// `starts.stallAfterMinutes` when left out, and its bounds: a week at most.
const STALL_MINUTES: u64 = 60;
const STALL_MINUTES_MIN: u64 = 10;
const STALL_MINUTES_LIMIT: u64 = 7 * 24 * 60;
/// How long after the newest run a tripped breaker lets one run start to
/// find out whether the cause is gone.
const PROBE_SECONDS: i64 = 60 * 60;
/// The characters of a first error line kept.
const LINE_LIMIT: usize = 200;
/// The annotations of a failed job read for its first error line.
const ANNOTATIONS_PER_PAGE: usize = 50;
/// The pages of the bot's open issues read for its alarm issues, and their
/// size; more is an error, as an alarm could hide past them.
const ALARM_PAGES: usize = 3;
const ALARM_PER_PAGE: usize = 100;
/// The holds of a candidate that the pass does not expect to end by
/// themselves, which a stall names; the others wait on a person, a pull
/// request, a run, a date or pacing.
const UNEXPLAINED: &[HoldKind] = &[HoldKind::Other, HoldKind::Unreadable];

/// `starts` in the configuration ([`Config::starts`]).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Starts {
    /// How many of the newest ended runs, all failed before the agent
    /// started with the same first error line, stop stage 4's starts.
    #[serde(default = "breaker_runs")]
    pub breaker_runs: usize,
    /// How long nothing started, with slots free and a candidate held for
    /// no reason the pass expects, before it says so.
    #[serde(default = "stall_minutes")]
    pub stall_after_minutes: u64,
}

fn breaker_runs() -> usize {
    BREAKER_RUNS
}

fn stall_minutes() -> u64 {
    STALL_MINUTES
}

impl Starts {
    pub(super) fn check(&self, config: &Config) -> Result<()> {
        ensure!(
            (BREAKER_RUNS_MIN..=BREAKER_RUNS_LIMIT).contains(&self.breaker_runs),
            "starts breakerRuns is not {BREAKER_RUNS_MIN} to {BREAKER_RUNS_LIMIT}"
        );
        ensure!(
            (STALL_MINUTES_MIN..=STALL_MINUTES_LIMIT).contains(&self.stall_after_minutes),
            "starts stallAfterMinutes is not {STALL_MINUTES_MIN} to {STALL_MINUTES_LIMIT}"
        );
        // The breaker's issue names the caller's runs and their errors, so
        // it goes where they are, not to readers who cannot see them.
        let epics = config.epics.as_ref().context("starts needs epics")?;
        ensure!(
            epics.repository.eq_ignore_ascii_case(&config.dispatch_repo),
            "starts needs epics.repository to be dispatchRepo, whose runs its issues name"
        );
        Ok(())
    }
}

/// Whether an annotation is agentic-job's saying its run ended with exit
/// state `state`: it begins `agentic-job run ended with exit state STATE`
/// and no further digit. Only its start counts, as the agent may quote
/// these words in an annotation of its own.
fn ended_with(message: &str, state: u8) -> bool {
    let lower = message.trim_start().to_ascii_lowercase();
    (lower.strip_prefix(EXIT_STATE))
        .and_then(|rest| rest.strip_prefix(&state.to_string()))
        .is_some_and(|rest| !rest.starts_with(|c: char| c.is_ascii_digit()))
}

/// Whether an annotation is agentic-job's saying the agent session did not
/// start: it begins `the run did not start`, or says exit state 4.
fn not_started(message: &str) -> bool {
    let lower = message.trim_start().to_ascii_lowercase();
    lower.starts_with(NOT_STARTED) || ended_with(message, NOT_STARTED_STATE)
}

/// Whether an agent job's `log` has agentic-job's own warning that the
/// inference proxy no longer knew the run's token: a line that, after the
/// runner's timestamp, begins [`PROXY_WARNING`] and says [`TOKEN_LOST`].
/// The agent's condensed output in the log has lines of its own, so an
/// agent that printed such a line, and ended with one of [`ENDED_STATES`],
/// could earn its run the one rerun and its plan a release, as a broker
/// restart would: each time, two runs within WIP and the pass's caps.
/// agentic-job does not yet say this in an annotation, which the agent
/// could not write.
fn token_lost(log: &str) -> bool {
    (log.lines())
        .filter_map(|line| line.split_once(' ').map(|(_, rest)| rest))
        .any(|rest| rest.starts_with(PROXY_WARNING) && rest.contains(TOKEN_LOST))
}

/// Why a run failed on infrastructure ([`Scheduler::before_agent`]).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Infra {
    /// How, as a line says it after the run: [`BEFORE_AGENT`] or
    /// [`LOST_TOKEN`].
    pub how: &'static str,
    /// Its first error line.
    pub line: String,
}

/// The steps of an Actions `job`, in order.
fn steps(job: &Value) -> &[Value] {
    job["steps"].as_array().map_or(&[][..], Vec::as_slice)
}

/// The first line of `message`, trimmed, at most [`LINE_LIMIT`] characters,
/// with no backtick to end the fence the breaker's issue quotes it in.
fn first_line(message: &str) -> String {
    let line = message.lines().map(str::trim).find(|l| !l.is_empty());
    (line.unwrap_or_default().chars())
        .take(LINE_LIMIT)
        .map(|c| if c == '`' { '\'' } else { c })
        .collect()
}

/// What stage 4 may start, by [`Scheduler::guard`].
pub(super) struct Guard {
    /// How the runs that tripped the breaker failed, and those runs, newest
    /// first; none where it is not tripped.
    tripped: Option<(Infra, Vec<String>)>,
    /// When the newest run read was created or last started.
    newest: Option<OffsetDateTime>,
}

/// An open alarm issue of the bot's.
#[derive(Clone)]
pub(super) struct Alarm {
    number: u64,
    url: String,
    body: String,
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    /// Why `run` of the caller's workflow failed before its agent session
    /// started, as its first error line; none where it did not fail, or
    /// may have failed once the agent had started. Where the agent's step
    /// ran, in any job, only its own annotation saying so counts
    /// ([`not_started`]); one that ended otherwise (cancelled at a timeout,
    /// say) had started. Where it did not run, the first failed job decides:
    /// one with no failed step (a lost runner), its setup or checkout, or a
    /// step of the agent job before the agent's. A job of the caller's own
    /// that fails at its own steps (a policy refusal, say) would fail the
    /// same way again, and an agent job without the agent's step cannot be
    /// read, so neither is one. Two reads, once a pass: the run's jobs and
    /// that job's annotations. An agent job whose agent's step failed with
    /// exit state 1 or 3 ([`ENDED_STATES`]) is read a third time, for its
    /// log, and where the inference proxy lost the run's token there
    /// ([`token_lost`]), the run failed on infrastructure too.
    pub(super) fn before_agent(&self, repo: &str, run: &Value) -> Result<Option<Infra>> {
        if run["conclusion"] != RERUN_CONCLUSION {
            return Ok(None);
        }
        let id = run["id"].as_u64().context("run has no id")?;
        if let Some(found) = self.infra.borrow().get(&id) {
            return Ok(found.clone());
        }
        let url = format!("{API_ROOT}repos/{repo}/actions/runs/{id}/jobs?per_page={JOBS_PER_PAGE}");
        let response = self.client.get(&url, None)?;
        ensure!(
            response.status == 200,
            "reading run {id}'s jobs: GitHub HTTP status {}",
            response.status
        );
        let jobs = (response.body["jobs"].as_array()).context("run's jobs are not a list")?;
        let found = match jobs.iter().find(|j| j["conclusion"] == RERUN_CONCLUSION) {
            None => None,
            Some(job) => {
                let own = steps(job);
                let failed = own.iter().position(|s| s["conclusion"] == RERUN_CONCLUSION);
                let agent = own.iter().position(|s| text(s, "/name") == AGENT_STEP);
                let ran = (jobs.iter().flat_map(steps))
                    .find(|s| text(s, "/name") == AGENT_STEP)
                    .map_or("", |s| text(s, "/conclusion"));
                let messages = self.failures(repo, job)?;
                let name = text(job, "/name");
                let early = match ran {
                    RERUN_CONCLUSION => messages.iter().any(|m| not_started(m)),
                    "" | "skipped" => match (failed, agent) {
                        (None, _) => true,
                        (Some(f), Some(a)) if f < a => true,
                        (Some(f), _) => before_own_steps(text(&own[f], "/name")),
                    },
                    _ => false,
                };
                let step = failed.map_or("no step", |f| text(&own[f], "/name"));
                let line = (messages.first().map(|m| first_line(m)))
                    .filter(|l| !l.is_empty())
                    .unwrap_or_else(|| first_line(&format!("job {name} failed at {step}")));
                let ended = !early
                    && agent.is_some_and(|a| text(&own[a], "/conclusion") == RERUN_CONCLUSION)
                    && (messages.iter()).any(|m| ENDED_STATES.iter().any(|&s| ended_with(m, s)));
                if early {
                    Some(Infra {
                        how: BEFORE_AGENT,
                        line,
                    })
                } else if ended && self.lost_token(repo, job)? {
                    Some(Infra {
                        how: LOST_TOKEN,
                        line: LOST_TOKEN_LINE.to_owned(),
                    })
                } else {
                    None
                }
            }
        };
        self.infra.borrow_mut().insert(id, found.clone());
        Ok(found)
    }

    /// Whether the Actions agent `job`'s log says the inference proxy no
    /// longer knew the run's token ([`token_lost`]), at most [`LOG_BYTES`]
    /// of it read. A log that cannot be read, gone or too long, does not
    /// say so: an error here would stop the breaker's every pass while
    /// that run is among the newest. A later pass reads it again.
    fn lost_token(&self, repo: &str, job: &Value) -> Result<bool> {
        let id = job["id"].as_u64().context("job has no id")?;
        let url = format!("{API_ROOT}repos/{repo}/actions/jobs/{id}/logs");
        let log = self.client.download(&url, LOG_BYTES).unwrap_or_default();
        Ok(token_lost(&String::from_utf8_lossy(&log)))
    }

    /// The failure annotations' messages of the Actions `job`, in order:
    /// none where it names no check run.
    fn failures(&self, repo: &str, job: &Value) -> Result<Vec<String>> {
        let Some(check_run) = check_run_id(repo, job) else {
            return Ok(Vec::new());
        };
        let url = format!(
            "{API_ROOT}repos/{repo}/check-runs/{check_run}/annotations?per_page={ANNOTATIONS_PER_PAGE}"
        );
        let response = self.client.get(&url, None)?;
        ensure!(
            response.status == 200,
            "reading check run {check_run}'s annotations: GitHub HTTP status {}",
            response.status
        );
        let list = response
            .body
            .as_array()
            .context("annotations are not a list")?;
        Ok((list.iter())
            .filter(|a| a["annotation_level"] == "failure")
            .map(|a| text(a, "/message").to_owned())
            .collect())
    }

    /// The breaker, from the caller's newest [`RECENT_RUNS`] runs, one
    /// read: tripped where the newest `breakerRuns` of them that ended all
    /// failed before the agent started ([`Self::before_agent`]) with the
    /// same first error line. A run in flight neither trips nor clears it.
    fn guard(&self, starts: &Starts, target: &Target) -> Result<Guard> {
        let url = format!(
            "{API_ROOT}repos/{}/actions/workflows/{}/runs?per_page={RECENT_RUNS}",
            target.caller, target.workflow
        );
        let response = self.client.get(&url, None)?;
        ensure!(
            response.status == 200,
            "listing the newest runs: GitHub HTTP status {}",
            response.status
        );
        let runs =
            (response.body["workflow_runs"].as_array()).context("run list has no workflow_runs")?;
        // A rerun keeps its run's creation time, but starts it again.
        let newest = (runs.iter())
            .flat_map(|r| [at(&r["created_at"]), at(&r["run_started_at"])])
            .flatten()
            .max();
        let ended: Vec<&Value> = (runs.iter())
            .filter(|r| r["status"] == "completed")
            .take(starts.breaker_runs)
            .collect();
        let mut infra = Vec::new();
        for run in &ended {
            let Some(found) = self.before_agent(&target.caller, run)? else {
                break;
            };
            infra.push(found);
        }
        let tripped = (infra.len() == starts.breaker_runs && infra.iter().all(|i| *i == infra[0]))
            .then(|| {
                let urls = ended.iter().map(|r| text(r, "/html_url").to_owned());
                (infra[0].clone(), urls.collect())
            });
        Ok(Guard { tripped, newest })
    }

    /// With [`Config::starts`], how many of `free` slots stage 4 may fill:
    /// all of them, none while the breaker is tripped, or one once the
    /// newest run is [`PROBE_SECONDS`] old, to find out whether the cause
    /// is gone. The breaker's issue is raised, updated or closed here. None
    /// where stage 4 starts nothing, which it said; and when the newest run
    /// was created, for [`Self::stall`].
    pub(super) fn guarded(
        &mut self,
        target: &Target,
        free: usize,
    ) -> Option<(usize, Option<OffsetDateTime>)> {
        let Some(starts) = &self.config.starts else {
            return Some((free, None));
        };
        let guard = match self.guard(starts, target) {
            Ok(guard) => guard,
            // An unread breaker is not permission to start.
            Err(error) => {
                self.report.failures += 1;
                self.line(format!("breaker error: {error:#}; new runs wait"));
                return None;
            }
        };
        let Some((first, runs)) = guard.tripped else {
            self.alarm(BREAKER, None);
            return Some((free, guard.newest));
        };
        let n = runs.len();
        let Infra { how, line: first } = first;
        let said = format!("the newest {n} runs {how}: {first} ({})", runs.join(", "));
        let caller = format!("{}'s {}", target.caller, target.workflow);
        let links: String = runs.iter().map(|r| format!("- {r}\n")).collect();
        let text = format!(
            "The board pass stopped starting runs: the newest {n} runs of {caller} {how}, each with\n\n\
             ```text\n{first}\n```\n\n{links}\n\
             It starts one run an hour after the newest to find out whether the cause is gone, and closes this issue once a run starts cleanly. \
             Each such run that {how} is re-run once, and its plan is not spent."
        );
        let probe = guard
            .newest
            .is_none_or(|t| (self.now - t).whole_seconds() >= PROBE_SECONDS);
        let allowed = if probe && free > 0 {
            self.line(format!("breaker: {said}; one run starts to probe it"));
            Some((1, guard.newest))
        } else {
            let from = guard.newest.map_or(String::new(), |t| {
                let due = t + time::Duration::seconds(PROBE_SECONDS);
                format!(
                    ", and probes it from {}",
                    due.format(&Rfc3339).unwrap_or_default()
                )
            });
            self.line(format!(
                "breaker: {said}; stage 4 starts nothing until a run starts cleanly{from}"
            ));
            None
        };
        self.alarm(
            BREAKER,
            Some(("Board pass: runs fail on infrastructure", text)),
        );
        allowed
    }

    /// With [`Config::starts`], once stage 4's starts left `free` slots and
    /// took none: a stall where nothing started since `newest` for
    /// `stallAfterMinutes`, in this pass either (`earlier` runs), and a
    /// candidate considered since the `holds`th hold was held for no
    /// reason the pass expects ([`UNEXPLAINED`]). Its issue says so,
    /// without the candidates, whose repositories may be private; it is
    /// closed otherwise.
    pub(super) fn stall(
        &mut self,
        holds: usize,
        (earlier, taken, free): (usize, usize, usize),
        newest: Option<OffsetDateTime>,
    ) {
        let Some(starts) = &self.config.starts else {
            return;
        };
        let minutes = starts.stall_after_minutes;
        let quiet = newest.is_none_or(|t| (self.now - t).whole_minutes() >= minutes as i64);
        let held: Vec<&str> = (self.report.holds[holds..].iter())
            .filter(|e| matches!(e.scope, Scope::Item(_)))
            .filter(|e| !e.hold.operator && UNEXPLAINED.contains(&e.hold.kind))
            .map(|e| e.hold.kind.name())
            .collect();
        let stalled = quiet && earlier + taken == 0 && self.epic_starts.is_empty() && free > 0;
        if !stalled || held.is_empty() {
            self.alarm(STALL, None);
            return;
        }
        let since = newest.map_or("the newest runs read".into(), |t| {
            t.format(&Rfc3339).unwrap_or_default()
        });
        let kinds: BTreeSet<&str> = held.iter().copied().collect();
        let kinds: Vec<&str> = kinds.into_iter().collect();
        let kinds = kinds.join(", ");
        self.line(format!(
            "stall: nothing started since {since}, with {free} slot(s) free and {} candidate(s) held as {kinds}",
            held.len()
        ));
        // Kept the same from pass to pass, so that the issue is not edited
        // on every one.
        let text = format!(
            "The board pass has started no run since {since}, over {minutes} minutes, though slots are free and candidates are held \
             as {kinds}, which it does not expect to end by itself. Its run's log and hold snapshot name them. \
             The pass closes this issue once a run starts or nothing is held so."
        );
        self.alarm(STALL, Some(("Board pass: nothing starts", text)));
    }

    /// The bot's open alarm issues in the epics repository, by kind, the
    /// newest of each: read once a pass.
    fn alarms(&mut self) -> Result<BTreeMap<String, Alarm>> {
        if let Some(alarms) = &self.alarms {
            return Ok(alarms.clone());
        }
        let config = self.config;
        let repository = &config
            .epics
            .as_ref()
            .context("alarms need epics")?
            .repository;
        let mut alarms = BTreeMap::new();
        for page in 1..=ALARM_PAGES {
            let url = format!(
                "{API_ROOT}repos/{repository}/issues?state=open&creator={}&sort=created&direction=desc&per_page={ALARM_PER_PAGE}&page={page}",
                config.bot
            );
            let read = self.client.get(&url, None)?;
            ensure!(
                read.status == 200,
                "listing the alarm issues of {repository}: GitHub HTTP status {}",
                read.status
            );
            let issues = read.body.as_array().context("issues are not a list")?;
            for issue in issues.iter().filter(|i| i.get("pull_request").is_none()) {
                let body = issue["body"].as_str().unwrap_or_default();
                let kind = (body.lines().next())
                    .and_then(|l| l.strip_prefix(&format!("<!-- {ALARM_MARKER}: ")))
                    .and_then(|l| l.strip_suffix(" -->"));
                if let Some(kind) = kind {
                    alarms.entry(kind.to_owned()).or_insert(Alarm {
                        number: issue["number"].as_u64().context("issue has no number")?,
                        url: text(issue, "/html_url").to_owned(),
                        body: body.to_owned(),
                    });
                }
            }
            if issues.len() < ALARM_PER_PAGE {
                self.alarms = Some(alarms.clone());
                return Ok(alarms);
            }
        }
        bail!("the bot's open issues in {repository} are more than {ALARM_PAGES} pages")
    }

    /// Raises the alarm `kind` with its title and text, or updates its
    /// issue where the text changed; with none, closes its open issue.
    /// Each a write step within `--max-writes`, and only with the bot's
    /// token, as only the bot's issues are found again.
    fn alarm(&mut self, kind: &str, raise: Option<(&str, String)>) {
        let open = match self.alarms() {
            Ok(alarms) => alarms.get(kind).cloned(),
            Err(error) => {
                self.report.failures += 1;
                self.line(format!("alarm {kind} error: {error:#}"));
                return;
            }
        };
        let Some(epics) = &self.config.epics else {
            return;
        };
        let repository = &epics.repository;
        let body = raise.as_ref().map(|(_, text)| {
            format!(
                "<!-- {ALARM_MARKER}: {kind} -->\n{text}\n\n{}",
                crate::Footer::default()
            )
        });
        let at = |n: u64| format!("{API_ROOT}repos/{repository}/issues/{n}");
        let (line, method, url, sent) = match (raise, open) {
            (None, None) => return,
            (None, Some(open)) => (
                format!("alarm {kind}: close {}, cleared", open.url),
                "PATCH",
                at(open.number),
                json!({"state": "closed", "state_reason": "completed"}),
            ),
            (Some((title, _)), None) => (
                format!("alarm {kind}: file \"{title}\" in {repository}"),
                "POST",
                format!("{API_ROOT}repos/{repository}/issues"),
                json!({"title": title, "body": body, "labels": [NEEDS_HUMAN]}),
            ),
            (Some(_), Some(open)) if Some(&open.body) == body.as_ref() => {
                self.line(format!("alarm {kind}: raised already, {}", open.url));
                return;
            }
            (Some(_), Some(open)) => (
                format!("alarm {kind}: update {}", open.url),
                "PATCH",
                at(open.number),
                json!({"body": body}),
            ),
        };
        if let Some(held) = self.caps.check(Kind::Write) {
            self.line(format!("{held}: {line}"));
            return;
        }
        self.caps.count(Kind::Write);
        if !self.apply {
            self.line(format!("{line}; a preview writes nothing"));
            return;
        }
        let sent = plan::viewer(&self.client).and_then(|login| {
            let bot = &self.config.bot;
            ensure!(
                login.eq_ignore_ascii_case(bot),
                "the token is {login}'s, not {bot}'s; only the bot files the pass's alarms, which it would not find again"
            );
            plan::send(&self.client, method, &url, &sent, 0)
        });
        match sent {
            Ok(_) => self.line(line),
            Err(error) => {
                self.report.failures += 1;
                self.line(format!("{line} failed: {error:#}"));
            }
        }
    }

    /// The step for `issue`, whose implement `plan`'s dispatched `run` failed
    /// on infrastructure again after its rerun, `infra` saying how: its
    /// `record` is released as a refused dispatch's is, so the plan starts
    /// again on a later pass, which the breaker bounds. One write step
    /// within `--max-writes`; the comment links the run, but does not carry
    /// its error, which another repository's readers may not see.
    pub(super) fn release(
        &self,
        issue: &ItemRef,
        (plan, record): (&plan::Comment, &plan::Comment),
        run: &Value,
        Infra { how, line: first }: &Infra,
    ) -> Result<Step> {
        let url = text(run, "/html_url");
        let line = format!(
            "its implement run {url} {how} again after a rerun ({first}), so its record is released and its plan starts again"
        );
        let hold = |why: String| Ok(Step::Hold(Hold::new(HoldKind::Dispatched, why)));
        if let Some(held) = self.caps.check(Kind::Write) {
            return hold(format!("{held}: {line}"));
        }
        self.caps.count(Kind::Write);
        if !self.apply {
            return hold(format!("{line}; a preview releases nothing"));
        }
        let id = plan::comment_id(&record.url)
            .with_context(|| format!("its record {} has no id", record.url))?;
        let at = format!("{API_ROOT}repos/{}/issues/comments/{id}", issue.repository);
        let body = format!(
            "The plan in {} was dispatched, but its [run]({url}) {how}. A later dispatch may take it.",
            plan.url
        );
        plan::send(&self.client, "PATCH", &at, &json!({"body": body}), 0)
            .context("releasing its record")?;
        hold(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_started_reads_agentic_jobs_words() {
        for (message, expected) in [
            ("the run did not start: runner-sandbox cannot run …", true),
            ("agentic-job run ended with exit state 4", true),
            ("agentic-job run ended with exit state 4.", true),
            ("agentic-job run ended with exit state 40", false),
            ("agentic-job run ended with exit state 2", false),
            ("Process completed with exit code 4.", false),
            // The agent quoting them.
            ("the issue says: the run did not start", false),
            ("as #440 said, exit state 4", false),
        ] {
            assert_eq!(not_started(message), expected, "{message}");
        }
    }

    #[test]
    fn ended_with_reads_one_exit_state() {
        for (message, state, expected) in [
            ("agentic-job run ended with exit state 3", 3, true),
            (
                "agentic-job run ended with exit state 1 (inference_unauthorized)",
                1,
                true,
            ),
            ("agentic-job run ended with exit state 3", 1, false),
            ("agentic-job run ended with exit state 124", 1, false),
            ("agentic-job run ended with exit state 30", 3, false),
            (
                "the agent said: agentic-job run ended with exit state 3",
                3,
                false,
            ),
        ] {
            assert_eq!(ended_with(message, state), expected, "{message}");
        }
    }

    #[test]
    fn token_lost_reads_agentic_jobs_own_line() {
        let warning = "warning: the inference proxy at http://100.114.231.4:18080 no longer knows this run's token (HTTP 401; did it restart?), so the token admits nothing";
        let quoted = format!("▶ Bash: echo '{warning}' (ok, 0s)");
        let other = "warning: the inference proxy has given no count of this run's model requests 6 times in a row";
        for (lines, expected) in [
            (vec![warning], true),
            (vec![other, warning], true),
            (vec![other], false),
            (vec![quoted.as_str()], false),
        ] {
            let log: String = (lines.iter())
                .map(|l| format!("2026-10-29T21:30:01.1105672Z {l}\n"))
                .collect();
            assert_eq!(token_lost(&log), expected, "{log}");
        }
        // A runner's log line begins with its timestamp.
        assert!(!token_lost(warning));
    }

    #[test]
    fn first_line_is_the_first_nonempty_one_bounded() {
        assert_eq!(first_line("\n  error: one  \ntwo"), "error: one");
        assert_eq!(first_line("```end"), "'''end");
        assert_eq!(first_line(&"x".repeat(500)).len(), LINE_LIMIT);
    }
}
