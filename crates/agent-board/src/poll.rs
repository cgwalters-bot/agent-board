use crate::supervise::read;
use anyhow::{Context, Result, ensure};
use board_forge::poll::{self, State, bounds::FixBounds};
use std::{io::Write, path::PathBuf};

#[derive(Clone, clap::Args)]
pub struct Poll {
    /// Repository-owned agent-board.json from a trusted default-branch checkout,
    /// never a dispatched pull request or workflow input. File settings win.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Repository-owned pools file (each pool's `tier`), from the same trusted checkout, needed only where fixPools or kindPools names a tier such as tier-1.
    #[arg(long, value_name = "FILE")]
    pub pools: Option<PathBuf>,
    /// The caller's agentic-job bounds file for its `fix` kind (such as .github/agentic-job/fix.toml), from the same trusted checkout, never a dispatched head or workflow input: a pull request whose repository is outside its `repos`, or whose head branch is outside its push_to_pull_request_branch `branches`, gets no fix, rebase, continuation or answer round and is left to the operator, nor does a fix round on the operator's comments start there. The policy step stays the boundary.
    #[arg(long, value_name = "FILE")]
    pub fix_bounds: Option<PathBuf>,
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
    #[arg(long, value_name = "LOGIN", required_unless_present = "config")]
    pub operator: Option<String>,
    /// Login whose pull requests are followed and whose activity is state.
    #[arg(long, value_name = "LOGIN", required_unless_present = "config")]
    pub bot: Option<String>,
    /// Repository whose dispatch runs the fix markers name.
    #[arg(long, value_name = "OWNER/REPO", required_unless_present = "config")]
    pub dispatch_repo: Option<String>,
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

fn configured(
    args: &Poll,
    value: Option<&serde_json::Value>,
    pools: Option<&serde_json::Value>,
    now: time::OffsetDateTime,
) -> Result<(Poll, poll::PollPolicy)> {
    let mut effective = args.clone();
    let mut policy = poll::PollPolicy::default();
    if let Some(value) = value {
        policy = poll::PollPolicy::from_json(value).context("decode poll policy")?;
        for (key, target) in [
            ("operator", &mut effective.operator),
            ("bot", &mut effective.bot),
            ("dispatchRepo", &mut effective.dispatch_repo),
        ] {
            *target = Some(
                value[key]
                    .as_str()
                    .with_context(|| format!("config missing {key}"))?
                    .to_owned(),
            );
        }
        effective.follow =
            serde_json::from_value(value["follow"].clone()).context("config follow")?;
        if let Some(pools) = poll::option(value, "fixPools")? {
            effective.fix_pool =
                serde_json::from_value(pools.clone()).context("config fixPools")?;
        }
        if let Some(workflow) = poll::option(value, "reviewWorkflow")? {
            effective.review_workflow = Some(
                workflow
                    .as_str()
                    .context("config reviewWorkflow")?
                    .to_owned(),
            );
        }
        if let Some(workflow) = poll::option(value, "dispatchWorkflow")? {
            effective.dispatch_workflow = Some(
                workflow
                    .as_str()
                    .context("config dispatchWorkflow")?
                    .to_owned(),
            );
        }
    }
    policy
        .read_tiers(pools, &effective.fix_pool)
        .context("decode pool tiers")?;
    policy
        .check_overrides(now, &effective.fix_pool)
        .context("decode pace overrides")?;
    Ok((effective, policy))
}

/// One pass. Lines go to stdout as poll.cjs prints them, then when the next
/// pass is due as `next-due=` and `next-due-seconds=`, ready for `$GITHUB_OUTPUT`.
pub fn run(
    args: &Poll,
    board: impl FnOnce() -> Result<Option<board_core::Snapshot>>,
) -> Result<()> {
    let value = args.config.as_deref().map(read).transpose()?;
    let pools = args.pools.as_deref().map(read).transpose()?;
    let now = time::OffsetDateTime::now_utc();
    let (effective, mut policy) = configured(args, value.as_ref(), pools.as_ref(), now)?;
    policy.fix_bounds = args
        .fix_bounds
        .as_deref()
        .map(FixBounds::read)
        .transpose()?;
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
        Some(path) if path.exists() => State::from_json(&read(path)?)?,
        _ => State::default(),
    };
    let config = poll::Config {
        operator: args
            .operator
            .clone()
            .context("missing --operator or config operator")?,
        bot: args.bot.clone().context("missing --bot or config bot")?,
        dispatch_repo: args
            .dispatch_repo
            .clone()
            .context("missing --dispatch-repo or config dispatchRepo")?,
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
        &config.operator,
        &mut std::io::stderr().lock(),
    );
    let dispatcher = board_forge::github::Client::from_var("DISPATCH_TOKEN");
    let apply = args.apply.then(|| poll::Apply {
        dispatcher: dispatcher.as_ref().unwrap_or(&client),
        workflow: args.dispatch_workflow.clone().unwrap_or_default(),
        pools: args.fix_pool.clone(),
        git_ref: None,
    });
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
                    git_ref: None,
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
        let now = time::OffsetDateTime::parse(
            "2026-10-21T12:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        let base = json!({"operator": "owner", "bot": "worker", "dispatchRepo": "o/tracker", "follow": ["o/repo"]});
        let (cfg, policy) = configured(&args, Some(&base), None, now).unwrap();
        assert_eq!(cfg.operator.as_deref(), Some("owner"));
        assert_eq!(cfg.bot.as_deref(), Some("worker"));
        assert_eq!(cfg.dispatch_repo.as_deref(), Some("o/tracker"));
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
        let (cfg, _) = configured(&args, Some(&full), None, now).unwrap();
        assert_eq!(cfg.fix_pool, ["configured"]);
        assert_eq!(cfg.review_workflow.as_deref(), Some("review.yml"));
        assert_eq!(cfg.dispatch_workflow.as_deref(), Some("dispatch.yml"));

        // A tier is read from the pools file, and only where a setting names one.
        let paced = |pool: &str, until: &str| {
            json!({"repository": "o/ops", "issue": 1, "bot": "worker",
                "overrides": [{"pool": pool, "until": until, "mode": "spend"}]})
        };
        let pools = json!({"claude": {"tier": 1}, "openai": {"tier": 1}, "mistral": {"tier": 2},
            "fable": {"tier": 1, "auto": false}, "hand": {"tier": 3, "auto": false}});
        for (case, settings, file, expected) in [
            (
                "a pool name needs no file",
                json!({"fixPools": ["claude"]}),
                None,
                Ok(json!({})),
            ),
            (
                "fixPools and kindPools take a tier's automatic pools",
                json!({"fixPools": ["tier-1"], "kindPools": {"triage": "tier-2"}}),
                Some(&pools),
                Ok(json!({"tier-1": ["claude", "openai"], "tier-2": ["mistral"], "tier-3": []})),
            ),
            (
                "an unknown tier",
                json!({"kindPools": {"research": "tier-4"}}),
                Some(&pools),
                Err("kindPools names tier-4, which is not a tier of the pools file"),
            ),
            (
                "a tier of hand-dispatch pools only",
                json!({"fixPools": ["tier-3"]}),
                Some(&pools),
                Err("fixPools names tier-3, which has no automatic pool in the pools file"),
            ),
            (
                "a tier without the pools file",
                json!({"fixPools": ["tier-1"]}),
                None,
                Err("fixPools names tier-1, but no pools file gives the tiers"),
            ),
            (
                "a pool without its tier",
                json!({"fixPools": ["tier-1"]}),
                Some(&json!({"claude": {"tier": 1}, "openai": {}})),
                Err("pools file: openai tier is not 1 to 9"),
            ),
            (
                "a tier past the limit",
                json!({"fixPools": ["tier-1"]}),
                Some(&json!({"claude": {"tier": 10}})),
                Err("pools file: claude tier is not 1 to 9"),
            ),
            (
                "a tier of 0",
                json!({"fixPools": ["tier-1"]}),
                Some(&json!({"claude": {"tier": 0}})),
                Err("pools file: claude tier is not 1 to 9"),
            ),
            (
                "an auto that is not a boolean",
                json!({"fixPools": ["tier-1"]}),
                Some(&json!({"claude": {"tier": 1, "auto": "no"}})),
                Err("pools file: claude auto is not a boolean"),
            ),
            (
                "a pool named as a tier",
                json!({"fixPools": ["tier-1"]}),
                Some(&json!({"tier-1": {"tier": 1}})),
                Err("pools file: pool \"tier-1\" is named as a tier"),
            ),
            (
                "a pools file that is not an object",
                json!({"fixPools": ["tier-1"]}),
                Some(&json!(["claude"])),
                Err("pools file is not an object"),
            ),
            (
                "a tier where the round takes one pool",
                json!({"toilFixPool": "tier-1"}),
                Some(&pools),
                Err("toilFixPool names tier-1, but only fixPools and kindPools take a tier"),
            ),
            (
                "a tier for a conflict round",
                json!({"conflictFixPool": "tier-1"}),
                Some(&pools),
                Err("conflictFixPool names tier-1, but only fixPools and kindPools take a tier"),
            ),
            (
                "a tier for a diagnostic round",
                json!({"diagnosticPool": "tier-1"}),
                Some(&pools),
                Err("diagnosticPool names tier-1, but only fixPools and kindPools take a tier"),
            ),
            (
                "a tier for a continuation",
                json!({"continuation": {"pools": {"cap": "tier-1"}}}),
                Some(&pools),
                Err("continuation pools names tier-1, but only fixPools and kindPools take a tier"),
            ),
            (
                "an override may name a pool of a configured tier",
                json!({"fixPools": ["tier-1"], "pace": paced("openai", "2026-10-22T04:00:00Z")}),
                Some(&pools),
                Ok(json!({"tier-1": ["claude", "openai"], "tier-2": ["mistral"], "tier-3": []})),
            ),
            (
                "an override of a pool no setting names",
                json!({"fixPools": ["tier-1"], "pace": paced("fable", "2026-10-22T04:00:00Z")}),
                Some(&pools),
                Err("pace override names fable, which is not a configured pool"),
            ),
            (
                "an override more than a week ahead",
                json!({"fixPools": ["claude"], "pace": paced("claude", "2026-10-28T12:01:00Z")}),
                None,
                Err(
                    "pace override for claude runs until 2026-10-28T12:01Z, more than 7 days ahead",
                ),
            ),
        ] {
            let mut value = json!({"operator": "owner", "bot": "worker", "dispatchRepo": "o/tracker", "follow": ["o/repo"]});
            value
                .as_object_mut()
                .unwrap()
                .extend(settings.as_object().unwrap().clone());
            let got = configured(&args, Some(&value), file, now)
                .map(|(_, policy)| json!(policy.tiers))
                .map_err(|e| format!("{:#}", e.root_cause()));
            assert_eq!(got, expected.map_err(str::to_owned), "{case}");
        }
    }
}
