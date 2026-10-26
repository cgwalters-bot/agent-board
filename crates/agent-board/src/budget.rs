//! `agent-board budget` (docs/pass.md#budget-tree): the budget tree, or
//! with `--split` a window's spend split, read-only.
use anyhow::{Context, Result, ensure};
use board_forge::changes::parse_time;
use board_forge::github::Transport;
use board_forge::pass::Config;
use std::{io::Write, path::PathBuf};

#[derive(clap::Args)]
pub struct Budget {
    /// The pass configuration (docs/pass.md), from a trusted default-branch checkout: its `epics.repository`, `board`, `bot`, `budget.weeklyUsd` and, for --split, `budget.harnessRepositories`. GH_TOKEN reads.
    #[arg(long, value_name = "FILE")]
    pub config: PathBuf,
    /// Instead of the tree, print the share of the window's public spend on upstream work and on the harness (`budget.harnessRepositories`), in percentages only.
    #[arg(long, requires = "since")]
    pub split: bool,
    /// Start of the --split window, RFC 3339 (such as 2026-10-19T00:00:00Z).
    #[arg(long, value_name = "TIME", requires = "split")]
    pub since: Option<String>,
    /// End of the --split window, RFC 3339; without it, the window runs to now.
    #[arg(long, value_name = "TIME", requires = "split")]
    pub until: Option<String>,
}

/// Print the tree, a line an item, or the split's one line. The
/// configuration and window are checked before a token is read.
pub fn run<T: Transport>(
    args: &Budget,
    transport: impl FnOnce() -> Result<T>,
    out: &mut impl Write,
) -> Result<()> {
    let text = std::fs::read_to_string(&args.config)
        .with_context(|| format!("read {}", args.config.display()))?;
    let config = Config::parse(&text).with_context(|| format!("{}", args.config.display()))?;
    if args.split {
        ensure!(
            config.harness_repositories().is_some(),
            "--split needs budget.harnessRepositories in {}",
            args.config.display()
        );
        let since = parse_time(args.since.as_deref().context("--split needs --since")?)?;
        let until = match &args.until {
            Some(until) => parse_time(until)?,
            None => time::OffsetDateTime::now_utc(),
        };
        ensure!(until > since, "the window must end after it starts");
        let split = board_forge::budget::split::read(&transport()?, &config, since, until)?;
        writeln!(out, "{}", board_forge::budget::split::line(&split))?;
        return Ok(());
    }
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

    /// A board with one open epic, which has no sub-issues and one run; the
    /// same issue is the one item the split's search finds.
    struct Forge;

    impl Transport for &Forge {
        fn graphql(&self, query: &str, _: Value) -> Result<Value> {
            if query.contains("search(") {
                return Ok(
                    json!({"search": {"issueCount": 1, "nodes": [{"__typename": "Issue",
                    "number": 1, "repository": {"nameWithOwner": "cgwalters-forge/tracker", "isPrivate": false}}],
                    "pageInfo": {"hasNextPage": false, "endCursor": null}}}),
                );
            }
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

    const CONFIG: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/pass/config.json"
    );

    fn args(config: &str, split: bool) -> Budget {
        Budget {
            config: config.into(),
            split,
            since: split.then(|| "2026-10-19T00:00:00Z".into()),
            until: split.then(|| "2026-10-26T00:00:00Z".into()),
        }
    }

    #[test]
    fn prints_the_tree_and_checks_the_config_first() {
        let mut out = Vec::new();
        run(&args(CONFIG, false), || Ok(&Forge), &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "board: no weekly budget; actual $1.50 (1 run(s)) under 1 open epic(s), all time, at list prices\n  cgwalters-forge/tracker#1: actual $1.50 (1 run(s))\n"
        );
        let args = args(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"), false);
        let error = run(
            &args,
            || -> Result<&Forge> { panic!("read") },
            &mut Vec::new(),
        );
        assert!(format!("{:#}", error.unwrap_err()).contains("Cargo.toml"));
    }

    #[test]
    fn prints_the_split_and_refuses_it_without_harness_repositories() {
        let mut out = Vec::new();
        run(&args(CONFIG, true), || Ok(&Forge), &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "spend: 0% upstream, 100% harness (0 and 1 runs)\n"
        );
        let text = std::fs::read_to_string(CONFIG).unwrap();
        let mut value: Value = serde_json::from_str(&text).unwrap();
        value.as_object_mut().unwrap().remove("budget");
        let name = format!("budget-split-{}.json", std::process::id());
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, value.to_string()).unwrap();
        let error = run(
            &args(path.to_str().unwrap(), true),
            || -> Result<&Forge> { panic!("read") },
            &mut Vec::new(),
        );
        std::fs::remove_file(&path).unwrap();
        let error = format!("{:#}", error.unwrap_err());
        assert!(
            error.contains("--split needs budget.harnessRepositories"),
            "{error}"
        );
    }
}
