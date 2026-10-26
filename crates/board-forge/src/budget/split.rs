//! `agent-board budget --split` (docs/pass.md#budget-tree, agent-board#355):
//! the share of a window's public agent spend that went to upstream work
//! rather than to the harness, as percentages, never dollars.
//!
//! A run is in the window when its status comment last changed in it: one
//! search finds the items the bot commented on that were updated since the
//! window started, and each one's comments are read under the budget tree's bounds.
//! The status comment and its cost line count under the tree's rules
//! ([`costs`]), and each run once. A run is the harness's when its status
//! comment is on an item in `budget.harnessRepositories`, upstream otherwise.
//! A private repository's items are left out, as `changes --public-only`
//! leaves them out, since the line is published.
use super::{Cost, Counted, Spend, add, costs, run_line, trusted};
use crate::changes::format_time;
use crate::github::{DEFAULT_HOST, Transport};
use crate::pass::Config;
use crate::plan::{Comment, issue_comments, pull_request_comments};
use crate::poll::validate_repository;
use anyhow::{Context, Result, bail, ensure};
use board_core::ItemRef;
use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use time::OffsetDateTime;

/// The most results GitHub's search returns; a window with more is refused,
/// never read in part.
pub const SEARCH_LIMIT: u64 = 1000;
const PAGE: u64 = 100;

const SEARCH_QUERY: &str = r#"query($q: String!, $after: String) {
  search(query: $q, type: ISSUE, first: 100, after: $after) {
    issueCount
    nodes {
      __typename
      ... on Issue { number repository { nameWithOwner isPrivate } }
      ... on PullRequest { number repository { nameWithOwner isPrivate } }
    }
    pageInfo { hasNextPage endCursor }
  }
}"#;

/// A window's priced runs on each side, and the runs whose status comment
/// has no cost line.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Split {
    pub upstream: Spend,
    pub harness: Spend,
    pub unpriced: usize,
}

/// The split of the runs whose status comment by the configuration's `bot`
/// last changed in [since, until), in public repositories.
pub fn read<T: Transport>(
    client: &T,
    config: &Config,
    since: OffsetDateTime,
    until: OffsetDateTime,
) -> Result<Split> {
    let harness = config.harness_repositories().context(
        "the split needs budget.harnessRepositories, which the configuration does not set",
    )?;
    ensure!(until > since, "the window must end after it starts");
    let client = Counted {
        inner: client,
        used: Cell::new(0),
    };
    // No upper bound: an item updated after the window can still hold a
    // status comment from it, and the comments are filtered below.
    let q = format!("commenter:{} updated:>={}", config.bot, format_time(since)?);
    let mut runs: BTreeMap<u64, (bool, Option<Cost>)> = BTreeMap::new();
    for (_, number, pull, repository) in items(&client, &q)? {
        let item = ItemRef {
            host: DEFAULT_HOST.into(),
            repository,
            number,
        };
        let mut comments = if pull {
            pull_request_comments(&client, &item)?
        } else {
            issue_comments(&client, &item)?
        };
        comments.retain(|c| c.changed >= since && c.changed < until);
        let mut priced = BTreeMap::new();
        costs(&comments, &config.bot, &mut priced);
        let side = harness
            .iter()
            .any(|h| h.eq_ignore_ascii_case(&item.repository));
        for run in status_runs(&comments, &config.bot) {
            // On the first item it is on, priced where any copy is.
            let entry = runs.entry(run).or_insert((side, None));
            entry.1 = entry.1.or(priced.get(&run).copied());
        }
    }
    let mut split = Split::default();
    for (harness, cost) in runs.into_values() {
        let Some(cost) = cost else {
            split.unpriced += 1;
            continue;
        };
        let side = if harness {
            &mut split.harness
        } else {
            &mut split.upstream
        };
        *side = add(
            *side,
            Spend {
                usd: cost.usd,
                runs: 1,
                partial: cost.partial,
            },
        );
    }
    Ok(split)
}

/// The public items the search `q` finds, sorted by repository and number:
/// `(key, number, is a pull request, repository)`.
fn items<T: Transport>(
    client: &Counted<'_, T>,
    q: &str,
) -> Result<BTreeSet<(String, u64, bool, String)>> {
    let mut items = BTreeSet::new();
    let mut after = Value::Null;
    for _ in 0..SEARCH_LIMIT / PAGE {
        let data = client.graphql(SEARCH_QUERY, json!({"q": q, "after": after}))?;
        let search = data
            .get("search")
            .filter(|s| s.is_object())
            .context("the search for the bot's comments could not be read")?;
        let count = search["issueCount"]
            .as_u64()
            .context("the search has no issueCount")?;
        ensure!(
            count <= SEARCH_LIMIT,
            "the bot commented on {count} items in the window, more than the {SEARCH_LIMIT} a search returns"
        );
        for node in search["nodes"]
            .as_array()
            .context("the search has no nodes")?
        {
            let pull = match node["__typename"].as_str() {
                Some("Issue") => false,
                Some("PullRequest") => true,
                _ => bail!("the search found something other than an issue or pull request"),
            };
            // Unread visibility counts as private, and before anything else
            // so that no error names a private repository: errors are logged.
            let private = node["repository"]["isPrivate"]
                .as_bool()
                .context("a found item's visibility could not be read")?;
            if private {
                continue;
            }
            let repository = node["repository"]["nameWithOwner"]
                .as_str()
                .context("a found item has no repository")?;
            validate_repository(repository)?;
            let number = node["number"]
                .as_u64()
                .context("a found item has no number")?;
            items.insert((
                repository.to_ascii_lowercase(),
                number,
                pull,
                repository.to_owned(),
            ));
        }
        match search["pageInfo"]["hasNextPage"].as_bool() {
            Some(false) => return Ok(items),
            Some(true) => {
                after = search["pageInfo"]["endCursor"].clone();
                ensure!(after.is_string(), "a search page has no cursor");
            }
            None => bail!("a search page has no pageInfo"),
        }
    }
    bail!("the search has more than {SEARCH_LIMIT} results")
}

/// The runs whose status comment is among `comments`: a run marker line in a
/// comment by `bot` that no one else last edited.
fn status_runs(comments: &[Comment], bot: &str) -> BTreeSet<u64> {
    comments
        .iter()
        .filter(|c| trusted(c, bot))
        .flat_map(|c| c.body.lines().filter_map(run_line))
        .collect()
}

/// The split as one line of percentages of the summed known cost.
pub fn line(split: &Split) -> String {
    let (upstream, harness) = (split.upstream, split.harness);
    let total = upstream.usd + harness.usd;
    let mut notes = vec![];
    let head = if total > 0.0 {
        let share = (upstream.usd / total * 100.0).round() as u64;
        notes.push(format!("{} and {} runs", upstream.runs, harness.runs));
        format!("{share}% upstream, {}% harness", 100 - share)
    } else {
        "no priced runs".to_owned()
    };
    if split.unpriced > 0 {
        notes.push(format!("{} without a cost", split.unpriced));
    }
    if total > 0.0 && (upstream.partial || harness.partial) {
        notes.push("at least, partial costs".to_owned());
    }
    if notes.is_empty() {
        format!("spend: {head}")
    } else {
        format!("spend: {head} ({})", notes.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::{COST_MARKER, RUN_MARKER};

    const CONFIG: &str = include_str!("../../../../fixtures/pass/config.json");
    const BOT: &str = "cgwalters-bot";
    const SINCE: &str = "2026-10-19T00:00:00Z";
    const UNTIL: &str = "2026-10-26T00:00:00Z";

    fn at(time: &str) -> OffsetDateTime {
        crate::changes::parse_time(time).unwrap()
    }

    fn config() -> Config {
        Config::parse(CONFIG).unwrap()
    }

    /// The search's results, two a page, and each item's comments by
    /// `OWNER/NAME#N`: `[author, body, editor, createdAt, lastEditedAt]`.
    struct Forge {
        found: Vec<Value>,
        count: Option<u64>,
        comments: BTreeMap<String, Value>,
    }

    impl Transport for Forge {
        fn graphql(&self, query: &str, v: Value) -> Result<Value> {
            if query == SEARCH_QUERY {
                assert_eq!(v["q"], format!("commenter:{BOT} updated:>={SINCE}"));
                let found: Vec<_> = (self.found.iter())
                    .filter(|n| n["updatedAt"].as_str().unwrap() >= SINCE)
                    .collect();
                let start = v["after"].as_str().map_or(0, |a| a.parse().unwrap());
                let end = found.len().min(start + 2);
                let more = end < found.len();
                return Ok(json!({"search": {
                    "issueCount": self.count.unwrap_or(found.len() as u64),
                    "nodes": found[start..end],
                    "pageInfo": {"hasNextPage": more, "endCursor": more.then(|| end.to_string())}}}));
            }
            let name = format!(
                "{}/{}#{}",
                v["owner"].as_str().unwrap(),
                v["name"].as_str().unwrap(),
                v["number"]
            );
            let field = if query.contains("pullRequest(") {
                "pullRequest"
            } else {
                "issue"
            };
            let comments = self
                .comments
                .get(&format!("{field} {name}"))
                .with_context(|| format!("no {field} {name}"))?;
            let nodes: Vec<_> = (comments.as_array().unwrap().iter().enumerate())
                .map(|(i, c)| {
                    json!({"url": format!("https://github.com/{name}#issuecomment-{i}"),
                    "body": c[1], "createdAt": c[3], "lastEditedAt": c[4],
                    "author": {"login": c[0]},
                    "editor": if c[2].is_null() { Value::Null } else { json!({"login": c[2]}) }})
                })
                .collect();
            Ok(json!({"repository": {field: {"comments": {"nodes": nodes,
                "pageInfo": {"hasNextPage": false, "endCursor": null}}}}}))
        }
    }

    /// A search result, with the item's `updatedAt` for the fake's filter.
    fn found(kind: &str, repository: &str, number: u64, private: bool, updated: &str) -> Value {
        json!({"__typename": kind, "number": number, "updatedAt": updated,
            "repository": {"nameWithOwner": repository, "isPrivate": private}})
    }

    fn status(run: u64, cost: Option<(&str, bool)>) -> String {
        let cost = cost.map_or(String::new(), |(usd, partial)| {
            format!("<!-- {COST_MARKER}: {run} usd={usd} partial={partial} -->\n\n")
        });
        format!("Finished.\n\n{cost}<!-- {RUN_MARKER}: {run} -->")
    }

    const IN: &str = "2026-10-20T00:00:00Z";
    const BEFORE: &str = "2026-10-10T00:00:00Z";
    const AFTER: &str = "2026-10-27T00:00:00Z";

    /// Upstream: an issue's run and a pull request's partial one, beside a
    /// visitor's pasted line and a line with no run marker. The harness: a
    /// priced run, one without a cost, one edited after the window and one
    /// unedited since before it. A private repository and an item last
    /// updated before the window, never read; the upstream issue was updated
    /// after the window, and its runs in the window still count.
    fn forge() -> Forge {
        let comments = [
            (
                "issue cgwalters-forge/bootc#1",
                json!([
                    [BOT, status(10, Some(("6", false))), null, IN, null],
                    ["visitor", status(11, Some(("99", false))), null, IN, null],
                    [
                        BOT,
                        format!("<!-- {COST_MARKER}: 12 usd=99 partial=false -->"),
                        null,
                        IN,
                        null
                    ],
                    // A rerun's edit by someone else is not the bot's.
                    [BOT, status(13, Some(("99", false))), "visitor", BEFORE, IN],
                    // Unpriced here, priced on the pull request: it counts priced.
                    [BOT, status(14, None), null, IN, null],
                ]),
            ),
            (
                "pullRequest cgwalters-forge/composefs-rs#5",
                json!([[BOT, status(14, Some(("2", true))), BOT, BEFORE, IN]]),
            ),
            (
                "issue cgwalters-forge/agent-board#7",
                json!([
                    [BOT, status(20, Some(("4", false))), null, IN, null],
                    [BOT, status(21, None), null, IN, null],
                    [BOT, status(22, Some(("99", false))), BOT, BEFORE, AFTER],
                    [BOT, status(23, Some(("99", false))), null, BEFORE, null],
                    // The same run's comment twice counts once.
                    [BOT, status(20, Some(("4", false))), null, IN, null],
                ]),
            ),
        ];
        Forge {
            found: vec![
                found("Issue", "cgwalters-forge/bootc", 1, false, AFTER),
                found("Issue", "cgwalters-forge/bootc", 2, false, BEFORE),
                found("Issue", "cgwalters-forge/bot-ops", 1, true, IN),
                found("Issue", "cgwalters-forge/agent-board", 7, false, IN),
                found("PullRequest", "cgwalters-forge/composefs-rs", 5, false, IN),
            ],
            count: None,
            comments: comments.into_iter().map(|(k, v)| (k.into(), v)).collect(),
        }
    }

    #[test]
    fn splits_a_window_between_upstream_and_the_harness() {
        type Edit = fn(&mut Forge);
        let cases: [(&str, Edit, &str); 4] = [
            (
                "mixed",
                |_| {},
                "spend: 67% upstream, 33% harness (2 and 1 runs; 1 without a cost; at least, partial costs)",
            ),
            (
                "only a run without a cost",
                |f| {
                    f.found.retain(|n| n["number"] == 7);
                    f.comments.insert(
                        "issue cgwalters-forge/agent-board#7".into(),
                        json!([[BOT, status(21, None), null, IN, null]]),
                    );
                },
                "spend: no priced runs (1 without a cost)",
            ),
            ("nothing", |f| f.found.clear(), "spend: no priced runs"),
            (
                "only upstream",
                |f| f.found.retain(|n| n["number"] == 1),
                "spend: 100% upstream, 0% harness (1 and 0 runs; 1 without a cost)",
            ),
        ];
        for (name, edit, expected) in cases {
            let mut forge = forge();
            edit(&mut forge);
            let split = read(&forge, &config(), at(SINCE), at(UNTIL)).unwrap();
            assert_eq!(line(&split), expected, "{name}");
        }
        let split = read(&forge(), &config(), at(SINCE), at(UNTIL)).unwrap();
        assert_eq!(split.upstream.usd, 8.0);
        assert_eq!(split.harness.usd, 4.0);
    }

    #[test]
    fn unreadable_splits_are_refused() {
        type Edit = fn(&mut Forge);
        let cases: [(Edit, &str); 4] = [
            (
                |f| f.count = Some(SEARCH_LIMIT + 1),
                "the bot commented on 1001 items in the window, more than the 1000 a search returns",
            ),
            (
                |f| f.found[0]["repository"]["isPrivate"] = Value::Null,
                "a found item's visibility could not be read",
            ),
            (
                |f| f.found[0]["__typename"] = json!("Discussion"),
                "other than an issue or pull request",
            ),
            (
                |f| {
                    f.comments.remove("issue cgwalters-forge/agent-board#7");
                },
                "no issue cgwalters-forge/agent-board#7",
            ),
        ];
        for (edit, expected) in cases {
            let mut forge = forge();
            edit(&mut forge);
            let error = read(&forge, &config(), at(SINCE), at(UNTIL)).unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
        let error = read(&forge(), &config(), at(UNTIL), at(SINCE)).unwrap_err();
        assert!(error.to_string().contains("must end after"), "{error}");
        let mut value: Value = serde_json::from_str(CONFIG).unwrap();
        value.as_object_mut().unwrap().remove("budget");
        let config = Config::parse(&value.to_string()).unwrap();
        let error = read(&forge(), &config, at(SINCE), at(UNTIL)).unwrap_err();
        assert!(
            error.to_string().contains("budget.harnessRepositories"),
            "{error}"
        );
    }
}
