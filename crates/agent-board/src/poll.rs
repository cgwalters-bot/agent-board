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
    /// Take the steps: start fix and continuation runs, merge an approved green head, and label needs-human, with GH_TOKEN (the bot's); fix runs are started with DISPATCH_TOKEN if set. Only in --follow repositories, and on the operator's comments where the config's operatorComments names.
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
pub fn run(
    args: &Poll,
    board: impl FnOnce() -> Result<Option<board_core::Snapshot>>,
) -> Result<()> {
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
    let snapshot = board()?;
    if let Some(snapshot) = &snapshot {
        watch.extend(
            snapshot
                .items
                .iter()
                .filter(|item| item.identity.host == board_forge::github::DEFAULT_HOST)
                .map(|item| item.identity.repository.clone()),
        );
    }
    let client = board_forge::github::Client::from_env()?;
    // Editable/stale Run text must not suppress the existing PR poll's duties.
    let preview_error = status_preview(
        &client,
        snapshot.as_ref(),
        value.as_ref(),
        &args.operator,
        &mut std::io::stderr().lock(),
    );
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
    ensure!(
        preview_error.is_none(),
        "status preview failed: {}",
        preview_error.unwrap_or_default()
    );
    Ok(())
}

/// Deliberately does not return Result: run the normal poll before reporting
/// this optional preview's failure in the final exit status.
fn status_preview<R: poll::Rest>(
    client: &R,
    snapshot: Option<&board_core::Snapshot>,
    value: Option<&serde_json::Value>,
    operator: &str,
    err: &mut impl Write,
) -> Option<String> {
    let result = (|| -> Result<()> {
        if let Some(value) = value {
            let settings = board_forge::status::configured(value)?;
            if settings.enabled {
                let snapshot = snapshot.context("statusReconcile requires a board snapshot")?;
                let plan = board_forge::status::plan(client, snapshot, &settings, operator)?;
                // Including with --apply, keep this off the poll's stdout.
                writeln!(err, "status-preview={}", serde_json::to_string(&plan)?)?;
            }
        }
        Ok(())
    })();
    result.err().map(|error| {
        let message = format!("{error:#}");
        let _ = writeln!(err, "status-preview-error={message}");
        message
    })
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
    fn status_preview_is_isolated_from_normal_poll() {
        use board_forge::{
            github::Transport,
            poll::{Rest, RestResponse},
        };
        use std::cell::RefCell;
        struct Offline(RefCell<Vec<String>>);

        impl Rest for Offline {
            fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
                self.0.borrow_mut().push(url.into());
                let body = if url.ends_with("/user") {
                    json!({"login":"worker"})
                } else {
                    json!([])
                };
                Ok(RestResponse {
                    status: 200,
                    etag: None,
                    last_modified: None,
                    link: None,
                    body,
                })
            }

            fn send(
                &self,
                _: &str,
                _: &str,
                _: &serde_json::Value,
            ) -> Result<(u16, serde_json::Value)> {
                panic!("no mutation expected")
            }
        }

        impl Transport for Offline {
            fn graphql(&self, _: &str, _: serde_json::Value) -> Result<serde_json::Value> {
                panic!("no GraphQL request expected")
            }
        }

        for case in ["disabled", "missing_board", "bad_run", "project", "enabled"] {
            let client = Offline(RefCell::new(vec![]));
            let mut snapshot: board_core::Snapshot =
                serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
            let mut value = json!({"statusReconcile":{
                "enabled":case != "disabled", "project":snapshot.project,
                "repositories":["example/intake"], "runRepositories":["example/runners"]
            }});
            if case == "bad_run" {
                snapshot.items[0].fields.insert(
                    "Run".into(),
                    json!("https://evil.test/example/runners/actions/runs/42"),
                );
            }
            if case == "project" {
                value["statusReconcile"]["project"] =
                    json!("https://github.com/orgs/example/projects/2");
            }
            let mut stderr = vec![];
            let error = status_preview(
                &client,
                if case == "missing_board" {
                    None
                } else {
                    Some(&snapshot)
                },
                Some(&value),
                "operator",
                &mut stderr,
            );
            assert_eq!(
                error.is_some(),
                matches!(case, "missing_board" | "bad_run" | "project"),
                "{case}"
            );
            assert!(client.0.borrow().is_empty());
            let stderr = String::from_utf8(stderr).unwrap();
            if error.is_some() {
                assert!(stderr.starts_with("status-preview-error="));
            } else if case == "enabled" {
                assert!(stderr.starts_with("status-preview={"));
            } else {
                assert!(stderr.is_empty());
            }
            // Exercise normal apply polling after a failed preview. Its auth
            // and activity reads must still occur, and no board write occurs.
            let config = poll::Config {
                operator: "operator".into(),
                bot: "worker".into(),
                dispatch_repo: "example/runners".into(),
                review_workflow: None,
                policy: poll::PollPolicy::default(),
            };
            let outcome = poll::run(
                &client,
                &client,
                &config,
                State::default(),
                &["example/intake".into()],
                &[],
                Some(poll::Apply {
                    dispatcher: &client,
                    workflow: "dispatch.yml".into(),
                    pools: vec![],
                }),
                time::OffsetDateTime::UNIX_EPOCH,
            )
            .unwrap();
            assert_eq!(outcome.errors, 0);
            assert!(client.0.borrow().iter().any(|url| url.ends_with("/user")));
            assert!(client.0.borrow().iter().any(|url| url.contains("/events")));
        }
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
        assert!(policy.operator_comments.is_empty());
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
