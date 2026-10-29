//! `agent-board cost` (docs/cost.md): one day's agent spend, read-only.
use anyhow::{Context, Result, ensure};
use board_forge::cost::{Reader, parse_date, read_day, report, repositories};
use board_forge::pass::Config;
use board_forge::poll::Rest;
use std::{io::Write, path::PathBuf};

#[derive(clap::Args)]
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
    /// A directory that keeps each run's summary.json by run id, so that a run read once is never downloaded again.
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
