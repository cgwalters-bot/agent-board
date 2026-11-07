//! `agent-board cost` (docs/cost.md): one day's agent spend, read-only.
use anyhow::{Context, Result, ensure};
use board_forge::cost::{Reader, parse_date, read_day, report, repositories};
use board_forge::pass::Config;
use board_forge::poll::Rest;
use std::{io::Write, path::PathBuf};

pub const COST_ABOUT: &str = "Report one UTC day's agent spend by kind, model, repository and item. Use this for daily accounting, including reviews that are not board items; use budget for the board's epic roll-up instead.

Reads completed GitHub Actions runs created on --date and their agent-run summaries in the pass configuration's workRepositories, follow and dispatchRepo. Supply --config from a trusted default-branch checkout and GH_TOKEN with read access to those repositories' Actions runs and artifacts. The date must be YYYY-MM-DD and not after today (UTC).

Prints agent-board/cost-day/v1 JSON to stdout by default; --human prints a short table. Missing model prices produce null costs, not zero. Unread artifacts are reported under unread; failed requests, exhausted read bounds or cache errors fail the command rather than silently omit runs. Reads are bounded to 1,000 requests and ten minutes.

Never writes to the forge or publishes a report. With --cache, writes summaries locally in an existing directory as RUN_ID.json; cached summaries are not downloaded again. Without --cache, no summaries are saved. The pass's optional cost publication stage is separate; see docs/cost.md for accounting, schema and publication contracts.";

#[derive(clap::Args)]
#[command(after_long_help = "Examples:
  Print the JSON report for a completed UTC day:
    agent-board cost --config agent-board.json --date 2026-10-28

  Print a human summary of the same day:
    agent-board cost --config agent-board.json --date 2026-10-28 --human

  Reuse summaries between reads (create the cache directory first):
    agent-board cost --config agent-board.json --date 2026-10-28 --cache cost-cache

  All examples read GitHub and require GH_TOKEN; none publishes a report.")]
pub struct Cost {
    /// The pass configuration (docs/pass.md), from a trusted default-branch checkout: its `workRepositories`, `follow` and `dispatchRepo`, whose runs count, and its `prices`. GH_TOKEN reads their Actions runs and artifacts.
    #[arg(long, value_name = "FILE")]
    pub config: PathBuf,
    /// The UTC day, YYYY-MM-DD, whose completed runs count; not after today.
    #[arg(long, value_name = "DATE")]
    pub date: String,
    /// Print a short table instead of the agent-board/cost-day/v1 JSON.
    #[arg(long)]
    pub human: bool,
    /// Existing directory that keeps summaries as RUN_ID.json, so a cached run is never downloaded again; omitted by default.
    #[arg(long, value_name = "DIR")]
    pub cache: Option<PathBuf>,
}

/// Print the day's report. The configuration and the day are checked
/// before a token is read.
pub fn run<C: Rest>(
    args: &Cost,
    client: impl FnOnce() -> Result<C>,
    today: time::Date,
    out: &mut impl Write,
) -> Result<()> {
    let text = std::fs::read_to_string(&args.config)
        .with_context(|| format!("read {}", args.config.display()))?;
    let config = Config::parse(&text).with_context(|| format!("{}", args.config.display()))?;
    let date = parse_date(&args.date)?;
    ensure!(date <= today, "{} is after today", args.date);
    if let Some(cache) = &args.cache {
        ensure!(cache.is_dir(), "{} is not a directory", cache.display());
    }
    let client = client()?;
    let reader = Reader::new(&client, args.cache.as_deref());
    let day = read_day(&reader, &repositories(&config), date)?;
    let report = report(date, day, &config.prices);
    match args.human {
        true => write!(out, "{}", board_forge::cost::human(&report))?,
        false => writeln!(out, "{report:#}")?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use board_forge::github::Client;

    const CONFIG: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/pass/config.json"
    );

    #[test]
    fn checks_the_day_and_cache_before_reading_a_token() {
        let today = parse_date("2026-10-29").unwrap();
        for (date, cache, expected) in [
            ("2026-10-30", None, "is after today"),
            ("2026-10-28T00:00:00Z", None, "is not a date YYYY-MM-DD"),
            ("2026-10-28", Some(CONFIG), "is not a directory"),
        ] {
            let args = Cost {
                config: CONFIG.into(),
                date: date.into(),
                human: false,
                cache: cache.map(PathBuf::from),
            };
            let error = run(
                &args,
                || -> Result<Client> { panic!("read a token") },
                today,
                &mut Vec::new(),
            );
            let error = format!("{:#}", error.unwrap_err());
            assert!(error.contains(expected), "{error}");
        }
    }
}
