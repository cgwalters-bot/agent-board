use anyhow::{Context, Result, ensure};
use board_forge::poll::{self, State};
use std::{io::Write, path::PathBuf};

#[derive(Clone, clap::Args)]
pub struct Poll {
    /// Repository-owned agent-board.json from a trusted default-branch checkout,
    /// never a dispatched pull request or workflow input. File settings win.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Print the step a pass would take for each pull request and take none.
    #[arg(long, conflicts_with = "apply")]
    pub dry_run: bool,
    /// Take the steps: mark ready, start fix runs, merge an approved green head, and label needs-human, with GH_TOKEN (the bot's); fix runs are started with DISPATCH_TOKEN if set. Only in --follow repositories.
    #[arg(long)]
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
    #[arg(
        long,
        value_name = "LOGIN",
        required_unless_present = "config",
        default_value = ""
    )]
    pub operator: String,
    /// Login whose pull requests are followed and whose activity is state.
    #[arg(
        long,
        value_name = "LOGIN",
        required_unless_present = "config",
        default_value = ""
    )]
    pub bot: String,
    /// Repository whose dispatch runs the fix markers name.
    #[arg(
        long,
        value_name = "OWNER/REPO",
        required_unless_present = "config",
        default_value = ""
    )]
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

fn configured(args: &Poll, value: Option<&serde_json::Value>) -> Result<(Poll, poll::PollPolicy)> {
    let mut effective = args.clone();
    let mut policy = poll::PollPolicy::default();
    if let Some(value) = value {
        policy = poll::PollPolicy::from_json(value).context("decode poll policy")?;
        for (key, target) in [
            ("operator", &mut effective.operator),
            ("bot", &mut effective.bot),
            ("dispatchRepo", &mut effective.dispatch_repo),
        ] {
            *target = value[key]
                .as_str()
                .with_context(|| format!("config missing {key}"))?
                .to_owned();
        }
        effective.follow =
            serde_json::from_value(value["follow"].clone()).context("config follow")?;
        if let Some(pools) = value.get("fixPools") {
            effective.fix_pool =
                serde_json::from_value(pools.clone()).context("config fixPools")?;
        }
        if let Some(workflow) = value.get("reviewWorkflow") {
            effective.review_workflow = Some(
                workflow
                    .as_str()
                    .context("config reviewWorkflow")?
                    .to_owned(),
            );
        }
        if let Some(workflow) = value.get("dispatchWorkflow") {
            effective.dispatch_workflow = Some(
                workflow
                    .as_str()
                    .context("config dispatchWorkflow")?
                    .to_owned(),
            );
        }
    }
    Ok((effective, policy))
}

/// One pass. Lines go to stdout as poll.cjs prints them, then when the next
/// pass is due as `next-due=` and `next-due-seconds=`, ready for `$GITHUB_OUTPUT`.
pub fn run(args: &Poll, board: impl FnOnce() -> Result<Vec<String>>) -> Result<()> {
    let value = args
        .config
        .as_ref()
        .map(|path| -> Result<serde_json::Value> {
            serde_json::from_slice(
                &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
            )
            .with_context(|| format!("decode {}", path.display()))
        })
        .transpose()?;
    let (effective, policy) = configured(args, value.as_ref())?;
    let args = &effective;
    ensure!(
        !args.apply || args.dispatch_workflow.is_some(),
        "--apply requires --dispatch-workflow <FILE> or config dispatchWorkflow"
    );
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
        policy,
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serde_json::json;

    #[derive(Parser)]
    struct Args {
        #[command(flatten)]
        poll: Poll,
    }

    #[test]
    fn trusted_settings_override_flags_and_optional_settings_default() {
        let args = Args::parse_from([
            "poll",
            "--config",
            "unused",
            "--operator",
            "other",
            "--bot",
            "other",
            "--dispatch-repo",
            "other/repo",
            "--follow",
            "other/repo",
            "--fix-pool",
            "large",
            "--review-workflow",
            "other.yml",
            "--dispatch-workflow",
            "other.yml",
        ])
        .poll;
        let base = json!({"operator": "owner", "bot": "worker", "dispatchRepo": "o/tracker", "follow": ["o/repo"]});
        let (cfg, policy) = configured(&args, Some(&base)).unwrap();
        assert_eq!(cfg.operator, "owner");
        assert_eq!(cfg.bot, "worker");
        assert_eq!(cfg.dispatch_repo, "o/tracker");
        assert_eq!(cfg.follow, ["o/repo"]);
        // Optional legacy flags remain usable when the file has no setting.
        assert_eq!(cfg.fix_pool, ["large"]);
        assert_eq!(cfg.review_workflow.as_deref(), Some("other.yml"));
        assert!(policy.merge_authors.is_empty());
        assert!(policy.toil_fix_pool.is_empty());
        assert!(policy.kind_pools.is_empty());
        let mut full = base;
        full["fixPools"] = json!(["configured"]);
        full["reviewWorkflow"] = json!("review.yml");
        full["dispatchWorkflow"] = json!("dispatch.yml");
        let (cfg, _) = configured(&args, Some(&full)).unwrap();
        assert_eq!(cfg.fix_pool, ["configured"]);
        assert_eq!(cfg.review_workflow.as_deref(), Some("review.yml"));
        assert_eq!(cfg.dispatch_workflow.as_deref(), Some("dispatch.yml"));
    }
}
