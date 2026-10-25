//! `agent-board budget` (docs/pass.md#budget-tree, agent-board#281): what
//! each item under the board's open epics has cost, its own runs' and its
//! sub-issues', beside the board's weekly budget and each item's
//! [`BUDGET_FIELD`]. Read-only: nothing is held on it yet.
//!
//! A run's cost is the line agentic-job writes into the run's status comment
//! on its item (agentic-job#1945), `<!-- agentic-job-cost: RUN usd=N
//! partial=B -->`, at API list prices. Only such a line on a line of its own,
//! in a comment by the pass's bot that no one else last edited, counts:
//! anyone may comment on a public issue, and a pasted line must not move an
//! item's actual. A run whose cost was unknown leaves no line, a partial cost
//! is a lower bound, and a rerun that edits the item's one marked status
//! comment replaces the line before it, so an actual is at least what it says.
use crate::epic::EPIC_LABEL;
use crate::github::{DEFAULT_HOST, Transport};
use crate::pass::{Board, Config, board_item};
use crate::plan::{Comment, issue_comments};
use crate::poll::validate_repository;
use anyhow::{Context, Result, bail, ensure};
use board_core::ItemRef;
use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

/// The marker of a run's cost line.
pub const COST_MARKER: &str = "agentic-job-cost";
/// The board's number field that holds an item's budget in dollars.
pub const BUDGET_FIELD: &str = "Budget ($)";
/// Open epics read, one page; more is an error.
const EPIC_LIMIT: usize = 100;
/// How deep sub-issues are followed below an epic: GitHub's own limit.
const DEPTH_LIMIT: usize = 8;
/// Items read in all, epics included; more is an error, never a smaller total.
pub const ITEM_LIMIT: usize = 300;
/// Forge requests in all: an item's read and its pages of comments.
pub const REQUEST_LIMIT: usize = 1000;

const EPICS_QUERY: &str = r#"query($owner: String!, $name: String!, $label: String!) {
  repository(owner: $owner, name: $name) {
    issues(first: 100, states: OPEN, labels: [$label], orderBy: {field: CREATED_AT, direction: ASC}) {
      nodes { number }
      pageInfo { hasNextPage }
    }
  }
}"#;

/// An item's state, its budget on each board and its sub-issues.
const ITEM_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!, $field: String!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      state
      projectItems(first: 10) {
        nodes {
          project { number owner { ... on Organization { login } ... on User { login } } }
          budget: fieldValueByName(name: $field) { ... on ProjectV2ItemFieldNumberValue { number } }
        }
        totalCount
      }
      subIssues(first: 100) { nodes { number repository { nameWithOwner } } totalCount }
    }
  }
}"#;

/// One run's cost in dollars, and whether it is only a lower bound.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cost {
    pub usd: f64,
    pub partial: bool,
}

/// What a set of runs cost.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spend {
    pub usd: f64,
    pub runs: usize,
    pub partial: bool,
}

/// An item and the items below it.
#[derive(Clone, Debug)]
pub struct Item {
    pub issue: ItemRef,
    pub open: bool,
    /// Its [`BUDGET_FIELD`] on the configured board, where set.
    pub budget: Option<f64>,
    /// Its own runs' costs, by run ID.
    pub runs: BTreeMap<u64, Cost>,
    pub children: Vec<Item>,
}

impl Item {
    /// Its own runs' cost and its descendants'.
    pub fn actual(&self) -> Spend {
        let own = Spend {
            // Not `sum`, whose empty sum is -0.0.
            usd: self.runs.values().fold(0.0, |usd, c| usd + c.usd),
            runs: self.runs.len(),
            partial: self.runs.values().any(|c| c.partial),
        };
        self.children.iter().map(Item::actual).fold(own, add)
    }

    fn descendants(&self, into: &mut BTreeSet<String>) {
        for child in &self.children {
            into.insert(key(&child.issue));
            child.descendants(into);
        }
    }
}

fn add(a: Spend, b: Spend) -> Spend {
    Spend {
        usd: a.usd + b.usd,
        runs: a.runs + b.runs,
        partial: a.partial || b.partial,
    }
}

fn key(issue: &ItemRef) -> String {
    issue.display().to_ascii_lowercase()
}

/// The budget tree: the board's weekly budget, and its open epics.
#[derive(Clone, Debug)]
pub struct Tree {
    pub weekly_usd: Option<f64>,
    pub epics: Vec<Item>,
}

/// The run costs on `comments` that count: lines of their own in comments
/// by `bot` that no one else last edited, each run once.
pub fn costs(comments: &[Comment], bot: &str) -> BTreeMap<u64, Cost> {
    let by_bot = |login: Option<&str>| login.is_some_and(|l| l.eq_ignore_ascii_case(bot));
    let mut runs = BTreeMap::new();
    for comment in comments.iter().filter(|c| {
        by_bot(c.author.as_deref())
            && (!c.edited && c.editor.is_none() || by_bot(c.editor.as_deref()))
    }) {
        for (run, cost) in comment.body.lines().filter_map(cost_line) {
            runs.entry(run).or_insert(cost);
        }
    }
    runs
}

/// A cost line as agentic-job writes it, or `None`.
fn cost_line(line: &str) -> Option<(u64, Cost)> {
    let rest = line
        .strip_prefix(&format!("<!-- {COST_MARKER}: "))?
        .strip_suffix(" -->")?;
    let mut words = rest.split(' ');
    let run = words.next()?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(run) || run.starts_with('0') {
        return None;
    }
    let usd = words.next()?.strip_prefix("usd=")?;
    let (whole, fraction) = usd.split_once('.').unwrap_or((usd, "0"));
    if !digits(whole) || !digits(fraction) {
        return None;
    }
    let partial = match words.next()? {
        "partial=true" => true,
        "partial=false" => false,
        _ => return None,
    };
    let usd: f64 = usd.parse().ok().filter(|u: &f64| u.is_finite())?;
    words
        .next()
        .is_none()
        .then_some((run.parse().ok()?, Cost { usd, partial }))
}

/// Counts requests against [`REQUEST_LIMIT`].
struct Counted<'a, T> {
    inner: &'a T,
    used: Cell<usize>,
}

impl<T: Transport> Transport for Counted<'_, T> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        ensure!(
            self.used.get() < REQUEST_LIMIT,
            "the budget tree takes more than {REQUEST_LIMIT} requests"
        );
        self.used.set(self.used.get() + 1);
        self.inner.graphql(query, variables)
    }
}

struct Reader<'a, T> {
    client: Counted<'a, T>,
    bot: &'a str,
    board: Option<&'a Board>,
    items: Cell<usize>,
}

impl<T: Transport> Reader<'_, T> {
    fn read(&self, issue: ItemRef, depth: usize) -> Result<Item> {
        ensure!(
            self.items.get() < ITEM_LIMIT,
            "the board's epics have more than {ITEM_LIMIT} items"
        );
        self.items.set(self.items.get() + 1);
        let (owner, name) = issue.repository.split_once('/').context("repository")?;
        let data = self.client.graphql(
            ITEM_QUERY,
            json!({"owner": owner, "name": name, "number": issue.number, "field": BUDGET_FIELD}),
        )?;
        let node = data
            .pointer("/repository/issue")
            .filter(|n| n.is_object())
            .with_context(|| format!("{} is not an issue", issue.display()))?;
        let open = match node["state"].as_str() {
            Some("OPEN") => true,
            Some("CLOSED") => false,
            _ => bail!("{} has no state", issue.display()),
        };
        let budget = budget(node, self.board).with_context(|| issue.display())?;
        let subs = &node["subIssues"];
        let nodes = subs["nodes"]
            .as_array()
            .filter(|n| subs["totalCount"].as_u64() == Some(n.len() as u64))
            .with_context(|| {
                format!(
                    "the sub-issues of {} could not all be read",
                    issue.display()
                )
            })?;
        ensure!(
            nodes.is_empty() || depth < DEPTH_LIMIT,
            "{} is more than {DEPTH_LIMIT} levels below an epic",
            issue.display()
        );
        let runs = costs(&issue_comments(&self.client, &issue)?, self.bot);
        let children = nodes
            .iter()
            .map(|sub| {
                let repository = sub["repository"]["nameWithOwner"]
                    .as_str()
                    .context("sub-issue has no repository")?;
                validate_repository(repository)?;
                let number = sub["number"].as_u64().context("sub-issue has no number")?;
                self.read(
                    ItemRef {
                        host: DEFAULT_HOST.into(),
                        repository: repository.into(),
                        number,
                    },
                    depth + 1,
                )
            })
            .collect::<Result<_>>()?;
        Ok(Item {
            issue,
            open,
            budget,
            runs,
            children,
        })
    }
}

/// An item's [`BUDGET_FIELD`] on `board`, or an error when not all of its
/// boards were read: one missing from a partial read would look unset.
fn budget(node: &Value, board: Option<&Board>) -> Result<Option<f64>> {
    let items = node["projectItems"]["nodes"]
        .as_array()
        .context("no project items")?;
    ensure!(
        node["projectItems"]["totalCount"].as_u64() == Some(items.len() as u64),
        "not all of its boards could be read"
    );
    let Some(value) = board
        .and_then(|b| board_item(items, b))
        .map(|i| &i["budget"]["number"])
    else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        value => value
            .as_f64()
            .filter(|usd| usd.is_finite() && *usd >= 0.0)
            .map(Some)
            .with_context(|| format!("its {BUDGET_FIELD} {value} is not a dollar amount")),
    }
}

/// The open epics of the configuration's `epics.repository`, each with the
/// items below it, and the board's weekly budget. An epic that is itself
/// below another is shown there only, so no run counts twice.
pub fn read<T: Transport>(client: &T, config: &Config) -> Result<Tree> {
    let epics = config
        .epics
        .as_ref()
        .context("the budget tree is read from the epics of epics.repository, which the configuration does not set")?;
    let (owner, name) = epics
        .repository
        .split_once('/')
        .context("epics repository")?;
    let reader = Reader {
        client: Counted {
            inner: client,
            used: Cell::new(0),
        },
        bot: &config.bot,
        board: config.board.as_ref(),
        items: Cell::new(0),
    };
    let data = reader.client.graphql(
        EPICS_QUERY,
        json!({"owner": owner, "name": name, "label": EPIC_LABEL}),
    )?;
    let connection = data
        .pointer("/repository/issues")
        .filter(|c| c.is_object())
        .with_context(|| format!("the epics of {} could not be read", epics.repository))?;
    ensure!(
        connection["pageInfo"]["hasNextPage"] == false,
        "{} has more than {EPIC_LIMIT} open epics, or GitHub did not say",
        epics.repository
    );
    let mut items = Vec::new();
    for node in connection["nodes"]
        .as_array()
        .context("epics have no nodes")?
    {
        let number = node["number"].as_u64().context("an epic has no number")?;
        let issue = ItemRef {
            host: DEFAULT_HOST.into(),
            repository: epics.repository.clone(),
            number,
        };
        items.push(reader.read(issue, 0)?);
    }
    let mut below = BTreeSet::new();
    for item in &items {
        item.descendants(&mut below);
    }
    items.retain(|i| !below.contains(&key(&i.issue)));
    Ok(Tree {
        weekly_usd: config.budget.as_ref().and_then(|b| b.weekly_usd),
        epics: items,
    })
}

fn dollars(usd: f64) -> String {
    format!("${usd:.2}")
}

fn actual(spend: Spend) -> String {
    let at_least = if spend.partial { "at least " } else { "" };
    format!(
        "actual {at_least}{} ({} run(s))",
        dollars(spend.usd),
        spend.runs
    )
}

/// The tree as lines: the board first, then each item indented below its
/// parent, with its budget where set.
pub fn lines(tree: &Tree) -> Vec<String> {
    let total = (tree.epics.iter().map(Item::actual)).fold(Spend::default(), add);
    let budget = tree.weekly_usd.map_or("no weekly budget".into(), |usd| {
        format!("budget {} a week", dollars(usd))
    });
    let mut out = vec![format!(
        "board: {budget}; {} under {} open epic(s), all time, at list prices",
        actual(total),
        tree.epics.len()
    )];
    fn walk(item: &Item, depth: usize, out: &mut Vec<String>) {
        let spend = item.actual();
        let closed = if item.open { "" } else { ", closed" };
        let budget = item.budget.map_or(String::new(), |usd| {
            let over = if spend.usd > usd { ", over" } else { "" };
            format!("budget {}{over}, ", dollars(usd))
        });
        out.push(format!(
            "{}{}{closed}: {budget}{}",
            "  ".repeat(depth),
            item.issue.display(),
            actual(spend)
        ));
        for child in &item.children {
            walk(child, depth + 1, out);
        }
    }
    for epic in &tree.epics {
        walk(epic, 1, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = include_str!("../../../fixtures/pass/config.json");

    fn config(edit: Value) -> Config {
        let mut value: Value = serde_json::from_str(CONFIG).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .extend(edit.as_object().unwrap().clone());
        Config::parse(&value.to_string()).unwrap()
    }

    /// A forge of issues by `OWNER/REPO#N`: state, budget on board 1 (or
    /// `null`), sub-issues and comments; and the open epics of tracker.
    struct Forge {
        epics: Vec<u64>,
        issues: BTreeMap<String, Value>,
        reads: Cell<usize>,
    }

    impl Transport for Forge {
        fn graphql(&self, query: &str, v: Value) -> Result<Value> {
            self.reads.set(self.reads.get() + 1);
            if query == EPICS_QUERY {
                assert_eq!(v["label"], EPIC_LABEL);
                let nodes: Vec<_> = self.epics.iter().map(|n| json!({"number": n})).collect();
                return Ok(json!({"repository": {"issues": {"nodes": nodes,
                    "pageInfo": {"hasNextPage": false}}}}));
            }
            let name = format!(
                "{}/{}#{}",
                v["owner"].as_str().unwrap(),
                v["name"].as_str().unwrap(),
                v["number"]
            );
            let issue = self
                .issues
                .get(&name)
                .with_context(|| format!("no {name}"))?;
            if query == ITEM_QUERY {
                assert_eq!(v["field"], BUDGET_FIELD);
                let subs: Vec<_> = (issue["subs"].as_array().unwrap().iter())
                    .map(|s| {
                        let (repo, n) = s.as_str().unwrap().split_once('#').unwrap();
                        json!({"number": n.parse::<u64>().unwrap(), "repository": {"nameWithOwner": repo}})
                    })
                    .collect();
                let board = json!({"project": {"number": 1, "owner": {"login": "cgwalters-forge"}},
                    "budget": if issue["budget"].is_null() { Value::Null } else { json!({"number": issue["budget"]}) }});
                return Ok(json!({"repository": {"issue": {"state": issue["state"],
                    "projectItems": {"nodes": [board], "totalCount": 1},
                    "subIssues": {"nodes": subs, "totalCount": subs.len()}}}}));
            }
            let comments: Vec<_> = (issue["comments"].as_array().unwrap().iter().enumerate())
                .map(|(i, c)| json!({"url": format!("https://github.com/{name}#issuecomment-{i}"),
                    "body": c[1], "createdAt": "2026-10-24T00:00:00Z",
                    "lastEditedAt": if c[2].is_null() { Value::Null } else { "2026-10-24T01:00:00Z".into() },
                    "author": {"login": c[0]}, "editor": if c[2].is_null() { Value::Null } else { json!({"login": c[2]}) }}))
                .collect();
            Ok(
                json!({"repository": {"issue": {"comments": {"nodes": comments,
                "pageInfo": {"hasNextPage": false, "endCursor": null}}}}}),
            )
        }
    }

    fn issue(state: &str, budget: Value, subs: &[&str], comments: Value) -> Value {
        json!({"state": state, "budget": budget, "subs": subs, "comments": comments})
    }

    fn cost(run: u64, usd: &str, partial: bool) -> String {
        format!(
            "Finished.\n\n<!-- {COST_MARKER}: {run} usd={usd} partial={partial} -->\n\n<!-- agentic-job-run: {run} -->"
        )
    }

    const BOT: &str = "cgwalters-bot";

    /// An epic with a budget, its sub-issues' runs (an implement, its fix
    /// rounds and a rerun) and a grandchild's, and an epic nested in it.
    fn forge() -> Forge {
        let issues = [
            (
                "cgwalters-forge/tracker#10",
                issue(
                    "OPEN",
                    json!(50),
                    &[
                        "cgwalters-forge/agent-board#5",
                        "cgwalters-forge/agent-board#6",
                        "cgwalters-forge/tracker#11",
                    ],
                    json!([[BOT, cost(1, "0.5", false), BOT]]),
                ),
            ),
            (
                "cgwalters-forge/agent-board#5",
                issue(
                    "CLOSED",
                    Value::Null,
                    &[],
                    json!([
                        [BOT, cost(2, "10", false), BOT],
                        [BOT, cost(3, "4.25", false), Value::Null],
                        [BOT, cost(4, "2.25", false), BOT],
                        // The same run's line again counts once.
                        [BOT, cost(4, "2.25", false), BOT]
                    ]),
                ),
            ),
            (
                "cgwalters-forge/agent-board#6",
                issue(
                    "OPEN",
                    json!(5),
                    &["cgwalters-forge/agent-board#7"],
                    json!([[BOT, cost(5, "3", true), BOT]]),
                ),
            ),
            (
                "cgwalters-forge/agent-board#7",
                issue(
                    "OPEN",
                    Value::Null,
                    &[],
                    json!([[BOT, cost(6, "6.0001", false), BOT]]),
                ),
            ),
            (
                "cgwalters-forge/tracker#11",
                issue("OPEN", Value::Null, &[], json!([])),
            ),
            (
                "cgwalters-forge/tracker#12",
                issue(
                    "OPEN",
                    Value::Null,
                    &[],
                    json!([
                        // Not counted: a visitor's, the bot's edited by someone else,
                        // and one quoted inline.
                        ["visitor", cost(7, "99", false), Value::Null],
                        [BOT, cost(8, "99", false), "cgwalters"],
                        [
                            BOT,
                            format!("Say `<!-- {COST_MARKER}: 9 usd=99 partial=false -->`."),
                            Value::Null
                        ]
                    ]),
                ),
            ),
        ];
        Forge {
            epics: vec![10, 11, 12],
            issues: issues.into_iter().map(|(k, v)| (k.into(), v)).collect(),
            reads: Cell::new(0),
        }
    }

    #[test]
    fn tree_sums_runs_with_descendants() {
        let forge = forge();
        let tree = read(&forge, &config(json!({}))).unwrap();
        assert_eq!(
            lines(&tree),
            [
                "board: no weekly budget; actual at least $26.00 (6 run(s)) under 2 open epic(s), all time, at list prices",
                "  cgwalters-forge/tracker#10: budget $50.00, actual at least $26.00 (6 run(s))",
                "    cgwalters-forge/agent-board#5, closed: actual $16.50 (3 run(s))",
                "    cgwalters-forge/agent-board#6: budget $5.00, over, actual at least $9.00 (2 run(s))",
                "      cgwalters-forge/agent-board#7: actual $6.00 (1 run(s))",
                "    cgwalters-forge/tracker#11: actual $0.00 (0 run(s))",
                "  cgwalters-forge/tracker#12: actual $0.00 (0 run(s))",
            ]
        );
        // Each item's state and comments, and the epics; #11 read twice.
        assert_eq!(forge.reads.get(), 1 + 2 * 7);
        let tree = read(
            &forge,
            &config(json!({"budget": {"weeklyUsd": 1000},
            "usage": {"url": "http://broker", "repository": "o/r", "issue": 1}})),
        )
        .unwrap();
        assert!(lines(&tree)[0].starts_with("board: budget $1000.00 a week; "));
    }

    #[test]
    fn cost_lines_are_read_strictly() {
        for (line, expected) in [
            (
                "<!-- agentic-job-cost: 100 usd=1.2345 partial=false -->",
                Some((100, 1.2345, false)),
            ),
            (
                "<!-- agentic-job-cost: 100 usd=0 partial=true -->",
                Some((100, 0.0, true)),
            ),
            ("<!-- agentic-job-cost: 0100 usd=1 partial=true -->", None),
            ("<!-- agentic-job-cost: 100 usd=-1 partial=true -->", None),
            ("<!-- agentic-job-cost: 100 usd=1e3 partial=true -->", None),
            ("<!-- agentic-job-cost: 100 usd=1. partial=true -->", None),
            ("<!-- agentic-job-cost: 100 usd=NaN partial=true -->", None),
            ("<!-- agentic-job-cost: 100 usd=1 partial=maybe -->", None),
            ("<!-- agentic-job-cost: 100 usd=1 partial=true x -->", None),
            ("<!-- agentic-job-cost: 100 usd=1 -->", None),
            (" <!-- agentic-job-cost: 100 usd=1 partial=true -->", None),
        ] {
            let got = cost_line(line).map(|(run, c)| (run, c.usd, c.partial));
            assert_eq!(got, expected, "{line}");
        }
    }

    #[test]
    fn unreadable_trees_are_refused() {
        type Edit = fn(&mut Forge);
        let cases: [(Edit, &str); 4] = [
            (
                |f| {
                    f.issues.get_mut("cgwalters-forge/agent-board#6").unwrap()["budget"] = json!(-1)
                },
                "is not a dollar amount",
            ),
            (
                |f| {
                    f.issues.get_mut("cgwalters-forge/agent-board#6").unwrap()["state"] =
                        json!(null)
                },
                "has no state",
            ),
            (
                |f| {
                    f.issues.remove("cgwalters-forge/agent-board#7");
                },
                "no cgwalters-forge/agent-board#7",
            ),
            (
                |f| {
                    // A chain deeper than GitHub allows.
                    for n in 100..=100 + DEPTH_LIMIT as u64 {
                        let sub = format!("cgwalters-forge/tracker#{}", n + 1);
                        f.issues.insert(
                            format!("cgwalters-forge/tracker#{n}"),
                            issue("OPEN", Value::Null, &[&sub], json!([])),
                        );
                    }
                    f.issues.insert(
                        format!("cgwalters-forge/tracker#{}", 101 + DEPTH_LIMIT),
                        issue("OPEN", Value::Null, &[], json!([])),
                    );
                    f.epics = vec![100];
                },
                "levels below an epic",
            ),
        ];
        for (edit, expected) in cases {
            let mut forge = forge();
            edit(&mut forge);
            let error = read(&forge, &config(json!({}))).unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
        let mut value: Value = serde_json::from_str(CONFIG).unwrap();
        value.as_object_mut().unwrap().remove("epics");
        let error = read(&forge(), &Config::parse(&value.to_string()).unwrap()).unwrap_err();
        assert!(error.to_string().contains("epics.repository"), "{error}");
    }
}
