//! `agent-board prune` (docs/prune.md): open issues that are candidates to
//! close, read-only.
use anyhow::{Context, Result};
use board_forge::github::Transport;
use board_forge::pass::Config;
use board_forge::poll::Rest;
use board_forge::prune::{self, Cache, DEFAULT_STALE_DAYS, PRUNE_REQUEST_LIMIT, STALE_DAYS_LIMIT};
use std::{io::Write, path::PathBuf};

#[derive(clap::Args)]
pub struct Prune {
    /// The pass configuration (docs/pass.md), from a trusted default-branch checkout: its `workRepositories` are read, `board` gives each issue's Priority and `bot` names the bot whose activity does not count. GH_TOKEN reads; nothing is written to the forge.
    #[arg(long, value_name = "FILE")]
    pub config: PathBuf,
    /// Days without activity by a person or a pull request after which an issue at neither P0 nor P1 is stale (1 to 365).
    #[arg(long, value_name = "DAYS", default_value_t = DEFAULT_STALE_DAYS,
        value_parser = clap::value_parser!(u64).range(1..=STALE_DAYS_LIMIT))]
    pub stale_days: u64,
    /// The response cache: read if it exists, so unchanged pages are revalidated and cost no rate budget, then replaced by the responses this read used.
    #[arg(long, value_name = "FILE")]
    pub cache: Option<PathBuf>,
    /// Output format.
    #[arg(long, default_value = "text", value_parser = ["text", "json"])]
    pub format: String,
}

/// List the candidates. The configuration and cache are checked before a
/// token is read; the cache is written only after a whole read.
pub fn run<C: Rest + Transport>(
    args: &Prune,
    client: impl FnOnce() -> Result<C>,
    out: &mut impl Write,
) -> Result<()> {
    let read = |path: &PathBuf| {
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
    };
    let config = Config::parse(&read(&args.config)?)
        .with_context(|| format!("{}", args.config.display()))?;
    prune::check(&config, args.stale_days).with_context(|| format!("{}", args.config.display()))?;
    let cache = match &args.cache {
        Some(path) if path.exists() => {
            let value = serde_json::from_str(&read(path)?)
                .with_context(|| format!("parse {}", path.display()))?;
            Cache::from_json(&value).with_context(|| format!("{}", path.display()))?
        }
        _ => Cache::default(),
    };
    let now = time::OffsetDateTime::now_utc();
    let limits = (PRUNE_REQUEST_LIMIT, args.stale_days);
    let (report, used) = prune::read(&client()?, &config, cache, limits, now)?;
    if let Some(path) = &args.cache {
        std::fs::write(path, serde_json::to_string(&used.to_json())?)
            .with_context(|| format!("write {}", path.display()))?;
    }
    if args.format == "json" {
        writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
    } else {
        write!(out, "{}", report.text())?;
    }
    Ok(())
}
