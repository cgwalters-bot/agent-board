//! Explicit, read-only rolling CI report; never invoked by the supervisor.
use anyhow::{Context, Result, ensure};
use board_forge::ci_health::{Config, collect, human};
use board_forge::supervise::Api;
use std::{
    io::{Read, Write},
    path::PathBuf,
};

pub const CI_HEALTH_ABOUT: &str = "Report rolling 30-day CI observations for explicit GitHub repositories and workflow files. Use this for historical first-attempt rates and recurring failure evidence, not supervise health's one-day signature check or latest-main verdict.

Supply --config from a trusted local checkout and GH_TOKEN or GITHUB_TOKEN with read access to the targets' Actions runs, jobs and logs. There is no organization discovery or implicit board scope. The exclusive --until date must be YYYY-MM-DD and no later than today (UTC); it defaults to today's UTC midnight.

Prints agent-board/ci-health/v1 JSON to stdout by default; --format human prints a concise summary. Exit zero means a report was produced, not that CI is healthy or coverage complete. Check complete at both report and target levels and display warnings before using rates. Invalid configuration or authentication setup fails the command. Consumers must use JSON, not scrape the human format.

Reads are bounded to 500 API operations, a 120-second collection deadline, ten 100-item pages per listing, 1 MiB per log and 32 MiB aggregate decoded bytes. In-flight requests can overrun the deadline. Busy windows and unavailable logs can leave coverage incomplete.

Nothing invokes this command by default. It never writes to the forge, files issues, retries runs, quarantines, dispatches, posts upstream or schedules anything. See docs/ci-health.md for window, denominator, signature and coverage contracts.";

#[derive(clap::Args)]
#[command(after_long_help = "Examples:
  Print the rolling report ending at today's UTC midnight:
    agent-board ci-health --config ci-health.json

  Print a human summary for a fixed exclusive end date:
    agent-board ci-health --config ci-health.json --until 2026-10-28 --format human

  Configuration shape (save as ci-health.json in the trusted checkout):
    {\"targets\":[{\"repository\":\"cgwalters-forge/agent-board\",\"workflow\":\"ci.yml\"}],\"index\":{}}

  Configure 1 to 20 unique repository/workflow targets. Unknown fields are rejected.
  The optional index maps a prior report's 64-hex signature to a reviewed
  classification; observations never assign authoritative classes.
  Config field contracts: crates/board-forge/src/ci_health.rs (Config, Target,
  Classification). Both examples read GitHub and require a read token;
  neither publishes a report.")]
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
