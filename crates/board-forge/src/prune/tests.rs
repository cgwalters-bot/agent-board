use super::*;
use crate::poll::RestResponse;
use std::cell::RefCell;

const FORGE: &str = include_str!("../../../../fixtures/prune/forge.json");
const NOW: &str = "2026-10-26T00:00:00Z";

/// The recorded forge: a GET answers 304 to the validator its entry names,
/// and nothing may be written.
struct Fake {
    forge: Value,
    /// Each GET's URL and the validator sent.
    gets: RefCell<Vec<(String, Option<String>)>>,
    sends: RefCell<Vec<String>>,
}

impl Fake {
    fn new() -> Self {
        Self {
            forge: serde_json::from_str(FORGE).unwrap(),
            gets: RefCell::default(),
            sends: RefCell::default(),
        }
    }
}

impl Transport for Fake {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        assert_eq!(variables["owner"], "example");
        if query == CLOSING_QUERY {
            let name = variables["name"].as_str().unwrap();
            let issue = format!("example/{name}#{}", variables["number"]);
            let nodes = self.forge["closing"][&issue].as_array().cloned();
            let nodes = nodes.with_context(|| format!("unexpected closing read of {issue}"))?;
            let refs = json!({"nodes": nodes, "totalCount": nodes.len()});
            return Ok(json!({"repository": {"issue": {"closedByPullRequestsReferences": refs}}}));
        }
        assert_eq!(query, BOARD_QUERY);
        assert_eq!(variables["field"], "Priority");
        Ok(self.forge["graphql"].clone())
    }
}

/// `projectV2` is a field of the owner's concrete types, not of the
/// `RepositoryOwner` interface `repositoryOwner` returns: GitHub refuses a
/// query that selects it there.
#[test]
fn the_board_is_selected_through_the_owner_types() {
    let selections: Vec<_> = (BOARD_QUERY.lines().map(str::trim))
        .filter(|l| l.contains("projectV2("))
        .collect();
    assert_eq!(
        selections,
        [
            "... on Organization { projectV2(number: $number) { ...Board } }",
            "... on User { projectV2(number: $number) { ...Board } }",
        ]
    );
    assert!(BOARD_QUERY.contains("fragment Board on ProjectV2 {"));
}

#[test]
fn an_unreadable_board_or_priority_field_is_refused() {
    for (pointer, value, error) in [
        (
            "/repositoryOwner/projectV2",
            Value::Null,
            "the board could not be read",
        ),
        (
            "/repositoryOwner/projectV2/field",
            Value::Null,
            "the board has no Priority single-select field",
        ),
        (
            "/repositoryOwner/projectV2/field",
            json!({}),
            "the board has no Priority single-select field",
        ),
    ] {
        let mut fake = Fake::new();
        *fake.forge["graphql"].pointer_mut(pointer).unwrap() = value;
        let found = run(&fake, Cache::default(), PRUNE_REQUEST_LIMIT).unwrap_err();
        assert_eq!(found.to_string(), error);
    }
}

impl Rest for Fake {
    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
        let entry = self.forge["rest"]
            .get(url)
            .with_context(|| format!("unexpected GET {url}"))?;
        let (etag, modified) = (entry["etag"].as_str(), entry["last_modified"].as_str());
        let fresh = match validator {
            Some(("If-None-Match", v)) => Some(v) != etag,
            Some(("If-Modified-Since", v)) => Some(v) != modified,
            _ => true,
        };
        self.gets
            .borrow_mut()
            .push((url.into(), validator.map(|(n, v)| format!("{n}: {v}"))));
        Ok(RestResponse {
            status: if fresh { 200 } else { 304 },
            etag: etag.map(str::to_owned),
            last_modified: modified.map(str::to_owned),
            link: entry["link"].as_str().map(str::to_owned),
            body: if fresh {
                entry["body"].clone()
            } else {
                Value::Null
            },
        })
    }

    fn send(&self, method: &str, url: &str, _body: &Value) -> Result<(u16, Value)> {
        self.sends.borrow_mut().push(format!("{method} {url}"));
        bail!("prune must not write")
    }
}

fn config() -> Config {
    Config::parse(
        &json!({
            "operator": "alice",
            "bot": "example-bot",
            "dispatchRepo": "example/tracker",
            "dispatchWorkflow": "dispatch.yml",
            "workRepositories": ["example/tracker", "example/app"],
            "board": {"owner": "example", "project": 1},
            "wip": 2
        })
        .to_string(),
    )
    .unwrap()
}

fn now() -> OffsetDateTime {
    OffsetDateTime::parse(NOW, &Rfc3339).unwrap()
}

fn run(fake: &Fake, cache: Cache, limit: usize) -> Result<(Report, Cache)> {
    read(fake, &config(), cache, (limit, DEFAULT_STALE_DAYS), now())
}

#[test]
fn each_reason_is_found_with_its_evidence() {
    let fake = Fake::new();
    let (report, _) = run(&fake, Cache::default(), PRUNE_REQUEST_LIMIT).unwrap();
    let issue = |repo: &str, n: u64| format!("https://github.com/example/{repo}/issues/{n}");
    // Each candidate, its reasons and their evidence. Left out: tracker#2
    // (P1), #3's stale (a pull request cross-referenced it 16 days ago), #5
    // (labelled Keep), #7 (open sub-issues), #8 (a person answered the done
    // verdict), #9 (a pull request), #10 (a verdict not the bot's), #11
    // (references from issues, an unmerged and a non-closing pull request,
    // and a merged closing one outside the work repositories), #12
    // (summaries not read), #13 (it blocks another), app#22 and app#23
    // (titles with no words). tracker#1 stays stale through the bot's
    // mention of a person.
    type Expected<'a> = (&'a str, u64, Vec<(Reason, Vec<String>)>);
    let expected: Vec<Expected> = vec![
        (
            "example/app",
            20,
            vec![(
                Reason::Stale,
                vec![format!("{}#issuecomment-2001", issue("app", 20))],
            )],
        ),
        (
            "example/app",
            21,
            vec![
                (Reason::Duplicate, vec![issue("app", 20)]),
                (Reason::Stale, vec![issue("app", 21)]),
            ],
        ),
        (
            "example/tracker",
            1,
            vec![(Reason::Stale, vec![issue("tracker", 1)])],
        ),
        (
            "example/tracker",
            3,
            vec![(
                Reason::Overtaken,
                vec!["https://github.com/example/app/pull/40".into()],
            )],
        ),
        (
            "example/tracker",
            4,
            vec![(Reason::Duplicate, vec![issue("tracker", 1)])],
        ),
        (
            "example/tracker",
            6,
            vec![(
                Reason::TriageDone,
                vec![format!("{}#issuecomment-602", issue("tracker", 6))],
            )],
        ),
    ];
    let found: Vec<_> = (report.candidates.iter())
        .map(|c| {
            let reasons = c.reasons.iter();
            let reasons = reasons.map(|f| (f.reason, f.evidence.clone())).collect();
            (c.repository.as_str(), c.number, reasons)
        })
        .collect();
    assert_eq!(found, expected);
    assert_eq!((report.open, report.kept), (16, 1));
    // The board, three list pages, the timelines of the 15 issues not kept
    // and the closing references of the two a merged pull request referenced.
    assert_eq!((report.requests, report.not_modified), (21, 0));
    let stale = &report.candidates[0];
    assert_eq!(stale.priority.as_deref(), Some("P2"));
    assert_eq!(
        stale.reasons[0].detail,
        "P2 with no activity by a person or a pull request for 46 days"
    );
    assert_eq!(
        report.candidates[1].reasons[0].detail,
        "same title as the older #20"
    );
    assert!(fake.sends.borrow().is_empty());
    let text = report.text();
    assert!(text.starts_with(
        "prune: 6 candidates among 16 open issues (1 kept) in 2 repositories; nothing written\n"
    ));
    assert!(text.contains(
        "\nhttps://github.com/example/tracker/issues/6 Refactor the loader\n  triage-done: "
    ));
}

#[test]
fn a_second_read_revalidates_every_page_from_the_cache() {
    let fake = Fake::new();
    let (first, cache) = run(&fake, Cache::default(), PRUNE_REQUEST_LIMIT).unwrap();
    assert!(fake.gets.borrow().iter().all(|(_, v)| v.is_none()));
    // Kept bodies are slimmed: a comment without a verdict keeps no body.
    let timeline =
        &cache.0["https://api.github.com/repos/example/tracker/issues/1/timeline?per_page=100"];
    assert_eq!(timeline["body"][0].get("body"), None);
    let cache = Cache::from_json(&cache.to_json()).unwrap();
    fake.gets.borrow_mut().clear();
    let (second, again) = run(&fake, cache, PRUNE_REQUEST_LIMIT).unwrap();
    assert_eq!(second.candidates, first.candidates);
    // GraphQL, the board and two closing reads, cannot be revalidated.
    assert_eq!(second.not_modified, second.requests - 3);
    let gets = fake.gets.borrow();
    assert!(gets.iter().all(|(_, v)| v.is_some()), "{gets:?}");
    assert!(
        gets.iter()
            .any(|(_, v)| v.as_deref() == Some("If-Modified-Since: Sun, 25 Oct 2026 00:00:00 GMT"))
    );
    assert_eq!(again.0.len(), gets.len());
}

#[test]
fn reads_are_bounded_and_refused_whole() {
    for limit in [0, 1, 16] {
        let error = run(&Fake::new(), Cache::default(), limit).unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            format!("prune request budget ({limit}) exhausted; nothing was listed")
        );
    }
    let error = read(&Fake::new(), &config(), Cache::default(), (10, 0), now()).unwrap_err();
    assert_eq!(error.to_string(), "stale days must be 1 to 365");
}

#[test]
fn the_cache_is_checked() {
    let entry = json!({"etag": "x", "body": []});
    for (cache, error) in [
        (json!([]), "prune cache is not an object"),
        (
            json!({"schema": "agent-board/poll-state/v1", "responses": {}}),
            "unsupported prune cache schema",
        ),
        (
            json!({"schema": PRUNE_CACHE_SCHEMA, "responses": {}, "extra": 1}),
            "unknown prune cache field extra",
        ),
        (
            json!({"schema": PRUNE_CACHE_SCHEMA, "responses": {"https://example.com/": entry}}),
            "prune cache URL outside the API",
        ),
        (
            json!({"schema": PRUNE_CACHE_SCHEMA, "responses": {"https://api.github.com/x": {}}}),
            "prune cache response has no body",
        ),
    ] {
        let found = Cache::from_json(&cache).unwrap_err().to_string();
        assert!(found.starts_with(error), "{found}");
    }
}

#[test]
fn title_stems() {
    for (title, stem) in [
        ("Fix the widget", "fix the widget"),
        ("fix  the widget!", "fix the widget"),
        ("App: crash", "app crash"),
        ("poll: Retry 503s", "poll retry 503s"),
        ("", ""),
        ("🚀", ""),
    ] {
        assert_eq!(title_stem(title), stem, "{title:?}");
    }
}
