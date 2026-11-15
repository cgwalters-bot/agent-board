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
    /// One-time repair of bare references in bot-owned midstream drafts, not publication. Preview by default; --apply --max-writes 1..10 edits bodies and logs each repair. Ambiguous sources hold.
    #[arg(long, conflicts_with = "repair_bodies")]
    pub repair_drafts: bool,
    /// Whole-step publication cap (three writes, or four with a rebase/sign-off branch); with --repair-bodies, the most draft bodies edited.
    #[arg(long, requires = "apply")]
    pub max_writes: Option<usize>,
    /// One-time repair instead of promotion (agent-board#1002): normalize source bookkeeping in the bot's open drafts in verified midstream forks to one full-URL source line, dropping forge-internal reference lines, and log each before/after or why a draft is left for manual repair. Preview by default; --apply --max-writes N edits at most N fork draft bodies, never anything upstream, and does not need promote.publish.
    #[arg(long, conflicts_with_all = ["record", "replay"])]
    pub repair_bodies: bool,
    /// Trusted shared pass configuration. Promotion discovers midstream forks from origin=fork and bot-midstream metadata; legacy promote.forks remains readable for one release as checked parent/base hints. Without promote settings or any discovery scope, off with no token lookup. Reads use the pass's request/time bounds.
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
    if args.repair_bodies && !promote::enabled(&config) {
        writeln!(out, "promote: off")?;
        return Ok(());
    }
    if args.repair_bodies {
        let client = Client::from_env()?
            .retry_within(Instant::now() + Duration::from_secs(PASS_SECONDS_LIMIT));
        let mut lines = Vec::new();
        let resolved = config.resolve(&client, &mut lines)?;
        lines.extend(promote::repair(
            &client,
            resolved.as_ref().unwrap_or(&config),
            args.max_writes.filter(|_| args.apply),
        )?);
        for line in lines {
            writeln!(out, "{line}")?;
        }
        return Ok(());
    }
    if args.apply && !args.repair_drafts && promote::enabled(&config) {
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
        if args.repair_drafts {
            repair(replay, &config, 0)?
        } else {
            preview(replay, &config, replay.now)?
        }
    } else {
        let now = time::OffsetDateTime::now_utc();
        let client = Client::from_env()?
            .retry_within(Instant::now() + Duration::from_secs(PASS_SECONDS_LIMIT));
        if let Some(path) = &args.record {
            let recorder = Recorder::new(&client);
            let result = if args.repair_drafts {
                repair(&recorder, &config, 0)
            } else {
                preview(&recorder, &config, now)
            };
            std::fs::write(path, promote::recording(&recorder.fixture(now)?)?)
                .with_context(|| format!("write {}", path.display()))?;
            result?
        } else if args.repair_drafts {
            repair(
                &client,
                &config,
                if args.apply {
                    args.max_writes
                        .filter(|n| *n > 0)
                        .context("repair requires --max-writes 1..10")?
                } else {
                    0
                },
            )?
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

fn repair<C: board_forge::poll::Rest + board_forge::github::Transport>(
    client: &C,
    config: &Config,
    max_writes: usize,
) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let resolved = config.resolve(client, &mut lines)?;
    lines.extend(promote::repair_drafts(
        client,
        resolved.as_ref().unwrap_or(config),
        max_writes,
    )?);
    Ok(lines)
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
