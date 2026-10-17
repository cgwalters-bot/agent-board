//! Bounded operator lookup of dispatch.yml runs; names are correlation hints, not authority.
use crate::{
    github::API_ROOT,
    parse_github_item,
    poll::{Rest, next_link},
};
use anyhow::{Context, Result, ensure};
use board_core::ItemRef;

pub const DISPATCH_RUN_LIMIT: usize = 100;

pub fn find(transport: &impl Rest, caller: &str, item: &ItemRef, kind: &str) -> Result<String> {
    parse_github_item("1", Some(caller), "github.com").context("invalid caller repository")?;
    let response = transport.get(&format!("{API_ROOT}repos/{caller}/actions/workflows/dispatch.yml/runs?per_page={DISPATCH_RUN_LIMIT}&page=1"), None)?;
    ensure!(
        response.status == 200,
        "list dispatch runs: HTTP {}",
        response.status
    );
    let runs = response.body["workflow_runs"]
        .as_array()
        .context("missing workflow_runs")?;
    let total = response.body["total_count"]
        .as_u64()
        .context("missing run total_count")?;
    // A hidden second match would make a seemingly unique selection ambiguous.
    ensure!(
        total == runs.len() as u64
            && runs.len() <= DISPATCH_RUN_LIMIT
            && response.link.as_deref().and_then(next_link).is_none(),
        "dispatch run page truncated; pass --run explicitly"
    );
    let name = format!("{kind} {}#{}", item.repository, item.number);
    let matches: Vec<_> = runs
        .iter()
        .filter(|run| run["display_title"] == name)
        .collect();
    ensure!(
        matches.len() == 1,
        "expected exactly one matching dispatch run, found {}; pass --run explicitly",
        matches.len()
    );
    let url = matches[0]["html_url"]
        .as_str()
        .context("missing run html_url")?;
    let prefix = format!("https://github.com/{caller}/actions/runs/");
    let id = url
        .strip_prefix(&prefix)
        .context("run URL outside caller repository")?;
    ensure!(
        id.starts_with(|c: char| ('1'..='9').contains(&c))
            && id.bytes().all(|c| c.is_ascii_digit())
            && id.parse::<u64>().is_ok(),
        "invalid Actions run URL"
    );
    Ok(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::poll::RestResponse;
    use serde_json::{Value, json};

    struct Fake(Value);

    impl Rest for Fake {
        fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
            assert_eq!(
                url,
                "https://api.github.com/repos/example/runners/actions/workflows/dispatch.yml/runs?per_page=100&page=1"
            );
            assert!(validator.is_none());
            Ok(RestResponse {
                status: 200,
                etag: None,
                last_modified: None,
                link: None,
                body: self.0.clone(),
            })
        }

        fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
            panic!("lookup must not write")
        }
    }

    #[test]
    fn exact_unique_complete_run_only() {
        let item = parse_github_item("example/code#3", None, "github.com").unwrap();
        let run = |name: &str, repo: &str| json!({"display_title":name,"html_url":format!("https://github.com/{repo}/actions/runs/42")});
        let good = run("implement example/code#3", "example/runners");
        for (runs, total, ok) in [
            (vec![good.clone()], 1, true),
            (
                vec![
                    good.clone(),
                    run("implement example/code#30", "example/runners"),
                    run("implement other/code#3", "example/runners"),
                ],
                3,
                true,
            ),
            (
                vec![run("implement example/code#3 extra", "example/runners")],
                1,
                false,
            ),
            (
                vec![run("implement example/code#3", "other/runners")],
                1,
                false,
            ),
            (vec![good.clone(), good.clone()], 2, false),
            (vec![good], 101, false),
            (vec![], 0, false),
        ] {
            assert_eq!(
                find(
                    &Fake(json!({"workflow_runs":runs,"total_count":total})),
                    "example/runners",
                    &item,
                    "implement"
                )
                .is_ok(),
                ok
            );
        }
    }
}
