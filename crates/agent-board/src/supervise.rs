use anyhow::{Context, Result, ensure};
use board_forge::supervise::{self, Api, Bounded, Settings, agent, health};
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
    /// The agent run's result URL: its comment on the requests issue.
    #[arg(long, value_name = "URL")]
    result_url: String,
}

fn read(path: &Path) -> Result<Value> {
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
            let mut nonce = [0u8; 16];
            getrandom::fill(&mut nonce).map_err(|_| anyhow::anyhow!("generate delimiter"))?;
            let delimiter: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
            let out = trigger_output(
                &decision,
                policy.as_ref(),
                &pools,
                checks,
                &format!("task-{delimiter}"),
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
            let run = agent::Run {
                result_url: args.result_url.clone(),
                run_url: format!(
                    "{}/{}/actions/runs/{}",
                    env("GITHUB_SERVER_URL")?,
                    env("GITHUB_REPOSITORY")?,
                    env("GITHUB_RUN_ID")?
                ),
                default_branch: env("DEFAULT_BRANCH")?,
                attempt: env("GITHUB_RUN_ATTEMPT")?
                    .parse()
                    .context("GITHUB_RUN_ATTEMPT")?,
                now: now(),
            };
            let outcome = agent::execute(&client, &policy, &run);
            let body = agent::record(&client, &policy, &run, &outcome)?;
            writeln!(std::io::stdout().lock(), "{body}")?;
            ensure!(outcome.ok, "supervisor: not every request was performed");
            Ok(())
        }
    }
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
    ensure!(!task.contains(delimiter), "task contains its delimiter");
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
    out.push_str(&format!("task<<{delimiter}\n{task}\n{delimiter}\n"));
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
    }
}
