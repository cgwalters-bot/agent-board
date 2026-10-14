use anyhow::{Context, Result, ensure};
use board_forge::poll::{self, State};
use std::{io::Write, path::PathBuf};

#[derive(clap::Args)]
pub struct Poll {
    /// Print the step a pass would take for each pull request and take none.
    #[arg(long, conflicts_with = "apply")]
    pub dry_run: bool,
    /// Take the steps: mark ready, start fix runs, merge an approved green head, and label needs-human, with GH_TOKEN (the bot's); fix runs are started with DISPATCH_TOKEN if set. Only in --follow repositories.
    #[arg(long, requires = "dispatch_workflow")]
    pub apply: bool,
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
    /// Workflow file in --dispatch-repo that --apply starts fix runs with.
    #[arg(long, value_name = "FILE")]
    pub dispatch_workflow: Option<String>,
    /// Workflow file of the review job in each --follow repository. Its reviews of a head count as the verdict only with this, and only from a run of it on pull_request_target for that head of a --bot dispatch pull request from its fork into a public repository; without it only --operator's count.
    #[arg(long, value_name = "FILE")]
    pub review_workflow: Option<String>,
    /// The workflow's `pool` input for each fix round in turn. Repeatable; none leaves its default.
    #[arg(long, value_name = "POOL")]
    pub fix_pool: Vec<String>,
}

/// One pass. Lines go to stdout as poll.cjs prints them, then when the next
/// pass is due as `next-due=` and `next-due-seconds=`, ready for `$GITHUB_OUTPUT`.
pub fn run(args: &Poll, board: impl FnOnce() -> Result<Vec<String>>) -> Result<()> {
    ensure!(
        args.dry_run || args.apply,
        "poll takes its steps only when asked; pass --dry-run or --apply"
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
        review_workflow: args.review_workflow.clone(),
    };
    let mut watch = args.watch.clone();
    watch.extend(board()?);
    let client = board_forge::github::Client::from_env()?;
    let dispatcher = board_forge::github::Client::from_var("DISPATCH_TOKEN");
    let apply = args.apply.then(|| poll::Apply {
        dispatcher: dispatcher.as_ref().unwrap_or(&client),
        workflow: args.dispatch_workflow.clone().unwrap_or_default(),
        pools: args.fix_pool.clone(),
    });
    let now = time::OffsetDateTime::now_utc();
    let outcome = poll::run(
        &client,
        &client,
        &config,
        state,
        &watch,
        &args.follow,
        apply,
        now,
    )?;
    if let Some(path) = &args.state {
        std::fs::write(path, serde_json::to_vec(&outcome.state.to_json())?)
            .with_context(|| format!("write {}", path.display()))?;
    }
    let mut out = std::io::stdout().lock();
    let prefix = if args.apply { "" } else { "[dry-run] " };
    for line in &outcome.lines {
        writeln!(out, "{prefix}{line}")?;
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
