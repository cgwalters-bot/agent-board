//! Explicit, read-only rolling CI report; never invoked by the supervisor.
use anyhow::{Context, Result, ensure};
use board_forge::ci_health::{Config, collect, human};
use board_forge::supervise::Api;
use std::{
    io::{Read, Write},
    path::PathBuf,
};

#[derive(clap::Args)]
pub struct CiHealth {
    /// Trusted JSON with explicit targets and an optional reviewed classification index.
    #[arg(long, value_name = "FILE")]
    config: PathBuf,
    /// Exclusive UTC date YYYY-MM-DD; defaults to today (30 complete UTC days).
    #[arg(long)]
    until: Option<String>,
    /// Stable JSON report or a concise human summary with coverage warnings.
    #[arg(long, default_value = "json", value_parser = ["json", "human"])]
    format: String,
}

pub fn run<A: Api>(
    args: &CiHealth,
    client: impl FnOnce() -> Result<A>,
    out: &mut impl Write,
) -> Result<()> {
    let today = time::OffsetDateTime::now_utc().date();
    let date = args
        .until
        .as_deref()
        .map(board_forge::cost::parse_date)
        .transpose()?
        .unwrap_or(today);
    ensure!(date <= today, "until cannot be after today");
    let until = date.midnight().assume_utc().unix_timestamp() * 1000;
    let mut text = String::new();
    std::fs::File::open(&args.config)
        .with_context(|| format!("read {}", args.config.display()))?
        .take(1024 * 1024 + 1)
        .read_to_string(&mut text)?;
    let config = Config::parse(&text, until)?;
    let client = client()?;
    let clock = || (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    let report = collect(&client, &config, until, &clock);
    if args.format == "human" {
        write!(out, "{}", human(&report))?;
    } else {
        writeln!(out, "{report:#}")?;
    }
    Ok(())
}
