//! `epic plan`: the GitHub mutations the newest accepted epic plan on an epic
//! would make, and opt-in `epic apply`.
use anyhow::{Result, ensure};
use board_forge::epic::{Bounds, newest_epic_plan};
use board_forge::github::Transport;
use board_forge::is_github_login;
use clap::Args;
use std::io::Write;

#[derive(Args)]
pub struct Plan {
    /// Epic whose newest accepted epic plan to check: URL, OWNER/REPO#N, or N with --repo.
    issue: String,
    /// Print the mutations instead of making them (required: apply is not implemented).
    #[arg(long)]
    dry_run: bool,
    /// Login whose plans count: the operator.
    #[arg(long, value_name = "LOGIN")]
    operator: String,
    /// Login the planning run comments as, whose plans also count.
    #[arg(long, value_name = "LOGIN")]
    planner: Option<String>,
    /// Repository whose issues a plan may create or change; repeat for each. From the caller's configuration.
    #[arg(long = "allow-repo", value_name = "OWNER/REPO")]
    repositories: Vec<String>,
    /// An option of the board's Priority field; repeat for each.
    #[arg(long = "priority-option", value_name = "OPTION")]
    priorities: Vec<String>,
}

/// Print the mutations, one JSON object a line. Every input is checked
/// before a token is read, so a refusal never touches the network.
pub fn plan<T: Transport>(
    args: &Plan,
    repo: Option<&str>,
    host: &str,
    transport: impl FnOnce() -> Result<T>,
    out: &mut impl Write,
) -> Result<()> {
    ensure!(
        args.dry_run,
        "epic plan requires --dry-run; use epic apply --apply to act"
    );
    ensure!(
        host == board_forge::github::DEFAULT_HOST,
        "epic plans are read only from github.com"
    );
    let issue = board_forge::github::parse_github_issue(&args.issue, repo, host)?;
    let writers: Vec<&str> = std::iter::once(args.operator.as_str())
        .chain(args.planner.as_deref())
        .collect();
    for writer in &writers {
        ensure!(is_github_login(writer), "invalid login {writer:?}");
    }
    let bounds = Bounds {
        repositories: args.repositories.clone(),
        priorities: args.priorities.clone(),
    };
    bounds.validate()?;
    let (plan, epic, from) = newest_epic_plan(&transport()?, &issue, &writers, &bounds)?;
    for mutation in plan.preview(&epic) {
        serde_json::to_writer(&mut *out, &mutation)?;
        writeln!(out)?;
    }
    eprintln!(
        "Dry run: mutations for the epic plan in {} by {}; nothing was changed.",
        from.url,
        from.author.as_deref().unwrap_or("-")
    );
    Ok(())
}

#[derive(Args)]
pub struct Apply {
    /// Epic URL, OWNER/REPO#N, or N with --repo.
    issue: String,
    /// Trusted configuration file from a protected default-branch checkout.
    #[arg(long)]
    config: std::path::PathBuf,
    /// Print requests without writing (the default).
    #[arg(long, conflicts_with = "apply")]
    dry_run: bool,
    /// Write with the configured bot's token.
    #[arg(long)]
    apply: bool,
}

pub fn apply<T: Transport + board_forge::poll::Rest>(
    args: &Apply,
    repo: Option<&str>,
    host: &str,
    transport: impl FnOnce() -> Result<T>,
    out: &mut impl Write,
) -> Result<()> {
    use anyhow::Context;
    ensure!(
        host == board_forge::github::DEFAULT_HOST,
        "epic apply supports only github.com"
    );
    let issue = board_forge::github::parse_github_issue(&args.issue, repo, host)?;
    let value =
        serde_json::from_str(&std::fs::read_to_string(&args.config).context("read epic config")?)
            .context("decode epic config")?;
    let config = board_forge::epic_apply::Config::from_json(&value)?;
    let client = transport()?;
    if args.apply {
        for line in board_forge::epic_apply::apply(
            &client,
            &issue,
            &config,
            time::OffsetDateTime::now_utc(),
        )? {
            writeln!(out, "{line}")?;
        }
    } else {
        for mutation in board_forge::epic_apply::preview(&client, &issue, &config)? {
            serde_json::to_writer(&mut *out, &mutation)?;
            writeln!(out)?;
        }
        eprintln!("Dry run; nothing changed. Pass --apply to act.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn epic_plan_dry_run_reads_only_after_checks() {
        /// Answers both queries for cgwalters-forge/tracker#572; counts the reads.
        struct Forge(std::cell::Cell<usize>);
        impl Transport for &Forge {
            fn graphql(&self, query: &str, _: Value) -> Result<Value> {
                self.0.set(self.0.get() + 1);
                if query.contains("subIssues") {
                    return Ok(
                        json!({"repository": {"issue": {"id": "I_epic", "subIssues": {
                        "nodes": [{"number": 63, "state": "OPEN", "repository": {"nameWithOwner": "cgwalters-forge/agent-board"}}],
                        "pageInfo": {"hasNextPage": false}}}}}),
                    );
                }
                let plan = json!({"schema": board_forge::epic::EPIC_PLAN_SCHEMA,
                    "epic": "cgwalters-forge/tracker#572", "actions": [
                        {"type": "set_priority", "issue": "cgwalters-forge/agent-board#63", "value": "P1"}]});
                Ok(json!({"repository": {"issue": {"comments": {"nodes": [{
                    "url": "https://github.com/cgwalters-forge/tracker/issues/572#issuecomment-9",
                    "body": format!("```{}\n{plan}\n```", board_forge::epic::EPIC_PLAN_FENCE),
                    "createdAt": "2026-10-14T10:00:00Z", "lastEditedAt": null,
                    "author": {"login": "cgwalters-bot"}, "editor": null}],
                    "pageInfo": {"hasNextPage": false, "endCursor": null}}}}}))
            }
        }
        let args = |dry_run: bool, repo: &str, planner: &str| Plan {
            issue: "572".into(),
            dry_run,
            operator: "cgwalters".into(),
            planner: Some(planner.into()),
            repositories: vec![repo.into()],
            priorities: vec!["P0".into(), "P1".into()],
        };
        let ok = "cgwalters-forge/agent-board";
        // Arguments, reads made, then mutation lines or error.
        for (args, reads, expected) in [
            (args(true, ok, "cgwalters-bot"), 2, Ok(1)),
            (args(true, ok, "someone"), 1, Err("no plan on this issue")),
            (
                args(true, "cgwalters-forge/tracker", "cgwalters-bot"),
                2,
                Err("repository cgwalters-forge/agent-board is not in the configured list"),
            ),
            (
                args(false, ok, "cgwalters-bot"),
                0,
                Err("requires --dry-run"),
            ),
            (
                args(true, "tracker", "cgwalters-bot"),
                0,
                Err("invalid configured repository"),
            ),
            (args(true, ok, "a b"), 0, Err("invalid login")),
        ] {
            let forge = Forge(Default::default());
            let mut out = Vec::new();
            let result = plan(
                &args,
                Some("cgwalters-forge/tracker"),
                "github.com",
                || Ok(&forge),
                &mut out,
            );
            assert_eq!(forge.0.get(), reads);
            match (result, expected) {
                (Ok(()), Ok(lines)) => {
                    let out = String::from_utf8(out).unwrap();
                    assert_eq!(out.lines().count(), lines, "{out}");
                    let last: Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
                    assert_eq!(last["value"], "P1");
                }
                (Err(error), Err(expected)) => {
                    assert!(format!("{error:#}").contains(expected), "{error:#}");
                    assert!(out.is_empty());
                }
                (result, expected) => panic!("{result:?} != {expected:?}"),
            }
        }
    }
}
