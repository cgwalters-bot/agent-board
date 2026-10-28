//! `agent-board admit` and `agent-board release` (docs/pass.md#leases): a
//! run's lease on a slot, taken before any inference and given back when it
//! ends.
use anyhow::{Context, Result, anyhow};
use board_forge::github::{Client, DEFAULT_HOST};
use board_forge::parse_github_item;
use board_forge::pass::{Config, lease};
use std::io::Write;
use std::path::PathBuf;

#[derive(clap::Args)]
pub struct Admit {
    /// The pass configuration (docs/pass.md), from a trusted default-branch checkout: its dispatchRepo holds the leases, and its wip, pools, usage and budget size each pool.
    #[arg(long, value_name = "FILE")]
    pub config: PathBuf,
    /// The pool the run uses, one of the configuration's pools.
    #[arg(long, value_name = "POOL")]
    pub pool: String,
    /// The item the run works on: URL or OWNER/REPO#N.
    #[arg(long, value_name = "ITEM")]
    pub item: String,
    /// The run taking the lease, in dispatchRepo (GITHUB_RUN_ID); a lease is reaped once it ends.
    #[arg(long, value_name = "ID")]
    pub run: u64,
    /// Take a slot past wip and pacing; only for GH_TOKEN of the configuration's operator, and said on stdout and in the lease.
    #[arg(long = "override")]
    pub overridden: bool,
}

#[derive(clap::Args)]
pub struct Release {
    /// The pass configuration, as for admit.
    #[arg(long, value_name = "FILE")]
    pub config: PathBuf,
    /// The pool the run took its lease of.
    #[arg(long, value_name = "POOL")]
    pub pool: String,
    /// The run whose lease to free.
    #[arg(long, value_name = "ID")]
    pub run: u64,
}

fn config(path: &PathBuf) -> Result<Config> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Config::parse(&text).with_context(|| format!("{}", path.display()))
}

/// Takes a lease with GH_TOKEN, printing what it reaped and `admit POOL/SLOT`;
/// none free is an error with the reason.
pub fn admit(args: &Admit, out: &mut impl Write) -> Result<()> {
    let config = config(&args.config)?;
    let item = parse_github_item(&args.item, None, DEFAULT_HOST)?;
    let request = lease::Request {
        pool: &args.pool,
        item: &item,
        run: args.run,
        overridden: args.overridden,
    };
    let now = time::OffsetDateTime::now_utc();
    let mut lines = Vec::new();
    let admitted = lease::admit(&Client::from_env()?, &config, &request, now, &mut lines);
    for line in &lines {
        writeln!(out, "{line}")?;
    }
    let held = admitted?.map_err(|why| anyhow!("admit refused: {why}"))?;
    writeln!(
        out,
        "admit {}/{}: run {} on {} until {}",
        held.lease.pool, held.slot, held.lease.run, held.lease.item, held.lease.expires_at
    )?;
    Ok(())
}

/// Frees the run's lease with GH_TOKEN, a line for each.
pub fn release(args: &Release, out: &mut impl Write) -> Result<()> {
    let config = config(&args.config)?;
    let now = time::OffsetDateTime::now_utc();
    let client = Client::from_env()?;
    let on = (args.pool.as_str(), args.run);
    for line in lease::release(&client, &config.dispatch_repo, on, now)? {
        writeln!(out, "{line}")?;
    }
    Ok(())
}
