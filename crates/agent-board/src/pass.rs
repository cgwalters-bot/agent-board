//! `agent-board pass` (docs/pass.md): the scheduled pass, previewed or applied.
use anyhow::{Context, Result, ensure};
use board_forge::github::Client;
use board_forge::github::Transport;
use board_forge::pass::{self, Config, PassRun, Recorder, Replay, Report, Seeded};
use board_forge::poll::{Rest, State};
use std::{io::Write, path::PathBuf};

#[derive(clap::Args)]
pub struct Pass {
    /// The pass configuration (docs/pass.md), from a trusted default-branch checkout. Credentials come from GH_TOKEN (the bot's) and DISPATCH_TOKEN, never the file.
    #[arg(long, value_name = "FILE")]
    pub config: PathBuf,
    /// Take the steps: follow up pull requests and start the chosen runs as GH_TOKEN's bot, starting runs with DISPATCH_TOKEN if set. Without it the pass previews and writes nothing. Only in a GitHub Actions run (GITHUB_REPOSITORY, GITHUB_RUN_ID): work starts only while no other run of its workflow is in progress.
    #[arg(long)]
    pub apply: bool,
    /// Preview a recorded pass (a replay fixture) at its clock instead of reading GitHub; needs no token. With --record, it seeds the recording instead: its answers stand in for live reads, such as a usage snapshot the token cannot read.
    #[arg(long, value_name = "FILE", conflicts_with = "apply")]
    pub replay: Option<PathBuf>,
    /// Preview live, and save every read and its answer to FILE as a replay fixture.
    #[arg(long, value_name = "FILE", conflicts_with = "apply")]
    pub record: Option<PathBuf>,
}

/// A preview through `live` whose reads are saved to `path` as a replay fixture.
fn record_pass<C: Rest + Transport>(
    live: &C,
    config: &Config,
    path: &PathBuf,
    now: time::OffsetDateTime,
) -> Result<Report> {
    let recorder = Recorder::new(live);
    let report = pass::run(&recorder, &recorder, config, State::default(), None, now)?;
    std::fs::write(path, recorder.fixture(now)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(report)
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
/// previews or applies. Any failed operation fails the command, after the rest
/// of the pass has run.
pub fn run(args: &Pass, out: &mut impl Write) -> Result<()> {
    let config = Config::parse(&read(&args.config)?)
        .with_context(|| format!("{}", args.config.display()))?;
    writeln!(
        out,
        "pass: {}",
        if args.apply { "apply" } else { "preview" }
    )?;
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
            pass::run(replay, replay, &config, state, None, replay.now)?
        }
        (seed, record) => {
            let now = time::OffsetDateTime::now_utc();
            let client = Client::from_env()?;
            match (seed, record) {
                (Some(seed), Some(path)) => {
                    let seeded = Seeded {
                        seed,
                        live: &client,
                    };
                    record_pass(&seeded, &config, path, now)?
                }
                (None, Some(path)) => record_pass(&client, &config, path, now)?,
                (_, None) => {
                    // Only an applying pass writes the cache; a preview reads it.
                    let state = match config.state.as_deref().map(PathBuf::from) {
                        Some(path) if path.exists() => {
                            State::from_json(&serde_json::from_str(&read(&path)?)?)?
                        }
                        _ => State::default(),
                    };
                    let own = args.apply.then(own_run).transpose()?;
                    let dispatcher = Client::from_var("DISPATCH_TOKEN");
                    let dispatcher = dispatcher.as_ref().unwrap_or(&client);
                    let report = pass::run(&client, dispatcher, &config, state, own.as_ref(), now)?;
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
    for line in &report.lines {
        writeln!(out, "{line}")?;
    }
    ensure!(
        report.failures == 0,
        "{} operation(s) of the pass failed",
        report.failures
    );
    Ok(())
}
