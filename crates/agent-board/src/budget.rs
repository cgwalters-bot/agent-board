//! `agent-board budget` (docs/pass.md#budget-tree): the budget tree, read-only.
use anyhow::{Context, Result};
use board_forge::github::Transport;
use board_forge::pass::Config;
use std::{io::Write, path::PathBuf};

#[derive(clap::Args)]
pub struct Budget {
    /// The pass configuration (docs/pass.md), from a trusted default-branch checkout: its `epics.repository`, `board`, `bot` and `budget.weeklyUsd`. GH_TOKEN reads.
    #[arg(long, value_name = "FILE")]
    pub config: PathBuf,
}

/// Print the tree, a line an item. The configuration is checked before a
/// token is read.
pub fn run<T: Transport>(
    args: &Budget,
    transport: impl FnOnce() -> Result<T>,
    out: &mut impl Write,
) -> Result<()> {
    let text = std::fs::read_to_string(&args.config)
        .with_context(|| format!("read {}", args.config.display()))?;
    let config = Config::parse(&text).with_context(|| format!("{}", args.config.display()))?;
    let tree = board_forge::budget::read(&transport()?, &config)?;
    for line in board_forge::budget::lines(&tree) {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// A board with one open epic, which has no sub-issues and one run.
    struct Forge;

    impl Transport for &Forge {
        fn graphql(&self, query: &str, _: Value) -> Result<Value> {
            if query.contains("issues(first") {
                return Ok(json!({"repository": {"issues": {"nodes": [{"number": 1}],
                    "pageInfo": {"hasNextPage": false}}}}));
            }
            if query.contains("subIssues") {
                return Ok(json!({"repository": {"issue": {"state": "OPEN",
                    "projectItems": {"nodes": [], "totalCount": 0},
                    "subIssues": {"nodes": [], "totalCount": 0},
                    "closedByPullRequestsReferences": {"nodes": [], "totalCount": 0}}}}));
            }
            Ok(json!({"repository": {"issue": {"comments": {"nodes": [{
                "url": "https://github.com/cgwalters-forge/tracker/issues/1#issuecomment-2",
                "body": "Done.\n\n<!-- agentic-job-cost: 7 usd=1.5 partial=false -->\n\n<!-- agentic-job-run: 7 -->",
                "createdAt": "2026-10-24T00:00:00Z", "lastEditedAt": null,
                "author": {"login": "cgwalters-bot"}, "editor": null}],
                "pageInfo": {"hasNextPage": false, "endCursor": null}}}}}))
        }
    }

    #[test]
    fn prints_the_tree_and_checks_the_config_first() {
        let config = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/pass/config.json"
        );
        let mut out = Vec::new();
        let args = Budget {
            config: config.into(),
        };
        run(&args, || Ok(&Forge), &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "board: no weekly budget; actual $1.50 (1 run(s)) under 1 open epic(s), all time, at list prices\n  cgwalters-forge/tracker#1: actual $1.50 (1 run(s))\n"
        );
        let args = Budget {
            config: concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml").into(),
        };
        let error = run(
            &args,
            || -> Result<&Forge> { panic!("read") },
            &mut Vec::new(),
        );
        assert!(format!("{:#}", error.unwrap_err()).contains("Cargo.toml"));
    }
}
