use anyhow::{Context, Result, ensure};
use board_forge::poll::{self, State};
use std::{io::Write, path::PathBuf};

#[derive(clap::Args)]
pub struct Poll {
    /// Print the step a pass would take for each pull request; required, as no step is taken yet.
    #[arg(long)]
    pub dry_run: bool,
    /// Poll state file (cached responses, the events pages last read, the idle interval); created if missing. Keep it where only the poll writes it: its bodies are trusted.
    #[arg(long, value_name = "FILE")]
    pub state: Option<PathBuf>,
    /// Repository whose open pull requests by --bot get a step; its activity is read too. Repeatable.
    #[arg(long, value_name = "OWNER/REPO")]
    pub follow: Vec<String>,
    /// Repository whose activity is read, besides those on the board given by --owner/--project or --snapshot. Repeatable.
    #[arg(long, value_name = "OWNER/REPO")]
    pub watch: Vec<String>,
    /// Login whose activity is direction and whose review of a head counts.
    #[arg(long, value_name = "LOGIN")]
    pub operator: String,
    /// Login whose pull requests are followed and whose activity is state.
    #[arg(long, value_name = "LOGIN")]
    pub bot: String,
    /// Repository whose dispatch runs the fix markers name.
    #[arg(long, value_name = "OWNER/REPO")]
    pub dispatch_repo: String,
}

/// One pass. Lines go to stdout as poll.cjs prints them, then when the next
/// pass is due as `next-due=` and `next-due-seconds=`, ready for `$GITHUB_OUTPUT`.
pub fn run(args: &Poll, board: impl FnOnce() -> Result<Vec<String>>) -> Result<()> {
    ensure!(
        args.dry_run,
        "unsupported_capability: poll only previews its steps; pass --dry-run"
    );
    let state = match &args.state {
        Some(path) if path.exists() => State::from_json(
            &serde_json::from_slice(
                &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
            )
            .with_context(|| format!("decode {}", path.display()))?,
        )?,
        _ => State::default(),
    };
    let config = poll::Config {
        operator: args.operator.clone(),
        bot: args.bot.clone(),
        dispatch_repo: args.dispatch_repo.clone(),
    };
    let mut watch = args.watch.clone();
    watch.extend(board()?);
    let client = board_forge::github::Client::from_env()?;
    let now = time::OffsetDateTime::now_utc();
    let outcome = poll::run(&client, &client, &config, state, &watch, &args.follow, now)?;
    if let Some(path) = &args.state {
        std::fs::write(path, serde_json::to_vec(&outcome.state.to_json())?)
            .with_context(|| format!("write {}", path.display()))?;
    }
    let mut out = std::io::stdout().lock();
    for line in &outcome.lines {
        writeln!(out, "[dry-run] {line}")?;
    }
    eprintln!(
        "{} pull request(s) in flight, {} activity line(s)",
        outcome.in_flight, outcome.activity
    );
    let seconds = outcome.state.interval_seconds;
    let due = (now + time::Duration::seconds(seconds))
        .replace_nanosecond(0)?
        .format(&time::format_description::well_known::Rfc3339)?;
    writeln!(out, "next-due={due}\nnext-due-seconds={seconds}")?;
    ensure!(
        outcome.errors == 0,
        "{} pull request(s) or repositories could not be handled",
        outcome.errors
    );
    Ok(())
}
