//! Body declarations shared by batching and PR dependency holds.
use crate::github::{DEFAULT_HOST, parse_github_item};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::BTreeSet;

pub const LIMIT: usize = 10;
const WORDS: &[&str] = &["blocked by", "depends on", "waiting on", "waiting for"];

/// Batching conservatively refuses even prose without a parseable reference.
pub fn declared(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    WORDS.iter().any(|word| lower.contains(word))
}

/// References following dependency wording, sorted and deduplicated. The poll
/// refuses ambiguous prose rather than silently treating it as satisfied.
pub fn parse(body: &str, repository: &str) -> Result<BTreeSet<(String, u64)>> {
    let mut refs = BTreeSet::new();
    for line in body.lines() {
        let lower = line.to_ascii_lowercase();
        let Some((at, word)) = WORDS
            .iter()
            .filter_map(|word| lower.find(word).map(|at| (at, word)))
            .min_by_key(|(at, _)| *at)
        else {
            continue;
        };
        let mut found = false;
        for token in line[at + word.len()..].split_whitespace() {
            let token = token.trim_matches(|c: char| ":,.;()<>`".contains(c));
            if !token.contains('#') && !token.starts_with("https://") {
                continue;
            }
            let local;
            let token = if token.starts_with('#') {
                local = format!("{repository}{token}");
                local.as_str()
            } else {
                token
            };
            let item = parse_github_item(token, Some(repository), DEFAULT_HOST)
                .with_context(|| format!("invalid dependency reference {token:?}"))?;
            refs.insert((item.repository.to_ascii_lowercase(), item.number));
            found = true;
            ensure!(refs.len() <= LIMIT, "more than {LIMIT} dependencies");
        }
        ensure!(found, "dependency declaration has no issue or PR reference");
    }
    Ok(refs)
}

/// REST's native blocked_by relationships share the same identity and bound as
/// body declarations. The caller must also reject a paginated response.
pub fn with_native(
    mut refs: BTreeSet<(String, u64)>,
    native: &Value,
) -> Result<BTreeSet<(String, u64)>> {
    let native = native
        .as_array()
        .context("native dependencies are not an array")?;
    ensure!(
        native.len() <= LIMIT,
        "more than {LIMIT} native dependencies"
    );
    for issue in native {
        let repo = issue["repository_url"]
            .as_str()
            .and_then(|s| s.strip_prefix("https://api.github.com/repos/"))
            .context("native dependency has no canonical repository URL")?;
        crate::poll::validate_repository(repo)?;
        let number = issue["number"]
            .as_u64()
            .filter(|n| *n > 0)
            .context("native dependency has no positive number")?;
        refs.insert((repo.to_ascii_lowercase(), number));
        ensure!(refs.len() <= LIMIT, "more than {LIMIT} dependencies");
    }
    Ok(refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_forms() {
        for body in [
            "Depends on: O/R#12",
            "Depends on #12",
            "This depends on https://github.com/O/R/pull/12.",
            "Blocked by O/R#12\nWaiting for: #12",
        ] {
            assert_eq!(
                parse(body, "o/r").unwrap(),
                [("o/r".into(), 12)].into(),
                "{body}"
            );
        }
        assert!(parse("No prerequisites", "o/r").unwrap().is_empty());
        assert!(parse("Depends on a future release", "o/r").is_err());
        assert!(parse("Depends on: o/r#12 and o/r#not-a-number", "o/r").is_err());
        let body = (1..=LIMIT + 1)
            .map(|n| format!("Depends on: #{n}\n"))
            .collect::<String>();
        assert!(parse(&body, "o/r").is_err());
        let native =
            serde_json::json!([{"repository_url":"https://api.github.com/repos/O/R","number":12}]);
        assert_eq!(
            with_native(parse("Depends on #12", "o/r").unwrap(), &native).unwrap(),
            [("o/r".into(), 12)].into()
        );
        assert!(
            with_native(
                BTreeSet::new(),
                &serde_json::json!([{"repository_url":"https://example.com/repos/o/r","number":12}])
            )
            .is_err()
        );
    }
}
