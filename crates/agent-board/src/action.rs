//! Read-only operator action checking.
use anyhow::{Context, Result, ensure};
use board_forge::{
    action::{Config, newest_action},
    github::Transport,
};
use clap::Args;
use std::{io::Write, path::PathBuf};

#[derive(Args)]
pub struct Check {
    /// Issue holding the proposal: URL, OWNER/REPO#N, or N with --repo.
    issue: String,
    /// Repository-owned configuration from a protected default-branch checkout.
    #[arg(long, value_name = "FILE")]
    config: PathBuf,
}

pub fn check<T: Transport>(
    args: &Check,
    repo: Option<&str>,
    host: &str,
    transport: impl FnOnce() -> Result<T>,
    out: &mut impl Write,
) -> Result<()> {
    ensure!(
        host == board_forge::github::DEFAULT_HOST,
        "actions support only github.com"
    );
    let issue = board_forge::github::parse_github_issue(&args.issue, repo, host)?;
    let value =
        serde_json::from_str(&std::fs::read_to_string(&args.config).context("read action config")?)
            .context("decode action config")?;
    let config = Config::from_json(&value)?;
    let comments = board_forge::plan::issue_comments(&transport()?, &issue)?;
    let (proposal, from) = newest_action(&comments, &config)?;
    for action in proposal.actions {
        serde_json::to_writer(&mut *out, action.mutation())?;
        writeln!(out)?;
    }
    eprintln!("Checked proposal in {}; nothing was changed.", from.url);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    struct Forge;

    impl Transport for Forge {
        fn graphql(&self, _: &str, _: Value) -> Result<Value> {
            let fixture: Value =
                serde_json::from_str(include_str!("../../../fixtures/operator-actions.json"))?;
            Ok(json!({"repository": {"issue": {"comments": {
                "nodes": [{"url": "comment-1", "body": format!("```agent-board-action\n{}\n```", fixture["valid"]),
                    "createdAt": "2026-10-14T00:00:00Z", "lastEditedAt": null,
                    "author": {"login": "planner-bot"}, "editor": null}],
                "pageInfo": {"hasNextPage": false}
            }}}}))
        }
    }

    #[test]
    fn prints_all_mutations_and_checks_config_before_transport() {
        let path = std::env::temp_dir().join(format!(
            "agent-board-action-test-{}.json",
            std::process::id()
        ));
        let fixture: Value =
            serde_json::from_str(include_str!("../../../fixtures/operator-actions.json")).unwrap();
        std::fs::write(&path, fixture["config"].to_string()).unwrap();
        let args = Check {
            issue: "example/work#1".into(),
            config: path.clone(),
        };
        let mut output = Vec::new();
        check(&args, None, "github.com", || Ok(Forge), &mut output).unwrap();
        let values: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        // Org-scoped mutations print like the others, unchanged and in order.
        assert_eq!(values, *fixture["valid"]["actions"].as_array().unwrap());
        assert!(values.iter().any(|v| v["type"] == "apply_org_ruleset"));
        // The proposal's org kinds need the configured org.
        for org in [None, Some(""), Some("elsewhere")] {
            let mut config = fixture["config"].clone();
            match org {
                Some(org) => config["org"] = json!(org),
                None => drop(config.as_object_mut().unwrap().remove("org")),
            }
            std::fs::write(&path, config.to_string()).unwrap();
            let mut output = Vec::new();
            assert!(check(&args, None, "github.com", || Ok(Forge), &mut output).is_err());
            assert!(output.is_empty(), "{org:?}");
        }
        for config in [
            "{}",
            r#"{"writers": ["operator"], "repositories": ["example/work"], "org": "-bad"}"#,
        ] {
            std::fs::write(&path, config).unwrap();
            assert!(
                check(
                    &args,
                    None,
                    "github.com",
                    || -> Result<Forge> { panic!("must not read token") },
                    &mut Vec::new()
                )
                .is_err()
            );
        }
        std::fs::remove_file(path).unwrap();
    }
}
