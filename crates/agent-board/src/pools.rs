use std::{io::Write, path::PathBuf};

use anyhow::{Context, Result};
use clap::{Args as ClapArgs, Subcommand};

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print agent=, model=, budget= and skilled= lines for $GITHUB_OUTPUT.
    ///
    /// Append stdout to $GITHUB_OUTPUT. Errors print no outputs. Nothing calls
    /// this automatically: use only the caller's trusted pools file, not a
    /// target checkout. Every pool's model and positive-integer budget string
    /// is validated on load. Heavy changes only the model; no pool and empty
    /// effort leave agent, model and budget empty for the wrapper's fallbacks.
    Resolve {
        /// Caller-owned pools.json from a trusted checkout.
        #[arg(long, value_name = "FILE")]
        pools: PathBuf,
        /// Explicit pool; empty uses the caller's fallbacks.
        #[arg(long, default_value = "", hide_default_value = true)]
        pool: String,
        /// Empty or heavy; heavy requires the selected pool's valid heavy.model.
        #[arg(long, default_value = "", hide_default_value = true)]
        effort: String,
        /// Run kind; review rejects heavy effort.
        #[arg(long)]
        kind: String,
        /// Wrapper's fallback agent, also used to identify an inherited heavy pool.
        #[arg(long, default_value = "opencode")]
        fallback_agent: String,
        /// Wrapper's fallback model; heavy inheritance needs exactly one matching pool.
        #[arg(long, default_value = "", hide_default_value = true)]
        fallback_model: String,
    },
}

pub fn run(args: &Args, out: &mut impl Write) -> Result<()> {
    let Command::Resolve {
        pools,
        pool,
        effort,
        kind,
        fallback_agent,
        fallback_model,
    } = &args.command;
    let value = serde_json::from_str(
        &std::fs::read_to_string(pools).with_context(|| format!("read {}", pools.display()))?,
    )
    .with_context(|| format!("decode {}", pools.display()))?;
    let output = board_forge::pools::Pools::from_json(&value)?.resolve(
        pool,
        effort,
        kind,
        fallback_agent,
        fallback_model,
    )?;
    out.write_all(output.as_bytes())?;
    Ok(())
}
