//! `agent-board pass` (docs/pass.md): the scheduled pass, previewed or applied.
use anyhow::{Context, Result, bail, ensure};
use board_forge::caps::Limits;
use board_forge::github::Client;
use board_forge::github::{GraphQl, Transport};
use board_forge::pass::{self, Config, Mode, PassRun, Recorder, Replay, Report, Seeded};
use board_forge::poll::{Rest, State, bounds::FixBounds};
use std::time::{Duration, Instant};
use std::{io::Write, path::PathBuf};
use tokio::runtime::Runtime;

#[derive(clap::Args)]
pub struct Pass {
    /// The pass configuration (docs/pass.md), from a trusted default-branch checkout. Credentials come from GH_TOKEN (the bot's) and DISPATCH_TOKEN, never the file.
    #[arg(long, value_name = "FILE")]
    pub config: PathBuf,
    /// The caller's agentic-job bounds file for its `fix` kind (such as .github/agentic-job/fix.toml), from the same trusted checkout, never a dispatched head or workflow input: a pull request whose repository is outside its `repos`, or whose head branch is outside its push_to_pull_request_branch `branches`, gets no fix, rebase or continuation round and is left to the operator. The policy step stays the boundary.
    #[arg(long, value_name = "FILE")]
    pub fix_bounds: Option<PathBuf>,
    /// Take the steps: follow up pull requests, publish the usage snapshot, apply epic plans, start the chosen runs, write board Status and, with holds, publish the hold snapshot as GH_TOKEN's bot, starting runs with DISPATCH_TOKEN if set. Without it the pass previews and writes nothing. In a GitHub Actions run (GITHUB_REPOSITORY, GITHUB_RUN_ID), work starts only while no other run of its workflow is in progress. Outside one, it needs both --max-dispatch and --max-writes, and is not serialized against the scheduled workflow.
    #[arg(long)]
    pub apply: bool,
    /// Start at most N agent runs in this pass: pull request fix, rebase and continuation rounds, an epic's planning run, and scheduled starts. The rest are held, each line saying so.
    #[arg(long, value_name = "N")]
    pub max_dispatch: Option<usize>,
    /// Take at most N other write steps in this pass: forge writes such as comments, labels, merges, the usage and hold snapshots, epic plans and Status (a pull request's comment, label, retitle, retarget, enqueue or merge step, the usage and hold comments' edits, an epic plan's receipt and each of its actions that writes, and each item's Status change). A step is taken whole or held whole, and an epic plan's steps together. A pass that made progress publishes its hold snapshot one past a full cap above 0, so its progress is kept.
    #[arg(long, value_name = "N")]
    pub max_writes: Option<usize>,
    /// Refuse the board Status stage whole when it would change more than N items' Status [default: 50]: the first pass over a board that drifted is looked at first. Pass a higher N once to let it through.
    #[arg(long, value_name = "N")]
    pub max_status_changes: Option<usize>,
    /// Preview a recorded pass (a replay fixture) at its clock instead of reading GitHub; needs no token. With --record, it seeds the recording instead: its answers stand in for live reads, such as a usage snapshot the token cannot read or the broker's /usage.
    #[arg(long, value_name = "FILE", conflicts_with = "apply")]
    pub replay: Option<PathBuf>,
    /// Preview live, and save every read and its answer to FILE as a replay fixture.
    #[arg(long, value_name = "FILE", conflicts_with = "apply")]
    pub record: Option<PathBuf>,
}

/// A preview through `live` whose reads are saved to `path` as a replay fixture.
fn record_pass<C: Rest + Transport + GraphQl + pass::Broker>(
    runtime: &Runtime,
    live: &C,
    config: &Config,
    (path, limits): (&PathBuf, Limits),
    now: time::OffsetDateTime,
) -> Result<Report> {
    let recorder = Recorder::new(live);
    let preview = (Mode::Preview, limits);
    let pass = pass::run(&recorder, &recorder, config, State::default(), preview, now);
    let report = runtime.block_on(pass)?;
    std::fs::write(path, recorder.fixture(now)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(report)
}

/// Set to `true` in every GitHub Actions run.
const ACTIONS_VAR: &str = "GITHUB_ACTIONS";

/// How an applying pass is kept from racing the scheduled one: in GitHub
/// Actions by its run ([`own_run`]); outside, only by its caps, so both must
/// be given. Checked before any token is read.
fn apply_mode(limits: Limits) -> Result<Option<PassRun>> {
    if std::env::var(ACTIONS_VAR).as_deref() == Ok("true") {
        return own_run().map(Some);
    }
    if limits.dispatch.is_none() || limits.writes.is_none() {
        bail!(
            "--apply outside GitHub Actions needs both --max-dispatch and --max-writes: a local pass is not serialized against the scheduled workflow, so its caps bound what it can do"
        );
    }
    Ok(None)
}

/// The GitHub Actions run this applying pass is in ([`PassRun`]).
fn own_run() -> Result<PassRun> {
    let var = |name| {
        std::env::var(name).with_context(|| {
            format!(
                "--apply runs in a GitHub Actions run, which sets {name}, so that two passes never start work at once"
            )
        })
    };
    let repository = var("GITHUB_REPOSITORY")?;
    board_forge::poll::validate_repository(&repository).context("GITHUB_REPOSITORY")?;
    let id = var("GITHUB_RUN_ID")?.parse().context("GITHUB_RUN_ID")?;
    Ok(PassRun { repository, id })
}

fn read(path: &PathBuf) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

/// One pass: a line per decision on stdout, after one saying whether it
/// previews or applies, and a summary of its caps last. Any failed operation
/// fails the command, after the rest of the pass has run.
pub fn run(args: &Pass, runtime: &Runtime, out: &mut impl Write) -> Result<()> {
    let mut config = Config::parse(&read(&args.config)?)
        .with_context(|| format!("{}", args.config.display()))?;
    if let Some(board) = config.forgejo_board() {
        // Stop unsupported operations before opening files or looking up any
        // credentials, including the GitHub apply/record/replay paths below.
        for (supplied, option, reason) in [
            (args.apply, "--apply", "Forgejo scheduling needs #513"),
            (
                args.fix_bounds.is_some(),
                "--fix-bounds",
                "PR follow-up has no reviewed Forgejo equivalent",
            ),
            (
                args.record.is_some(),
                "--record",
                "a Forgejo recorder is not built; use fake-transport tests",
            ),
            (
                args.replay.is_some(),
                "--replay",
                "a Forgejo replay is not built; use fake-transport tests",
            ),
        ] {
            ensure!(!supplied, "unsupported_capability: {option}: {reason}");
        }
        let limits = Limits {
            dispatch: args.max_dispatch,
            writes: args.max_writes,
            status_changes: args.max_status_changes,
        };
        if let Some(warning) = board.plain_http_warning() {
            eprintln!("{warning}");
        }
        let client = board_forge::forgejo::Client::from_env(board)?;
        let broker = pass::HttpBroker::default();
        writeln!(out, "pass: preview")?;
        let report = pass::forgejo::run(
            &client,
            &broker,
            &config,
            (Mode::Preview, limits),
            time::OffsetDateTime::now_utc(),
        )?;
        return print_report(&report, out);
    }
    config.fix_bounds = args
        .fix_bounds
        .as_deref()
        .map(FixBounds::read)
        .transpose()?;
    let limits = Limits {
        dispatch: args.max_dispatch,
        writes: args.max_writes,
        status_changes: args.max_status_changes,
    };
    let own = args.apply.then(|| apply_mode(limits)).transpose()?;
    writeln!(
        out,
        "pass: {}",
        if args.apply { "apply" } else { "preview" }
    )?;
    if let Some(None) = own {
        writeln!(
            out,
            "warning: applying outside GitHub Actions, not serialized against the scheduled workflow's passes"
        )?;
    }
    let replay = match &args.replay {
        Some(path) => {
            let value = serde_json::from_str(&read(path)?)
                .with_context(|| format!("decode {}", path.display()))?;
            Some(Replay::from_json(&value)?)
        }
        None => None,
    };
    let report = match (&replay, &args.record) {
        (Some(replay), None) => {
            let state = State::default();
            let preview = (Mode::Preview, limits);
            runtime.block_on(pass::run(
                replay, replay, &config, state, preview, replay.now,
            ))?
        }
        (seed, record) => {
            let now = time::OffsetDateTime::now_utc();
            // No wait under a rate limit outlasts the pass's time bound,
            // which starts with it.
            let deadline = Instant::now() + Duration::from_secs(pass::PASS_SECONDS_LIMIT);
            let client = Client::from_env()?.retry_within(deadline);
            match (seed, record) {
                (Some(seed), Some(path)) => {
                    let seeded = Seeded {
                        seed,
                        live: &client,
                    };
                    record_pass(runtime, &seeded, &config, (path, limits), now)?
                }
                (None, Some(path)) => record_pass(runtime, &client, &config, (path, limits), now)?,
                (_, None) => {
                    // Only an applying pass writes the cache; a preview reads it.
                    let state = match config.state.as_deref().map(PathBuf::from) {
                        Some(path) if path.exists() => {
                            State::from_json(&serde_json::from_str(&read(&path)?)?)?
                        }
                        _ => State::default(),
                    };
                    let mode = match &own {
                        None => Mode::Preview,
                        Some(Some(own)) => Mode::Actions(own),
                        Some(None) => Mode::Local,
                    };
                    let dispatcher = Client::from_var("DISPATCH_TOKEN")
                        .map(|dispatcher| dispatcher.retry_within(deadline));
                    let dispatcher = dispatcher.as_ref().unwrap_or(&client);
                    let pass = pass::run(&client, dispatcher, &config, state, (mode, limits), now);
                    let report = runtime.block_on(pass)?;
                    if let (true, Some(path), Some(state)) =
                        (args.apply, &config.state, &report.state)
                    {
                        std::fs::write(path, serde_json::to_vec(&state.to_json())?)
                            .with_context(|| format!("write {path}"))?;
                    }
                    report
                }
            }
        }
    };
    print_report(&report, out)
}

fn print_report(report: &Report, out: &mut impl Write) -> Result<()> {
    for line in &report.lines {
        writeln!(out, "{line}")?;
    }
    writeln!(out, "{}", report.summary_line())?;
    ensure!(
        report.failures == 0,
        "{} operation(s) of the pass failed",
        report.failures
    );
    Ok(())
}
