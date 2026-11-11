//! Standalone promotion; preview by default, publication explicitly opted in.
use anyhow::{Context, Result, ensure};
use board_forge::{
    github::Client,
    pass::{Config, PASS_SECONDS_LIMIT, Recorder, Replay},
    promote,
};
use std::{
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(clap::Args)]
pub struct Promote {
    /// Publish at most one approved candidate; also requires promote.publish.
    #[arg(long, conflicts_with_all = ["record", "replay"], requires = "max_writes")]
    pub apply: bool,
    /// Whole-step publication cap (three writes, or four with a rebase/sign-off branch).
    #[arg(long, requires = "apply")]
    pub max_writes: Option<usize>,
    /// Trusted shared pass configuration; promote.forks absent or empty means off, with no forge reads or token lookup. The pass uses it only for read-only awaiting-candidate diagnostics, never publication. With forks, GH_TOKEN reads within the pass's request/time bounds and ten 100-row pages per listing.
    #[arg(long, value_name = "FILE")]
    pub config: PathBuf,
    /// Save live read responses projected to the evidence fields used here for offline replay. No forge writes. Record public repositories only before publishing the file; comment bodies remain in it.
    #[arg(long, value_name = "FILE", conflicts_with = "replay")]
    pub record: Option<PathBuf>,
    /// Preview recorded reads at their recorded clock, without credentials.
    #[arg(long, value_name = "FILE")]
    pub replay: Option<PathBuf>,
}

pub fn run(args: &Promote, out: &mut impl Write) -> Result<()> {
    let read = |path: &PathBuf| {
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
    };
    let config = Config::parse_sources(&read(&args.config)?)?;
    ensure!(
        config.forgejo_board().is_none(),
        "promote supports GitHub only"
    );
    if args.apply && promote::enabled(&config) {
        ensure!(
            config.promote.as_ref().is_some_and(|p| p.publish),
            "promote.publish must be true for --apply"
        );
    }
    let replay = args
        .replay
        .as_ref()
        .map(|path| -> Result<Replay> { Replay::from_json(&serde_json::from_str(&read(path)?)?) })
        .transpose()?;
    let lines = if !promote::enabled(&config) {
        if let Some(path) = &args.record {
            let now = time::OffsetDateTime::now_utc();
            let empty = Replay {
                now,
                requests: vec![],
            };
            std::fs::write(path, Recorder::new(&empty).fixture(now)?)
                .with_context(|| format!("write {}", path.display()))?;
        }
        vec!["promote: off".to_owned()]
    } else if let Some(replay) = &replay {
        preview(replay, &config, replay.now)?
    } else {
        let now = time::OffsetDateTime::now_utc();
        let client = Client::from_env()?
            .retry_within(Instant::now() + Duration::from_secs(PASS_SECONDS_LIMIT));
        if let Some(path) = &args.record {
            let recorder = Recorder::new(&client);
            let result = preview(&recorder, &config, now);
            std::fs::write(path, promote::recording(&recorder.fixture(now)?)?)
                .with_context(|| format!("write {}", path.display()))?;
            result?
        } else if args.apply {
            let mut lines = Vec::new();
            let resolved = config.resolve(&client, &mut lines)?;
            lines.extend(promote::apply(
                &client,
                resolved.as_ref().unwrap_or(&config),
                now,
                args.max_writes.context("--max-writes required")?,
            )?);
            lines
        } else {
            preview(&client, &config, now)?
        }
    };
    for line in lines {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

fn preview<C: board_forge::poll::Rest + board_forge::github::Transport>(
    client: &C,
    config: &Config,
    now: time::OffsetDateTime,
) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let resolved = config.resolve(client, &mut lines)?;
    lines.extend(promote::preview(
        client,
        resolved.as_ref().unwrap_or(config),
        now,
    )?);
    Ok(lines)
}
