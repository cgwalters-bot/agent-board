//! Bounded operator lookup of dispatch.yml runs; names are correlation hints, not authority.
use crate::{
    github::API_ROOT,
    parse_github_item,
    poll::{Rest, next_link},
    supervise::run_title,
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
    let matches: Vec<_> = runs
        .iter()
        .filter(|run| {
            run["display_title"]
                .as_str()
                .is_some_and(|title| title_matches(title, item, kind))
        })
        .collect();
    ensure!(
        matches.len() == 1,
        "expected exactly one matching dispatch run, found {}; pass --run explicitly",
        matches.len()
    );
    let url = matches[0]["html_url"]
        .as_str()
        .context("missing run html_url")?;
    // GitHub returns the canonical spelling; the caller may be written in any case.
    let prefix = format!("https://github.com/{caller}/actions/runs/");
    let id = url
        .get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(&prefix))
        .and_then(|_| url.get(prefix.len()..))
        .context("run URL outside caller repository")?;
    ensure!(
        id.starts_with(|c: char| ('1'..='9').contains(&c))
            && id.bytes().all(|c| c.is_ascii_digit())
            && id.parse::<u64>().is_ok(),
        "invalid Actions run URL"
    );
    Ok(url.into())
}

/// dispatch.yml names a run `KIND OWNER/REPO#N`, plus ` (POOL)` when a pool is given.
/// Anything else after the target stays a non-match, so a title cannot claim an item
/// by merely mentioning it.
fn title_matches(title: &str, item: &ItemRef, kind: &str) -> bool {
    let Some((found_kind, repo, number)) = run_title(title) else {
        return false;
    };
    // run_title has checked `KIND OWNER/REPO#` is ASCII, so this skips exactly that prefix.
    let Some(suffix) = title.get(found_kind.len() + repo.len() + 2..) else {
        return false;
    };
    let suffix = suffix.trim_start_matches(|c: char| c.is_ascii_digit());
    let pooled = suffix
        .strip_prefix(" (")
        .and_then(|s| s.strip_suffix(')'))
        .is_some_and(|pool| {
            !pool.is_empty() && !pool.contains(|c: char| c.is_whitespace() || "()".contains(c))
        });
    found_kind == kind
        && repo.eq_ignore_ascii_case(&item.repository)
        && number == item.number
        && (suffix.is_empty() || pooled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::poll::RestResponse;
    use serde_json::{Value, json};

    const RUNNERS: &str = "example/runners";

    struct Fake(Value);

    impl Rest for Fake {
        fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
            assert!(url.eq_ignore_ascii_case(
                "https://api.github.com/repos/example/runners/actions/workflows/dispatch.yml/runs?per_page=100&page=1"
            ));
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
                vec![
                    good.clone(),
                    run("implement example/code#3 (small)", RUNNERS),
                ],
                2,
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
                ok,
                "{runs:?}"
            );
        }
    }

    #[test]
    fn pooled_titles_and_caller_case() {
        let item = parse_github_item("example/code#3", None, "github.com").unwrap();
        let url = "https://github.com/example/runners/actions/runs/42";
        let case = |title: &str, url: &str, caller: &str| {
            let fake = Fake(json!({"total_count": 1, "workflow_runs": [
                {"display_title": title, "html_url": url}]}));
            find(&fake, caller, &item, "implement").is_ok()
        };
        for (title, ok) in [
            ("implement example/code#3 (claude)", true),
            ("implement Example/Code#3 (claude)", true),
            ("implement example/code#3 extra", false),
            ("implement example/code#3 (claude) extra", false),
            ("implement example/code#3 ()", false),
            ("implement example/code#3 (a b)", false),
            ("implement example/code#3(claude)", false),
            ("implement example/code#30 (claude)", false),
            ("review example/code#3 (claude)", false),
        ] {
            assert_eq!(case(title, url, RUNNERS), ok, "{title}");
        }
        for (url, ok) in [
            (url, true),
            ("https://github.com/example/runners/actions/runs/042", false),
            ("https://github.com/example/runners/actions/runs/4x", false),
            ("https://github.com/example/runner/actions/runs/42", false),
        ] {
            let title = "implement example/code#3";
            assert_eq!(case(title, url, "Example/Runners"), ok, "{url}");
        }
    }
}
