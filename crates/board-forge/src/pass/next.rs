//! `agent-board pass next` (docs/pass.md#the-next-pass): what starts after an
//! applying pass. It waits out the pass interval from its own run's start,
//! wakes optional auxiliary workflows whose newest runs are stale, and dispatches the
//! pass workflow again unless a newer applying run will. Every name, input
//! and interval is the caller's.
use crate::github::API_ROOT;
use crate::poll::{Rest, validate_repository};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// Tries of each GitHub call: one 5xx must not end the chain.
pub const TRIES: u32 = 4;

/// The wait before the second try, doubled before each one after it.
pub const FIRST_RETRY_WAIT: Duration = Duration::from_secs(2);

/// Runs a page of the run listing holds, GitHub's most.
pub const RUN_PAGE: usize = 100;

/// Pages of the pass workflow's runs read for this run, newest first: the
/// listing must reach back to it, else a newer run may be unseen.
pub const RUN_PAGES: usize = 2;

/// The longest pass interval: a GitHub-hosted job runs at most six hours, so
/// a longer wait could only hold a runner until GitHub ends the job.
pub const MAX_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Run statuses of a run still to finish.
const UNFINISHED: [&str; 5] = ["queued", "waiting", "pending", "requested", "in_progress"];

/// The run event of an applying pass whatever its title, as a scheduled pass
/// only continues the chain while the caller applies.
const SCHEDULE_EVENT: &str = "schedule";

/// The run event of a dispatched pass, applying only by its title.
const DISPATCH_EVENT: &str = "workflow_dispatch";

/// Supervisor runs that say nothing about whether it is awake: any comment in
/// the repository starts one.
const IGNORED_SUPERVISOR_EVENT: &str = "issue_comment";

/// A workflow to dispatch, with its string inputs.
#[derive(Clone, Debug, PartialEq)]
pub struct Workflow {
    /// A workflow file name such as `pass.yml`, or its numeric ID.
    pub file: String,
    pub inputs: BTreeMap<String, String>,
}

/// An auxiliary workflow to wake, and how old its newest run may grow.
#[derive(Clone, Debug, PartialEq)]
pub struct Supervisor {
    pub workflow: Workflow,
    pub stale: Duration,
}

/// What one `pass next` does, all from its caller.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// The repository of the pass workflow and this run, `OWNER/REPO`.
    pub repository: String,
    /// This run's ID.
    pub run: u64,
    /// The default branch: the dispatches' ref and the runs listed.
    pub branch: String,
    /// The pass workflow, with the inputs of an applying dispatch.
    pub pass: Workflow,
    /// The run title of a dispatched pass that applies.
    pub apply_title: String,
    /// The pass interval: the next pass starts this long after this run
    /// started, and never more than this long from now.
    pub interval: Duration,
    pub supervisor: Option<Supervisor>,
    pub train: Option<Supervisor>,
    /// Read and print what would start; wait for and start nothing.
    pub dry_run: bool,
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        validate_repository(&self.repository)?;
        ensure!(self.run > 0, "invalid run ID 0");
        ensure!(
            !self.branch.is_empty() && !self.branch.chars().any(char::is_control),
            "invalid branch {:?}",
            self.branch
        );
        ensure!(!self.apply_title.is_empty(), "empty apply run title");
        ensure!(
            self.interval <= MAX_INTERVAL,
            "a pass interval over {} hours",
            MAX_INTERVAL.as_secs() / 3600
        );
        validate_workflow(&self.pass.file)?;
        for wake in [&self.supervisor, &self.train].into_iter().flatten() {
            validate_workflow(&wake.workflow.file)?;
        }
        Ok(())
    }
}

/// A workflow file name or ID, which is a path segment of the API's URLs.
fn validate_workflow(file: &str) -> Result<()> {
    ensure!(
        !file.is_empty()
            && !file.starts_with('.')
            && file
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)),
        "invalid workflow {file:?}: expected a file name such as pass.yml, or an ID"
    );
    Ok(())
}

/// The clock `pass next` waits by; tests advance it without sleeping.
pub trait Clock {
    fn now(&self) -> OffsetDateTime;
    fn sleep(&self, duration: Duration);
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration)
    }
}

/// How long to wait at `now` for the pass started at `started`: until one
/// `interval` after its start, never longer than `interval`.
pub fn wait_for(started: OffsetDateTime, now: OffsetDateTime, interval: Duration) -> Duration {
    let left = started + interval - now;
    Duration::try_from(left).unwrap_or_default().min(interval)
}

/// The wait before try `next` (2 or more) of a call.
fn retry_wait(next: u32) -> Duration {
    FIRST_RETRY_WAIT.saturating_mul(1 << (next - 2).min(16))
}

/// Whether the supervisor is due at `now`, given its newest run that counts:
/// with none, or with one at least `stale` old.
pub fn supervisor_due(
    newest: Option<OffsetDateTime>,
    now: OffsetDateTime,
    stale: Duration,
) -> bool {
    newest.is_none_or(|created| now - created >= stale)
}

/// The creation time of the newest run in `runs` (newest first) that is not
/// an [`IGNORED_SUPERVISOR_EVENT`]'s.
pub fn newest_supervisor_run(runs: &[Value]) -> Result<Option<OffsetDateTime>> {
    newest_auxiliary_run(runs, true)
}

fn newest_auxiliary_run(runs: &[Value], ignore_comments: bool) -> Result<Option<OffsetDateTime>> {
    for run in runs {
        if !ignore_comments || text(run, "event")? != IGNORED_SUPERVISOR_EVENT {
            return created(run).map(Some);
        }
    }
    Ok(None)
}

/// The newest run in `runs` other than `settings.run` that continues the
/// chain: an applying pass run on the branch, still to finish, and created
/// after this run (by ID within the same second). `runs` must hold this run,
/// else a newer one may be unlisted.
pub fn newer_run(runs: &[Value], settings: &Settings) -> Result<Option<u64>> {
    let me = runs
        .iter()
        .find(|run| run["id"].as_u64() == Some(settings.run))
        .with_context(|| {
            format!(
                "run {} is not among the newest {} runs of {}",
                settings.run,
                RUN_PAGE * RUN_PAGES,
                settings.pass.file
            )
        })?;
    let me = (created(me)?, settings.run);
    for run in runs {
        let id = run["id"].as_u64().context("a run without an id")?;
        let applying = match text(run, "event")? {
            SCHEDULE_EVENT => true,
            DISPATCH_EVENT => text(run, "display_title")? == settings.apply_title,
            _ => false,
        };
        if id != settings.run
            && text(run, "head_branch")? == settings.branch
            && UNFINISHED.contains(&text(run, "status")?)
            && applying
            && (created(run)?, id) > me
        {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

fn text<'a>(run: &'a Value, field: &str) -> Result<&'a str> {
    run[field].as_str().with_context(|| {
        format!(
            "run {}: missing {field}",
            run["id"].as_u64().unwrap_or_default()
        )
    })
}

fn created(run: &Value) -> Result<OffsetDateTime> {
    let at = text(run, "created_at")?;
    OffsetDateTime::parse(at, &Rfc3339).with_context(|| format!("created_at {at:?}"))
}

/// One `pass next`, with its decisions on `out` and failed tries and
/// warnings on `err`. Fails when it cannot tell whether to continue the
/// chain, or cannot start the next pass.
pub fn run(
    rest: &impl Rest,
    clock: &impl Clock,
    settings: &Settings,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<()> {
    settings.validate()?;
    let next = Next {
        rest,
        clock,
        settings,
    };
    let path = format!(
        "repos/{}/actions/runs/{}",
        settings.repository, settings.run
    );
    let started = next.tries(err, &format!("GET {path}"), || {
        let at = get(rest, &path)?["run_started_at"]
            .as_str()
            .map(str::to_owned)
            .with_context(|| format!("no start time for run {}", settings.run))?;
        OffsetDateTime::parse(&at, &Rfc3339).with_context(|| format!("run_started_at {at:?}"))
    })?;
    let wait = wait_for(started, clock.now(), settings.interval);
    if settings.dry_run {
        writeln!(out, "would wait {}s", wait.as_secs())?;
    } else if !wait.is_zero() {
        writeln!(out, "waiting {}s", wait.as_secs())?;
        clock.sleep(wait);
    }
    for (wake, ignore_comments) in [(&settings.supervisor, true), (&settings.train, false)] {
        if let Some(wake) = wake
            && let Err(e) = next.wake(wake, ignore_comments, out, err)
        {
            writeln!(
                err,
                "warning: could not start {}: {e:#}; continuing the chain",
                wake.workflow.file
            )?;
        }
    }
    // A dispatch that failed may still have started a run, so the runs are
    // listed again before each try. That narrows, but cannot rule out, a
    // second chain.
    let file = &settings.pass.file;
    for attempt in 1..=TRIES {
        if attempt > 1 {
            clock.sleep(retry_wait(attempt));
        }
        if let Some(id) = newer_run(&next.pass_runs(err)?, settings)? {
            writeln!(out, "run {id} continues the chain; not starting another")?;
            return Ok(());
        }
        if settings.dry_run {
            return writeln!(out, "would start {file}").map_err(Into::into);
        }
        match next.dispatch(&settings.pass) {
            Ok(()) => return writeln!(out, "started {file}").map_err(Into::into),
            Err(e) => writeln!(err, "attempt {attempt} of starting {file} failed: {e:#}")?,
        }
    }
    bail!("starting {file} failed {TRIES} times")
}

struct Next<'a, R, C> {
    rest: &'a R,
    clock: &'a C,
    settings: &'a Settings,
}

impl<R: Rest, C: Clock> Next<'_, R, C> {
    /// `call` until it succeeds, at most [`TRIES`] times.
    fn tries<T>(
        &self,
        err: &mut impl Write,
        what: &str,
        mut call: impl FnMut() -> Result<T>,
    ) -> Result<T> {
        let mut attempt = 1;
        loop {
            match call() {
                Ok(value) => return Ok(value),
                Err(e) if attempt == TRIES => {
                    return Err(e.context(format!("{what} failed {TRIES} times")));
                }
                Err(e) => writeln!(err, "attempt {attempt} of {what} failed: {e:#}")?,
            }
            attempt += 1;
            if !self.settings.dry_run {
                self.clock.sleep(retry_wait(attempt));
            }
        }
    }

    /// Page `page` of `workflow`'s runs on the branch, newest first.
    fn runs_page(&self, err: &mut impl Write, workflow: &str, page: usize) -> Result<Vec<Value>> {
        let Settings {
            repository, branch, ..
        } = self.settings;
        let branch: String = url::form_urlencoded::byte_serialize(branch.as_bytes()).collect();
        let path = format!(
            "repos/{repository}/actions/workflows/{workflow}/runs?branch={branch}&exclude_pull_requests=true&per_page={RUN_PAGE}&page={page}"
        );
        self.tries(err, &format!("GET {path}"), || {
            let body = get(self.rest, &path)?;
            body["workflow_runs"]
                .as_array()
                .cloned()
                .with_context(|| format!("GET {path}: no workflow_runs"))
        })
    }

    /// The pass workflow's newest runs, back to this run if [`RUN_PAGES`]
    /// reach it.
    fn pass_runs(&self, err: &mut impl Write) -> Result<Vec<Value>> {
        let mut runs = Vec::new();
        for page in 1..=RUN_PAGES {
            let listed = self.runs_page(err, &self.settings.pass.file, page)?;
            let full = listed.len() >= RUN_PAGE;
            runs.extend(listed);
            if !full
                || runs
                    .iter()
                    .any(|r| r["id"].as_u64() == Some(self.settings.run))
            {
                break;
            }
        }
        Ok(runs)
    }

    /// Start an auxiliary workflow when its newest run that counts is stale. An
    /// unreadable listing is an error, never an absent run.
    fn wake(
        &self,
        supervisor: &Supervisor,
        ignore_comments: bool,
        out: &mut impl Write,
        err: &mut impl Write,
    ) -> Result<()> {
        let file = &supervisor.workflow.file;
        let newest = newest_auxiliary_run(&self.runs_page(err, file, 1)?, ignore_comments)?;
        let now = self.clock.now();
        if !supervisor_due(newest, now, supervisor.stale) {
            let age = newest.map_or(0, |created| (now - created).whole_minutes());
            return writeln!(out, "{file} ran {age} minutes ago; not starting it")
                .map_err(Into::into);
        }
        if self.settings.dry_run {
            return writeln!(out, "would start {file}").map_err(Into::into);
        }
        self.tries(err, &format!("starting {file}"), || {
            self.dispatch(&supervisor.workflow)
        })?;
        writeln!(out, "started {file}")?;
        Ok(())
    }

    /// One `workflow_dispatch` of `workflow` on the branch.
    fn dispatch(&self, workflow: &Workflow) -> Result<()> {
        let Settings {
            repository, branch, ..
        } = self.settings;
        let path = format!(
            "repos/{repository}/actions/workflows/{}/dispatches",
            workflow.file
        );
        let inputs: Map<String, Value> = (workflow.inputs.iter())
            .map(|(name, value)| (name.clone(), value.as_str().into()))
            .collect();
        let body = json!({ "ref": branch, "inputs": inputs });
        let (status, answer) = self
            .rest
            .send("POST", &format!("{API_ROOT}{path}"), &body)?;
        ensure!(
            (200..300).contains(&status),
            "POST {path}: HTTP {status}: {}",
            answer["message"].as_str().unwrap_or("message unavailable")
        );
        Ok(())
    }
}

/// One unconditional GET, which must answer 200.
fn get(rest: &impl Rest, path: &str) -> Result<Value> {
    let (status, body) = rest.send("GET", &format!("{API_ROOT}{path}"), &Value::Null)?;
    ensure!(status == 200, "GET {path}: HTTP {status}");
    Ok(body)
}

#[cfg(test)]
mod tests;
