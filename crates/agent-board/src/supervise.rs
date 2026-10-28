use anyhow::{Context, Result, ensure};
use board_forge::supervise::{self, Api, Bounded, Settings, agent, health, pace, profile};
use serde_json::Value;
use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[derive(clap::Args)]
pub struct Supervise {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Read the health observations and write the report (read-only; GH_TOKEN reads).
    Health(Health),
    /// Say whether the agent runs on this pass and print its inputs for $GITHUB_OUTPUT.
    Trigger(Trigger),
    /// Perform the agent's validated dispatch requests and log them (writes with GH_TOKEN, the bot's).
    Execute(Execute),
    /// Say whether this pass is a sweep, from the previous run's times, for $GITHUB_OUTPUT (offline).
    Due(Due),
    /// Say whether to skip the trigger because no check newly fails, for $GITHUB_OUTPUT (offline).
    Repeat(Repeat),
    /// Compose the agent's task from the pinned profile, the trigger's task and the operator request (GH_TOKEN reads).
    Compose(Compose),
    /// Print the agent's result comment without applying it (GH_TOKEN reads).
    Preview(Preview),
    /// Print an operator session's wrapper inputs for $GITHUB_OUTPUT (GH_TOKEN reads).
    Session(Session),
}

/// Repository-owned settings from a trusted default-branch checkout, never a
/// dispatched pull request or workflow input.
#[derive(clap::Args)]
struct Files {
    /// The poll's agent-board.json: operator, bot, dispatch repository and workflow, follow.
    #[arg(long, value_name = "FILE")]
    config: PathBuf,
    /// The supervisor's settings: workflows, thresholds, the release and wrapper to compare, the agent.
    #[arg(long, value_name = "FILE")]
    supervisor_config: PathBuf,
    /// Where the usage snapshot is ({repo, issue, bot}); without it the pace check is unavailable.
    #[arg(long, value_name = "FILE")]
    usage_config: Option<PathBuf>,
}

#[derive(clap::Args)]
struct Health {
    #[command(flatten)]
    files: Files,
    /// The previous pass's report, for how long an observation has been unavailable.
    #[arg(long, value_name = "FILE")]
    previous: Option<PathBuf>,
    /// Where the report is written.
    #[arg(long, value_name = "FILE", default_value = "health.json")]
    out: PathBuf,
}

#[derive(clap::Args)]
struct Trigger {
    #[command(flatten)]
    files: Files,
    /// The pools file naming each pool's agent, model and budget.
    #[arg(long, value_name = "FILE")]
    pools: PathBuf,
    /// The report `health` wrote.
    #[arg(long, value_name = "FILE", default_value = "health.json")]
    health: PathBuf,
    /// The daily pass, which runs the agent with every check passing.
    #[arg(long)]
    daily: bool,
}

#[derive(clap::Args)]
struct Execute {
    #[command(flatten)]
    files: Files,
    /// The pools file; a request must name one of its pools.
    #[arg(long, value_name = "FILE")]
    pools: PathBuf,
    #[command(flatten)]
    source: Source,
    /// The previous executor run's ledger, an artifact of that run; leave it out when none was found.
    #[arg(
        long,
        value_name = "FILE",
        requires = "ledger_run",
        conflicts_with = "result_url"
    )]
    ledger: Option<PathBuf>,
    /// The id of the run whose artifact --ledger is.
    #[arg(long, value_name = "ID", requires = "ledger")]
    ledger_run: Option<u64>,
    /// Where this run's ledger is written for the next run; with --requests, the day's count comes from it, not the log issue.
    #[arg(
        long,
        value_name = "FILE",
        conflicts_with = "result_url",
        required_unless_present = "result_url"
    )]
    ledger_out: Option<PathBuf>,
}

/// Where the agent's requests are: one of the two.
#[derive(clap::Args)]
#[group(required = true, multiple = false)]
struct Source {
    /// The agent run's result URL: its comment on the requests issue.
    #[arg(long, value_name = "URL")]
    result_url: Option<String>,
    /// The agent run's safe outputs (JSON Lines), handed back as an artifact of this run.
    #[arg(long, value_name = "FILE")]
    requests: Option<PathBuf>,
}

#[derive(clap::Args)]
struct Due {
    /// The event that started the run (`github.event_name`).
    #[arg(long)]
    event: String,
    /// The dispatch's prompt; one makes a prompt-less dispatch an explicit request.
    #[arg(long, allow_hyphen_values = true)]
    prompt: Option<String>,
    /// The previous run's times. Give it whenever the lookup worked, even if
    /// there was no previous run; left out, the times are unknown and only an
    /// explicit request is a sweep.
    #[arg(long, value_name = "FILE")]
    previous: Option<PathBuf>,
    /// Where this run's times are written, for the next run to read.
    #[arg(long, value_name = "FILE", default_value = "times.json")]
    out: PathBuf,
}

#[derive(clap::Args)]
struct Repeat {
    #[command(flatten)]
    files: Files,
    /// The report `health` wrote.
    #[arg(long, value_name = "FILE", default_value = "health.json")]
    health: PathBuf,
    /// The previous run's report.
    #[arg(long, value_name = "FILE")]
    previous: Option<PathBuf>,
    /// A sweep, which never skips.
    #[arg(long)]
    daily: bool,
}

#[derive(clap::Args)]
struct Compose {
    #[command(flatten)]
    files: Files,
    /// The task `trigger` printed.
    #[arg(long, allow_hyphen_values = true)]
    task: String,
    /// The dispatch's prompt; an operator comment's body takes its place.
    #[arg(long, allow_hyphen_values = true)]
    prompt: Option<String>,
    /// The event payload (`$GITHUB_EVENT_PATH`).
    #[arg(long, value_name = "FILE")]
    event: PathBuf,
}

#[derive(clap::Args)]
struct Preview {
    #[command(flatten)]
    files: Files,
    #[command(flatten)]
    source: Source,
}

#[derive(clap::Args)]
struct Session {
    #[command(flatten)]
    files: Files,
    /// The session settings: enabled, repository, issue, idleMinutes.
    #[arg(long, value_name = "FILE")]
    session: PathBuf,
    /// The session's own rules, put before the supervisor's profile.
    #[arg(long, value_name = "FILE")]
    rules: PathBuf,
    /// The pools file.
    #[arg(long, value_name = "FILE")]
    pools: PathBuf,
    /// The pool, of the pools file, the session runs on.
    #[arg(long)]
    pool: String,
    /// The idle timeout, 1 to 60 minutes; empty uses the session settings'.
    #[arg(long)]
    idle_minutes: Option<String>,
    /// The opening prompt, task data.
    #[arg(long, allow_hyphen_values = true)]
    prompt: Option<String>,
}

pub(crate) fn read(path: &Path) -> Result<Value> {
    serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("decode {}", path.display()))
}

fn settings(files: &Files) -> Result<Settings> {
    let usage = files.usage_config.as_deref().map(read).transpose()?;
    Settings::from_json(
        &read(&files.config)?,
        &read(&files.supervisor_config)?,
        usage.as_ref(),
    )
}

fn env(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .with_context(|| format!("{name} is not set"))
}

fn now() -> i64 {
    supervise::now_millis(time::OffsetDateTime::now_utc())
}

/// A value for a `name=value` line of `$GITHUB_OUTPUT`.
fn output_line(name: &str, value: &str) -> Result<String> {
    ensure!(
        !value.contains(['\n', '\r']),
        "{name} must be a single line"
    );
    Ok(format!("{name}={value}\n"))
}

/// A multiline value for `$GITHUB_OUTPUT`, under a delimiter it cannot contain.
fn output_block(name: &str, value: &str, delimiter: &str) -> Result<String> {
    ensure!(!value.contains(delimiter), "{name} contains its delimiter");
    Ok(format!("{name}<<{delimiter}\n{value}\n{delimiter}\n"))
}

/// A random delimiter, which text written before it was drawn cannot hold.
fn delimiter(prefix: &str) -> Result<String> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| anyhow::anyhow!("generate delimiter"))?;
    Ok(nonce.iter().fold(format!("{prefix}-"), |mut s, b| {
        s.push_str(&format!("{b:02x}"));
        s
    }))
}

/// The agent's settings, which `compose`, `preview` and `session` need.
fn agent_settings(s: &Settings) -> Result<&supervise::AgentSettings> {
    s.agent
        .as_ref()
        .context("supervisor: the requests and log issues are not configured")
}

fn pinned_profile(s: &Settings) -> Result<&supervise::profile::Profile> {
    agent_settings(s)?
        .profile
        .as_ref()
        .context("supervisor agent.profile is not configured")
}

/// A file of at most `max` bytes.
fn bounded(path: &Path, max: usize) -> Result<String> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("read {}", path.display()))?
        .len();
    ensure!(size <= max as u64, "{} exceeds {max} bytes", path.display());
    std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

/// The agent's safe outputs, read only up to their bound.
fn outputs(path: &Path) -> Result<String> {
    bounded(path, agent::handback::MAX_OUTPUTS_BYTES)
}

/// The previous run's ledger, read only up to its bound.
fn ledger(path: &Path) -> Result<Value> {
    serde_json::from_str(&bounded(path, agent::handback::MAX_LEDGER_BYTES)?)
        .with_context(|| format!("decode {}", path.display()))
}

/// A file that may be absent: `None` when it cannot be read or decoded.
fn read_optional(path: Option<&Path>) -> Option<Value> {
    path.and_then(|p| read(p).ok())
}

pub fn run(args: &Supervise) -> Result<()> {
    match &args.command {
        Command::Health(args) => {
            let s = settings(&args.files)?;
            // No previous report starts each unavailable observation's clock afresh.
            let previous = args.previous.as_deref().and_then(|path| {
                read(path)
                    .ok()
                    .and_then(|v| v.as_array().cloned())
                    .or_else(|| {
                        eprintln!("previous report {} unreadable; ignored", path.display());
                        None
                    })
            });
            let client = board_forge::github::Client::from_env()?;
            let clock = now;
            let bounded = Bounded::new(&client, &s.limits, &clock);
            let snapshot = health::collect(&bounded, &s, &env("DEFAULT_BRANCH")?, now(), previous);
            let checks = health::evaluate(&snapshot, now(), &s.limits);
            let mut report = serde_json::to_string_pretty(&checks)?;
            report.push('\n');
            std::fs::write(&args.out, report)
                .with_context(|| format!("write {}", args.out.display()))?;
            // Only fixed text: remote titles and private text stay out of the summary.
            let mut out = std::io::stdout().lock();
            for c in &checks {
                writeln!(
                    out,
                    "{}: {} — {}",
                    c["id"].as_str().unwrap_or_default(),
                    c["status"].as_str().unwrap_or_default(),
                    c["suggested_action"].as_str().unwrap_or_default()
                )?;
            }
            Ok(())
        }
        Command::Trigger(args) => {
            let s = settings(&args.files)?;
            let pools = read(&args.pools)?;
            let policy = agent::Policy::new(&s, &pools)?;
            let checks = read(&args.health)?;
            let checks = checks.as_array().context("health report is not a list")?;
            let repo = env("GITHUB_REPOSITORY")?;
            let run_id: u64 = env("GITHUB_RUN_ID")?.parse().context("GITHUB_RUN_ID")?;
            let client = board_forge::github::Client::from_env()?;
            let decision = agent::decide(checks, args.daily, policy.as_ref(), |hours| {
                agent::recent_agent(
                    &client as &dyn Api,
                    &repo,
                    &s.supervisor_workflow,
                    run_id,
                    now(),
                    hours,
                )
            })?;
            let out = trigger_output(
                &decision,
                policy.as_ref(),
                &pools,
                checks,
                &delimiter("task")?,
            )?;
            std::io::stdout().lock().write_all(out.as_bytes())?;
            eprintln!(
                "agent: {}, {}",
                if decision.act { "runs" } else { "skipped" },
                decision.reason
            );
            Ok(())
        }
        Command::Execute(args) => {
            let s = settings(&args.files)?;
            let pools = read(&args.pools)?;
            let policy = agent::Policy::new(&s, &pools)?
                .context("supervisor: the requests and log issues are not configured")?;
            let client = board_forge::github::Client::from_env()?;
            let repository = env("GITHUB_REPOSITORY")?;
            let run_id = env("GITHUB_RUN_ID")?;
            let source = match (&args.source.result_url, &args.source.requests) {
                (Some(url), _) => agent::Source::Comment(url.clone()),
                (None, Some(path)) => agent::Source::HandBack(agent::handback::HandBack {
                    outputs: outputs(path)?,
                    previous: match (&args.ledger, args.ledger_run) {
                        (Some(path), Some(run)) => Some((run, ledger(path)?)),
                        _ => None,
                    },
                    repository: repository.clone(),
                    run_id: run_id.parse().context("GITHUB_RUN_ID")?,
                    run_number: env("GITHUB_RUN_NUMBER")?
                        .parse()
                        .context("GITHUB_RUN_NUMBER")?,
                }),
                (None, None) => anyhow::bail!("--result-url or --requests is required"),
            };
            let run = agent::Run {
                source,
                run_url: format!(
                    "{}/{repository}/actions/runs/{run_id}",
                    env("GITHUB_SERVER_URL")?
                ),
                default_branch: env("DEFAULT_BRANCH")?,
                attempt: env("GITHUB_RUN_ATTEMPT")?
                    .parse()
                    .context("GITHUB_RUN_ATTEMPT")?,
                now: now(),
            };
            let outcome = agent::execute(&client, &policy, &run);
            // Written before the log, which may fail: without it the next
            // run blocks writes for a day.
            if let (Some(ledger), Some(out)) = (&outcome.ledger, &args.ledger_out) {
                let mut text = serde_json::to_string_pretty(&ledger.to_json())?;
                text.push('\n');
                std::fs::write(out, text).with_context(|| format!("write {}", out.display()))?;
            }
            let body = agent::record(&client, &policy, &run, &outcome)?;
            writeln!(std::io::stdout().lock(), "{body}")?;
            ensure!(outcome.ok, "supervisor: not every request was performed");
            Ok(())
        }
        Command::Due(args) => {
            // A previous run without a times file, or with one this cannot
            // read, is no previous sweep: Null.
            let previous = args
                .previous
                .as_deref()
                .map(|p| read_optional(Some(p)).unwrap_or_default());
            let (daily, times) = pace::due(
                &args.event,
                args.prompt.as_deref().unwrap_or_default(),
                previous.as_ref(),
                now(),
            );
            std::fs::write(&args.out, format!("{times}\n"))
                .with_context(|| format!("write {}", args.out.display()))?;
            // Every run is a health pass now that only the caller's crons
            // start it; `health` stays so that the caller's step conditions
            // keep reading it.
            let mut out = std::io::stdout().lock();
            out.write_all(output_line("health", "true")?.as_bytes())?;
            out.write_all(output_line("daily", &daily.to_string())?.as_bytes())?;
            Ok(())
        }
        Command::Repeat(args) => {
            let s = settings(&args.files)?;
            let enabled = s.agent.as_ref().is_some_and(|a| a.skip_unchanged_failures);
            let skip = pace::unchanged(
                enabled,
                args.daily,
                read_optional(Some(&args.health)).as_ref(),
                read_optional(args.previous.as_deref()).as_ref(),
            );
            if let Some(failing) = &skip {
                let failing: Vec<&str> = failing.iter().map(String::as_str).collect();
                eprintln!(
                    "{} failing as in the previous pass; the sweep reads them",
                    if failing.is_empty() {
                        "no check".to_owned()
                    } else {
                        failing.join(", ")
                    }
                );
            }
            std::io::stdout()
                .lock()
                .write_all(output_line("skip", &skip.is_some().to_string())?.as_bytes())?;
            Ok(())
        }
        Command::Compose(args) => {
            let s = settings(&args.files)?;
            let requests = &agent_settings(&s)?.requests;
            let request = profile::request(
                &read(&args.event)?,
                args.prompt.as_deref().unwrap_or_default(),
                &s.operator,
                requests,
            )?;
            let client = board_forge::github::Client::from_env()?;
            let text = pinned_profile(&s)?.read(&client)?;
            let task = profile::supervisor_task(&text, &args.task, &request)?;
            std::io::stdout()
                .lock()
                .write_all(output_block("task", &task, &delimiter("task")?)?.as_bytes())?;
            Ok(())
        }
        Command::Preview(args) => {
            let s = settings(&args.files)?;
            let requests = &agent_settings(&s)?.requests;
            let body = match (&args.source.result_url, &args.source.requests) {
                (_, Some(path)) => agent::handback::report(&outputs(path)?)?,
                (Some(url), None) => {
                    let id = agent::request_comment(url, requests)?;
                    let client = board_forge::github::Client::from_env()?;
                    let comment = client.get(&format!(
                        "/repos/{}/issues/comments/{id}",
                        requests.repository
                    ))?;
                    comment["body"]
                        .as_str()
                        .context("comment has no body")?
                        .to_owned()
                }
                (None, None) => anyhow::bail!("--result-url or --requests is required"),
            };
            let body = body.as_str();
            // The agent's text must not be read as workflow commands.
            let token = delimiter("preview")?;
            ensure!(!body.contains(&token), "comment contains its delimiter");
            write!(
                std::io::stdout().lock(),
                "::stop-commands::{token}\nPreview only; no requests applied.\n{body}\n::{token}::\n"
            )?;
            Ok(())
        }
        Command::Session(args) => {
            let s = settings(&args.files)?;
            let agent = agent_settings(&s)?;
            let rules = std::fs::read_to_string(&args.rules)
                .with_context(|| format!("read {}", args.rules.display()))?;
            let client = board_forge::github::Client::from_env()?;
            let outputs = profile::session(
                &read(&args.session)?,
                &read(&args.pools)?,
                &rules,
                agent.state.as_ref(),
                &agent.requests,
                &pinned_profile(&s)?.read(&client)?,
                &args.pool,
                args.idle_minutes.as_deref().unwrap_or_default(),
                args.prompt.as_deref().unwrap_or_default(),
            )?;
            std::io::stdout()
                .lock()
                .write_all(session_output(&outputs, &delimiter("session")?)?.as_bytes())?;
            Ok(())
        }
    }
}

/// A session's `$GITHUB_OUTPUT` lines: its task under a delimiter, every
/// other value a single line.
fn session_output(outputs: &profile::Outputs, delimiter: &str) -> Result<String> {
    outputs
        .iter()
        .map(|(name, value)| match *name {
            "task" => output_block(name, value, delimiter),
            _ => output_line(name, value),
        })
        .collect()
}

/// The trigger's `$GITHUB_OUTPUT` lines: `act`, and when the agent runs, its
/// wrapper inputs, the task under a delimiter it cannot contain.
fn trigger_output(
    decision: &agent::Decision,
    policy: Option<&agent::Policy>,
    pools: &Value,
    checks: &[Value],
    delimiter: &str,
) -> Result<String> {
    let mut out = output_line("act", &decision.act.to_string())?;
    let Some(policy) = policy.filter(|_| decision.act) else {
        return Ok(out);
    };
    let pool = &pools[policy.agent.pool.as_str()];
    let task = agent::compose(checks, policy, &decision.reason)?;
    out.push_str(&output_line("repo", &policy.agent.requests.repository)?);
    out.push_str(&output_line(
        "item",
        &policy.agent.requests.issue.to_string(),
    )?);
    for key in ["agent", "model", "budget"] {
        let value = pool[key]
            .as_str()
            .with_context(|| format!("pool {} has no {key}", policy.agent.pool))?;
        out.push_str(&output_line(key, value)?);
    }
    out.push_str(&output_block("task", &task, delimiter)?);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn trigger_output_is_line_safe() {
        let board = json!({"operator": "owner", "bot": "worker", "dispatchRepo": "o/control",
            "dispatchWorkflow": "dispatch.yml", "follow": ["o/control"], "epics": {"repository": "o/control"}});
        let supervisor = json!({"pollWorkflow": "poll.yml", "probeWorkflow": "probe.yml",
            "supervisorWorkflow": "supervisor.yml", "schedulerJob": "board", "implementBranchPrefix": "p-",
            "releaseRepository": "o/aj", "wrapperRepository": "o/actions", "wrapperPath": "w.yml",
            "agent": {"pool": "large", "cooldownHours": 1, "requests": {"repository": "o/control", "issue": 5},
                "log": {"repository": "o/operator", "issue": 2}, "kinds": ["research"]}});
        let s = Settings::from_json(&board, &supervisor, None).unwrap();
        let pools = json!({"large": {"agent": "a", "model": "m", "budget": "100"}});
        let policy = agent::Policy::new(&s, &pools).unwrap();
        let checks = vec![json!({"id": "release", "status": "fail"})];
        let run = agent::Decision {
            act: true,
            reason: "release failing".into(),
        };
        let out = trigger_output(&run, policy.as_ref(), &pools, &checks, "task-d").unwrap();
        assert!(out.starts_with(
            "act=true\nrepo=o/control\nitem=5\nagent=a\nmodel=m\nbudget=100\ntask<<task-d\n"
        ));
        assert!(out.ends_with("```\n\ntask-d\n"));
        let skip = agent::Decision {
            act: false,
            reason: "no check fails".into(),
        };
        assert_eq!(
            trigger_output(&skip, policy.as_ref(), &pools, &checks, "task-d").unwrap(),
            "act=false\n"
        );
        // A pool value that would add an output line, or a delimiter the
        // task holds, is refused.
        let forged = json!({"large": {"agent": "a\ntask=forged", "model": "m", "budget": "1"}});
        assert!(trigger_output(&run, policy.as_ref(), &forged, &checks, "task-d").is_err());
        assert!(trigger_output(&run, policy.as_ref(), &pools, &checks, "release").is_err());
        assert!(output_line("x", "a\rb").is_err());
        // A session's prompt is task data under the delimiter; a pool value
        // cannot add a line.
        let session = vec![
            ("agent", "a".to_owned()),
            ("task", "hello\nrepo=attacker/repo".to_owned()),
        ];
        assert_eq!(
            session_output(&session, "session-d").unwrap(),
            "agent=a\ntask<<session-d\nhello\nrepo=attacker/repo\nsession-d\n"
        );
        let forged = vec![("agent", "a\nrepo=x".to_owned())];
        assert!(session_output(&forged, "session-d").is_err());
        assert!(session_output(&session, "hello").is_err());
    }
}
