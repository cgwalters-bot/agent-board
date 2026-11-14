use super::*;
use crate::pass::Replay;
use std::cell::RefCell;

const OPERATOR: &str = "owner";
const REPO: &str = "example/project";
const SINCE: &str = "2026-11-11T14:00:00Z";
const BEFORE: &str = "2026-11-11T13:00:00Z";
const IN: &str = "2026-11-11T15:00:00Z";

/// Recorded 2026-11-11 with `fixtures/changes/config.json`, every
/// repository's watermark at 14:05 UTC, then again after a commit: the
/// REST bodies projected to the fields the diff reads.
/// Query keys were refreshed for review-dismissal selection; these historical
/// answers do not exercise dismissals (covered separately below).
const FIRST: &str = include_str!("../../../../../fixtures/changes/agentic-job-2026-11-11.json");
const AGAIN: &str =
    include_str!("../../../../../fixtures/changes/agentic-job-2026-11-11-again.json");
/// The watermarks the first recording was read from.
const STATE: &str = include_str!("../../../../../fixtures/changes/state.json");
/// The fixture configuration's follow list and midstream owner and topic.
const FOLLOW: &str = "cgwalters-forge/agentic-job";
const MIDSTREAM_OWNER: &str = "cgwalters-forge";
const MIDSTREAM_TOPIC: &str = "bot-midstream";

fn replay(text: &str) -> Replay {
    Replay::from_json(&serde_json::from_str(text).unwrap()).unwrap()
}

fn midstream() -> Midstream {
    serde_json::from_value(json!({"owner": MIDSTREAM_OWNER, "topic": MIDSTREAM_TOPIC})).unwrap()
}

/// Every repository's watermark at `since`.
fn state_at(scope: &Scope, since: &str) -> State {
    let mut state = State::default();
    for repository in &scope.repositories {
        state.repositories.insert(
            repository.name.to_ascii_lowercase(),
            Watermark {
                since: Some(since.to_owned()),
                ..Watermark::default()
            },
        );
    }
    state
}

#[test]
fn recorded_read_lists_the_changes_and_reads_only_the_lists_again() {
    let first = replay(FIRST);
    let repositories = discover(&first, &[FOLLOW.to_owned()], Some(&midstream())).unwrap();
    // The followed repository first, then the forks with the topic or the
    // origin property, sorted; `.github` and the forks without either are not.
    assert_eq!(repositories.len(), 24, "{repositories:?}");
    assert_eq!(
        repositories[0],
        Followed {
            name: FOLLOW.to_owned(),
            midstream: false
        }
    );
    assert!(
        repositories[1..]
            .iter()
            .all(|r| r.midstream && r.name.starts_with("cgwalters-forge/")),
        "{repositories:?}"
    );
    let names: Vec<_> = repositories.iter().map(|r| r.name.as_str()).collect();
    for fork in [
        "cgwalters-forge/bootc",
        "cgwalters-forge/ostree",
        "cgwalters-forge/gh-aw",
    ] {
        assert!(names.contains(&fork), "{names:?}");
    }
    assert!(
        !names.contains(&"cgwalters-forge/ai-workflows"),
        "{names:?}"
    );
    let scope = Scope {
        repositories,
        operator: "cgwalters".to_owned(),
    };
    let mut state = State::from_json(&serde_json::from_str(STATE).unwrap()).unwrap();
    assert_eq!(state, state_at(&scope, "2026-11-11T14:05:00Z"));
    let diff = read(&first, &scope, &mut state, first.now).unwrap();
    assert_eq!((diff.requests, diff.revalidated), (28, 0));
    assert_eq!(diff.repositories.len(), 24);
    let changed: Vec<_> = diff
        .repositories
        .iter()
        .filter(|r| !r.items.is_empty())
        .map(|r| (r.repository.as_str(), r.midstream, r.items.len()))
        .collect();
    assert_eq!(
        changed,
        [
            (FOLLOW, false, 40),
            ("cgwalters-forge/bootc", true, 1),
            ("cgwalters-forge/composefs-rs", true, 2)
        ]
    );
    let items: BTreeMap<_, _> = diff.repositories[0]
        .items
        .iter()
        .map(|i| (i.reference.as_str(), i))
        .collect();
    for number in 3406..=3410 {
        let item = items[format!("{FOLLOW}#{number}").as_str()];
        assert!(item.changes.contains(&Change::Opened), "{item:?}");
        assert_eq!((item.kind, item.state.as_str()), (Kind::Issue, "open"));
    }
    for number in [3402, 3411] {
        let item = items[format!("{FOLLOW}#{number}").as_str()];
        assert!(item.changes.contains(&Change::Closed), "{item:?}");
        assert!(item.closed.is_some());
    }
    let numbers: Vec<u64> = diff.repositories[0]
        .items
        .iter()
        .filter_map(|i| i.reference.rsplit_once('#')?.1.parse().ok())
        .collect();
    assert!(numbers.windows(2).all(|w| w[0] < w[1]), "{numbers:?}");
    let summary = diff.summary();
    assert!(summary.lines().count() <= SUMMARY_LINES);
    assert!(
        summary.starts_with(
            "changes: 43 items in 3 of 24 repositories; 28 requests, 0 revalidated\ncgwalters-forge/agentic-job: 40 items; opened 33, "
        ),
        "{summary}"
    );
    // The JSON round-trips through the schema's fields.
    let value = serde_json::to_value(&diff).unwrap();
    assert_eq!(value["schema"], SCHEMA);
    assert_eq!(
        value.as_object().unwrap().keys().collect::<Vec<_>>(),
        FIELDS.to_vec()
    );
    // Nothing moved without a commit; a commit moves every watermark to
    // the read's time, and the next read, with nothing changed, lists
    // every repository once and reads no detail.
    assert!(
        state
            .repositories
            .values()
            .all(|w| w.since.as_deref() == Some("2026-11-11T14:05:00Z"))
    );
    diff.commit(&mut state);
    assert!(
        state
            .repositories
            .values()
            .all(|w| w.since.as_deref() == Some("2026-11-11T21:48:00Z"))
    );
    let again = replay(AGAIN);
    let diff = read(&again, &scope, &mut state, again.now).unwrap();
    assert_eq!(diff.requests, scope.repositories.len());
    assert!(diff.repositories.iter().all(|r| r.items.is_empty()));
    assert!(
        diff.repositories
            .iter()
            .all(|r| r.since == "2026-11-11T21:43:00Z")
    );
    assert_eq!(
        diff.summary(),
        "changes: 0 items in 0 of 24 repositories; 24 requests, 0 revalidated\n"
    );
}

/// Answers from fixed GET bodies, 304 when the validator names the body's
/// ETag, and GraphQL detail from a queue.
#[derive(Default)]
struct Fake {
    gets: BTreeMap<String, Value>,
    graphql: RefCell<Vec<Value>>,
    variables: RefCell<Vec<Value>>,
}

impl Rest for Fake {
    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
        let answer = self
            .gets
            .get(url)
            .with_context(|| format!("unexpected GET {url}"))?;
        let etag = answer["etag"].as_str().map(str::to_owned);
        if validator
            .is_some_and(|(name, value)| name == "If-None-Match" && Some(value) == etag.as_deref())
        {
            return Ok(RestResponse {
                status: 304,
                etag: None,
                last_modified: None,
                link: None,
                body: Value::Null,
            });
        }
        Ok(RestResponse {
            status: answer["status"].as_u64().unwrap_or(200) as u16,
            etag,
            last_modified: None,
            link: answer["link"].as_str().map(str::to_owned),
            body: answer["body"].clone(),
        })
    }

    fn send(&self, method: &str, url: &str, _: &Value) -> Result<(u16, Value)> {
        bail!("unexpected {method} {url}")
    }
}

impl Transport for Fake {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        assert!(query.starts_with("query($owner:String!,$name:String!,$since:DateTime!)"));
        assert!(query.contains("PULL_REQUEST_REVIEW,REVIEW_DISMISSED_EVENT,"));
        assert!(query.contains("... on ReviewDismissedEvent{"));
        self.variables.borrow_mut().push(variables);
        let mut queue = self.graphql.borrow_mut();
        ensure!(!queue.is_empty(), "unexpected GraphQL request");
        Ok(queue.remove(0))
    }
}

fn list_url(since: &str) -> String {
    format!(
        "{API_ROOT}repos/{REPO}/issues?state=all&sort=updated&direction=asc&since={since}&per_page=100"
    )
}

fn listing(numbers: &[u64]) -> Value {
    json!(
        numbers
            .iter()
            .map(|n| json!({"number": n}))
            .collect::<Vec<_>>()
    )
}

/// An item as the detail query reads it: open, created before the window,
/// with no events unless `fields` says otherwise.
fn node(kind: &str, number: u64, fields: Value) -> Value {
    let path = if kind == "Issue" { "issues" } else { "pull" };
    let mut node = json!({"__typename": kind, "number": number, "title": format!("Item {number}"),
        "url": format!("https://github.com/{REPO}/{path}/{number}"), "state": "OPEN",
        "createdAt": BEFORE, "updatedAt": IN, "closedAt": null, "lastEditedAt": null,
        "author": {"login": "someone"},
        "timelineItems": {"nodes": [], "pageInfo": {"hasNextPage": false}}});
    if kind == "PullRequest" {
        node["merged"] = json!(false);
        node["mergedAt"] = Value::Null;
        node["isDraft"] = json!(false);
        node["commits"] = json!({"nodes": [{"commit": {"abbreviatedOid": "abc1234", "statusCheckRollup": null}}]});
    }
    for (key, value) in fields.as_object().unwrap() {
        node[key] = value.clone();
    }
    node
}

fn events(events: Vec<Value>) -> Value {
    json!({"nodes": events, "pageInfo": {"hasNextPage": false}})
}

fn scope() -> Scope {
    Scope {
        repositories: vec![Followed {
            name: REPO.to_owned(),
            midstream: false,
        }],
        operator: OPERATOR.to_owned(),
    }
}

fn state_since(since: &str) -> State {
    state_at(&scope(), since)
}

/// What a classified item is checked for, beyond its changes.
type Check = Box<dyn Fn(&Item)>;

#[test]
fn review_dismissals_are_changes_not_new_approvals() {
    let review_url = "https://github.com/example/project/pull/2#pullrequestreview-1";
    let event_url = "https://github.com/example/project/pull/2#event-2";
    for (actor, reviewer, operator) in [
        (Some("Owner"), Some("someone"), true),
        (Some("someone"), Some(OPERATOR), false),
        (None, None, false),
    ] {
        let review = reviewer.map(|login| json!({"url": review_url, "author": {"login": login}}));
        let dismissal = json!({"__typename": "ReviewDismissedEvent", "createdAt": IN,
            "actor": actor.map(|login| json!({"login": login})), "url": event_url,
            "previousReviewState": "APPROVED", "review": review});
        let fake = Fake {
            gets: BTreeMap::from([(list_url(SINCE), json!({"body": listing(&[2])}))]),
            graphql: RefCell::new(vec![json!({"repository": {"i0": node("PullRequest", 2,
                json!({"timelineItems": events(vec![dismissal])}))}})]),
            ..Fake::default()
        };
        let now = parse_time(IN).unwrap();
        let mut state = state_since("2026-11-11T14:05:00Z");
        let diff = read(&fake, &scope(), &mut state, now).unwrap();
        assert_eq!(diff.requests, 2);
        let item = &diff.repositories[0].items[0];
        assert_eq!(item.changes, BTreeSet::from([Change::Reviewed]));
        assert!(item.reviews.is_empty());
        assert_eq!(
            item.review_dismissals,
            [ReviewDismissal {
                by: actor.unwrap_or_default().into(),
                at: IN.into(),
                url: event_url.into(),
                previous_state: "APPROVED".into(),
                review_url: reviewer.map(|_| review_url.into()),
                reviewer: reviewer.map(str::to_owned),
                operator,
            }]
        );
        let summary = diff.summary();
        assert!(summary.contains("reviewed 1"), "{summary}");
        assert_eq!(summary.contains("operator 1"), operator, "{summary}");
        let value = serde_json::to_value(item).unwrap();
        assert!(value.get("reviews").is_none());
        let encoded = &value["review_dismissals"][0];
        assert_eq!(encoded["previous_state"], "APPROVED");
        assert_eq!(encoded.get("review_url").is_some(), reviewer.is_some());
        assert_eq!(encoded.get("reviewer").is_some(), reviewer.is_some());
        assert_eq!(encoded.get("operator").is_some(), operator);
    }
}

#[test]
fn classify_table() {
    let actor = json!({"login": OPERATOR});
    let cases: Vec<(&str, Value, Vec<Change>, Check)> = vec![
        (
            "opened and edited by their times",
            node("Issue", 1, json!({"createdAt": IN, "lastEditedAt": IN})),
            vec![Change::Opened, Change::Edited],
            Box::new(|i| {
                assert_eq!(
                    (i.opened.as_deref(), i.edited.as_deref()),
                    (Some(IN), Some(IN))
                )
            }),
        ),
        (
            "times before the window are not changes",
            node(
                "Issue",
                1,
                json!({"createdAt": BEFORE, "lastEditedAt": BEFORE}),
            ),
            vec![],
            Box::new(|i| assert!(i.opened.is_none() && i.edited.is_none())),
        ),
        (
            "closed, reopened, renamed and labelled",
            node(
                "Issue",
                1,
                json!({"state": "CLOSED", "timelineItems": events(vec![
                    json!({"__typename": "ClosedEvent", "createdAt": IN, "actor": actor}),
                    json!({"__typename": "ReopenedEvent", "createdAt": IN, "actor": actor}),
                    json!({"__typename": "RenamedTitleEvent", "createdAt": IN, "actor": actor}),
                    json!({"__typename": "LabeledEvent", "createdAt": IN, "actor": actor, "label": {"name": "needs-human"}}),
                    json!({"__typename": "UnlabeledEvent", "createdAt": IN, "actor": actor, "label": {"name": "bug"}}),
                ])}),
            ),
            vec![
                Change::Closed,
                Change::Reopened,
                Change::Edited,
                Change::Labelled,
            ],
            Box::new(|i| {
                assert_eq!(i.state, "closed");
                assert_eq!(
                    i.labels,
                    [("needs-human", true), ("bug", false)].map(|(name, added)| LabelChange {
                        name: name.to_owned(),
                        added,
                        at: IN.to_owned(),
                        by: OPERATOR.to_owned()
                    })
                );
            }),
        ),
        (
            "comments flag the operator's, whatever its case",
            node(
                "Issue",
                1,
                json!({"timelineItems": events(vec![
                    json!({"__typename": "IssueComment", "createdAt": IN, "author": {"login": "Owner"}, "url": "https://github.com/example/project/issues/1#issuecomment-1"}),
                    json!({"__typename": "IssueComment", "createdAt": IN, "author": {"login": "bot"}, "url": "https://github.com/example/project/issues/1#issuecomment-2"}),
                ])}),
            ),
            vec![Change::Commented],
            Box::new(|i| {
                assert_eq!(
                    i.comments.iter().map(|c| c.operator).collect::<Vec<_>>(),
                    [true, false]
                )
            }),
        ),
        (
            "links and project changes",
            node(
                "Issue",
                1,
                json!({"timelineItems": events(vec![
                    json!({"__typename": "SubIssueAddedEvent", "createdAt": IN, "subIssue": {"url": "https://github.com/example/project/issues/2"}}),
                    json!({"__typename": "ParentIssueRemovedEvent", "createdAt": IN, "parent": {"url": "https://github.com/example/project/issues/3"}}),
                    json!({"__typename": "AddedToProjectV2Event", "createdAt": IN}),
                    json!({"__typename": "ProjectV2ItemStatusChangedEvent", "createdAt": IN, "previousStatus": "Todo", "status": "In progress"}),
                ])}),
            ),
            vec![Change::Linked, Change::Project],
            Box::new(|i| {
                assert_eq!(
                    i.links
                        .iter()
                        .map(|l| (l.kind, l.added))
                        .collect::<Vec<_>>(),
                    [("sub-issue", true), ("parent", false)]
                );
                assert_eq!(
                    i.project
                        .iter()
                        .map(|p| (p.kind, p.from.as_deref(), p.to.as_deref()))
                        .collect::<Vec<_>>(),
                    [
                        ("added", None, None),
                        ("status", Some("Todo"), Some("In progress"))
                    ]
                );
            }),
        ),
        (
            "a pull request's reviews, pushes, head checks and draft state",
            node(
                "PullRequest",
                2,
                json!({
                "commits": {"nodes": [{"commit": {"abbreviatedOid": "def5678", "statusCheckRollup": {"state": "FAILURE"}}}]},
                "timelineItems": events(vec![
                    json!({"__typename": "PullRequestReview", "submittedAt": IN, "author": actor, "state": "APPROVED", "url": "https://github.com/example/project/pull/2#pullrequestreview-1"}),
                    json!({"__typename": "PullRequestCommit", "commit": {"abbreviatedOid": "def5678", "committedDate": IN}}),
                    json!({"__typename": "HeadRefForcePushedEvent", "createdAt": IN, "actor": actor, "afterCommit": {"abbreviatedOid": "def5678"}}),
                    json!({"__typename": "ConvertToDraftEvent", "createdAt": IN}),
                ])}),
            ),
            vec![Change::Reviewed, Change::Pushed, Change::Draft],
            Box::new(|i| {
                assert_eq!(
                    (i.kind, i.checks.as_deref(), i.draft),
                    (Kind::PullRequest, Some("FAILURE"), Some(true))
                );
                assert!(i.reviews[0].operator && i.reviews[0].state == "APPROVED");
                assert_eq!(
                    i.pushes
                        .iter()
                        .map(|p| (p.commit.as_str(), p.forced))
                        .collect::<Vec<_>>(),
                    [("def5678", false), ("def5678", true)]
                );
            }),
        ),
        (
            "a merge by its event, and a closed pull request reports no checks",
            node(
                "PullRequest",
                2,
                json!({"state": "MERGED", "merged": true, "mergedAt": IN,
                "commits": {"nodes": [{"commit": {"abbreviatedOid": "def5678", "statusCheckRollup": {"state": "SUCCESS"}}}]},
                "timelineItems": events(vec![
                    json!({"__typename": "MergedEvent", "createdAt": IN, "actor": actor}),
                    json!({"__typename": "ClosedEvent", "createdAt": IN, "actor": actor}),
                ])}),
            ),
            vec![Change::Merged, Change::Closed],
            Box::new(|i| assert!(i.checks.is_none() && i.merged.as_deref() == Some(IN))),
        ),
        (
            "a merge by its time alone",
            node(
                "PullRequest",
                2,
                json!({"state": "MERGED", "merged": true, "mergedAt": IN}),
            ),
            vec![Change::Merged],
            Box::new(|_| ()),
        ),
    ];
    for (name, node, changes, check) in cases {
        let item =
            classify(&node, REPO, SINCE, OPERATOR).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert_eq!(item.changes, changes.into_iter().collect(), "{name}");
        check(&item);
    }
    for (name, node, error) in [
        (
            "a second page of events",
            node(
                "Issue",
                1,
                json!({"timelineItems": {"nodes": [], "pageInfo": {"hasNextPage": true}}}),
            ),
            "more than one page of timeline events",
        ),
        (
            "an event the query did not ask for",
            node(
                "Issue",
                1,
                json!({"timelineItems": events(vec![json!({"__typename": "AssignedEvent", "createdAt": IN})])}),
            ),
            "unexpected timeline event AssignedEvent",
        ),
        (
            "not an item",
            json!({"__typename": "Repository"}),
            "not an issue or pull request",
        ),
        (
            "a time that is not one",
            node("Issue", 1, json!({"createdAt": 5})),
            "invalid GitHub createdAt",
        ),
    ] {
        let text = format!("{:#}", classify(&node, REPO, SINCE, OPERATOR).unwrap_err());
        assert!(text.contains(error), "{name}: {text}");
    }
}

#[test]
fn read_follows_running_checks_and_reads_nothing_once_they_settle() {
    let url = list_url("2026-11-11T13:55:00Z");
    let mut fake = Fake::default();
    fake.gets.insert(
        url.clone(),
        json!({"status": 200, "etag": "W/\"first\"", "link": null, "body": listing(&[1, 2])}),
    );
    let running = json!({"nodes": [{"commit": {"abbreviatedOid": "abc1234", "statusCheckRollup": {"state": "PENDING"}}}]});
    let settled = json!({"nodes": [{"commit": {"abbreviatedOid": "abc1234", "statusCheckRollup": {"state": "SUCCESS"}}}]});
    let pushed = events(vec![
        json!({"__typename": "PullRequestCommit", "commit": {"abbreviatedOid": "abc1234", "committedDate": IN}}),
    ]);
    fake.graphql.borrow_mut().push(json!({"repository": {
        "i0": node("Issue", 1, json!({})),
        "i1": node("PullRequest", 2, json!({"commits": running, "timelineItems": pushed})),
    }}));
    let mut state = state_since(SINCE);
    let now = parse_time("2026-11-11T16:00:00Z").unwrap();
    let diff = read(&fake, &scope(), &mut state, now).unwrap();
    // The listed issue nothing explains is `updated`; the pushed pull request's
    // running checks are followed.
    let items = &diff.repositories[0].items;
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].changes, [Change::Updated].into());
    assert_eq!(items[1].changes, [Change::Pushed].into());
    assert_eq!(items[1].checks.as_deref(), Some("PENDING"));
    assert_eq!(state.pending, ["example/project#2".to_owned()].into());
    assert_eq!(
        (
            state.repositories[REPO].etag.as_deref(),
            state.repositories[REPO].numbers.as_slice()
        ),
        (Some("W/\"first\""), &[1, 2][..])
    );
    assert_eq!(
        fake.variables.borrow()[0],
        json!({"owner": "example", "name": "project", "since": "2026-11-11T13:55:00Z"})
    );
    // Not committed: the list is revalidated for nothing and its numbers
    // read in detail again, so the same changes are listed, the settled
    // checks now one of them.
    fake.graphql.borrow_mut().push(json!({"repository": {
        "i0": node("Issue", 1, json!({})),
        "i1": node("PullRequest", 2, json!({"commits": settled, "timelineItems": pushed})),
    }}));
    let diff = read(&fake, &scope(), &mut state, now).unwrap();
    assert_eq!((diff.requests, diff.revalidated), (2, 1));
    let items = &diff.repositories[0].items;
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].changes, [Change::Updated].into());
    assert_eq!(items[1].changes, [Change::Pushed, Change::Checked].into());
    assert_eq!(items[1].checks.as_deref(), Some("SUCCESS"));
    assert_eq!(state.pending, [format!("{REPO}#2")].into());
    // Committed: the new list is empty, and nothing is pending, so nothing
    // is read in detail; revalidated for nothing after that.
    diff.commit(&mut state);
    assert!(state.pending.is_empty());
    fake.gets.insert(
        list_url("2026-11-11T15:55:00Z"),
        json!({"status": 200, "etag": "W/\"empty\"", "link": null, "body": listing(&[])}),
    );
    for revalidated in [0, 1] {
        let diff = read(&fake, &scope(), &mut state, now).unwrap();
        assert_eq!((diff.requests, diff.revalidated), (1, revalidated));
        assert!(diff.repositories[0].items.is_empty());
    }
    assert_eq!(
        read(&fake, &scope(), &mut state, now).unwrap().summary(),
        "changes: 0 items in 0 of 1 repositories; 1 requests, 1 revalidated\n"
    );
    // A still-running head or absent rollup stays pending without being a
    // change. Closing the PR releases its slot even without a check result.
    for (fields, left) in [
        (json!({"commits": running}), 1),
        (json!({}), 1),
        (json!({"state": "CLOSED"}), 0),
    ] {
        fake.graphql
            .borrow_mut()
            .push(json!({"repository": {"i0": node("PullRequest", 2, fields)}}));
        state.pending.insert("example/project#2".to_owned());
        let diff = read(&fake, &scope(), &mut state, now).unwrap();
        assert!(diff.repositories[0].items.is_empty());
        assert_eq!(state.pending.len(), left);
    }
}

#[test]
fn late_check_registration_survives_commit_and_an_empty_update_list() {
    for result in ["SUCCESS", "FAILURE"] {
        let now = parse_time("2026-11-11T16:00:00Z").unwrap();
        let mut fake = Fake::default();
        fake.gets.insert(
            list_url("2026-11-11T13:55:00Z"),
            json!({"body": listing(&[2])}),
        );
        fake.graphql.borrow_mut().push(json!({"repository": {
            "i0": node("PullRequest", 2, json!({"timelineItems": events(vec![
                json!({"__typename": "PullRequestCommit", "commit": {
                    "abbreviatedOid": "abc1234", "committedDate": IN
                }})
            ])}))
        }}));
        let mut state = state_since(SINCE);
        let diff = read(&fake, &scope(), &mut state, now).unwrap();
        let item = &diff.repositories[0].items[0];
        assert_eq!(item.changes, [Change::Pushed].into());
        assert!(item.checks.is_none());
        assert_eq!(state.pending, [format!("{REPO}#2")].into());
        diff.commit(&mut state);
        state = State::from_json(&state.to_json().unwrap()).unwrap();
        assert_eq!(state.pending, [format!("{REPO}#2")].into());

        fake.gets.insert(
            list_url("2026-11-11T15:55:00Z"),
            json!({"etag": "empty", "body": []}),
        );
        // CI has not registered yet. The PR is no longer listed, but remains
        // discoverable; committing a quiet read must not forget it.
        fake.graphql.borrow_mut().push(json!({"repository": {
            "i0": node("PullRequest", 2, json!({"updatedAt": BEFORE}))
        }}));
        let diff = read(&fake, &scope(), &mut state, now).unwrap();
        assert_eq!(diff.requests, 2);
        assert!(diff.repositories[0].items.is_empty());
        diff.commit(&mut state);
        assert_eq!(state.pending, [format!("{REPO}#2")].into());

        // The first registered checks can already be settled. They do not
        // bump updatedAt, and remain a change on repeated uncommitted reads.
        for commit in [false, true] {
            fake.graphql.borrow_mut().push(json!({"repository": {
                "i0": node("PullRequest", 2, json!({"updatedAt": BEFORE,
                    "commits": {"nodes": [{"commit": {
                        "statusCheckRollup": {"state": result}
                    }}]}}))
            }}));
            let diff = read(&fake, &scope(), &mut state, now).unwrap();
            assert_eq!((diff.requests, diff.revalidated), (2, 1));
            let item = &diff.repositories[0].items[0];
            assert_eq!(item.changes, [Change::Checked].into());
            assert_eq!(item.checks.as_deref(), Some(result));
            assert_eq!(state.pending, [format!("{REPO}#2")].into());
            if commit {
                diff.commit(&mut state);
            }
        }
        // Acknowledgement releases the slot and returns to list-only reads.
        assert!(state.pending.is_empty());
        let diff = read(&fake, &scope(), &mut state, now).unwrap();
        assert_eq!((diff.requests, diff.revalidated), (1, 1));
        assert!(diff.repositories[0].items.is_empty());
    }
}

#[test]
fn absent_checks_share_the_existing_pending_bound() {
    let now = parse_time("2026-11-11T16:00:00Z").unwrap();
    for (kind, count, enrolled) in [
        ("PullRequest", PENDING_LIMIT - 1, true),
        ("PullRequest", PENDING_LIMIT, false),
        ("Issue", PENDING_LIMIT - 1, false),
    ] {
        let fake = Fake {
            gets: BTreeMap::from([(
                list_url("2026-11-11T13:55:00Z"),
                json!({"body": listing(&[2])}),
            )]),
            ..Fake::default()
        };
        let mut state = state_since(SINCE);
        let numbers: Vec<_> = std::iter::once(2).chain(10..10 + count as u64).collect();
        state.pending = numbers[1..].iter().map(|n| format!("{REPO}#{n}")).collect();
        for batch in numbers.chunks(DETAIL_BATCH) {
            let nodes: serde_json::Map<_, _> = batch
                .iter()
                .enumerate()
                .map(|(k, n)| {
                    let value = if *n == 2 {
                        node(kind, *n, json!({}))
                    } else {
                        node(
                            "PullRequest",
                            *n,
                            json!({"commits": {"nodes": [{"commit": {
                                "statusCheckRollup": {"state": "PENDING"}
                            }}]}}),
                        )
                    };
                    (format!("i{k}"), value)
                })
                .collect();
            fake.graphql.borrow_mut().push(json!({"repository": nodes}));
        }
        let diff = read(&fake, &scope(), &mut state, now).unwrap();
        assert_eq!(diff.requests, 1 + numbers.len().div_ceil(DETAIL_BATCH));
        assert_eq!(state.pending.contains(&format!("{REPO}#2")), enrolled);
        assert_eq!(state.pending.len(), count + usize::from(enrolled));
    }
}

#[test]
fn pending_only_results_are_replayed_until_commit() {
    let now = parse_time("2026-11-11T16:00:00Z").unwrap();
    let mut fake = Fake::default();
    fake.gets.insert(
        list_url("2026-11-11T13:55:00Z"),
        json!({"etag": "empty", "body": []}),
    );
    let mut state = state_since(SINCE);
    state.pending.insert(format!("{REPO}#2"));
    for revalidated in [0, 1] {
        fake.graphql.borrow_mut().push(json!({"repository": {
            "i0": node("PullRequest", 2, json!({"updatedAt": BEFORE,
                "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "SUCCESS"}}}]}}))
        }}));
        let diff = read(&fake, &scope(), &mut state, now).unwrap();
        assert_eq!((diff.requests, diff.revalidated), (2, revalidated));
        assert_eq!(
            diff.repositories[0].items[0].changes,
            [Change::Checked].into()
        );
        assert_eq!(state.pending, [format!("{REPO}#2")].into());
        // Persist and reload, as separate CLI invocations do.
        state = State::from_json(&state.to_json().unwrap()).unwrap();
        if revalidated == 1 {
            diff.commit(&mut state);
        }
    }
    assert!(state.pending.is_empty());
    fake.gets
        .insert(list_url("2026-11-11T15:55:00Z"), json!({"body": []}));
    assert!(
        read(&fake, &scope(), &mut state, now).unwrap().repositories[0]
            .items
            .is_empty()
    );
}

#[test]
fn out_of_scope_pending_entries_do_not_use_the_cap() {
    let now = parse_time("2026-11-11T16:00:00Z").unwrap();
    let mut fake = Fake::default();
    fake.gets.insert(
        list_url("2026-11-11T13:55:00Z"),
        json!({"body": listing(&[2])}),
    );
    fake.graphql.borrow_mut().push(json!({"repository": {
        "i0": node("PullRequest", 2, json!({"commits": {"nodes": [{"commit": {
            "statusCheckRollup": {"state": "PENDING"}
        }}]}}))
    }}));
    let mut state = state_since(SINCE);
    state.pending = (1..=PENDING_LIMIT)
        .map(|n| format!("removed/repo#{n}"))
        .collect();
    let mut scope = scope();
    scope.repositories[0].name = REPO.to_ascii_uppercase();
    // Scope membership is case-insensitive; use the same spelling for the request.
    let answer = fake.gets.remove(&list_url("2026-11-11T13:55:00Z")).unwrap();
    fake.gets.insert(
        list_url("2026-11-11T13:55:00Z").replace(REPO, &REPO.to_ascii_uppercase()),
        answer,
    );
    read(&fake, &scope, &mut state, now).unwrap();
    assert!(state.pending.contains(&format!("{REPO}#2")));
    assert_eq!(state.pending.len(), PENDING_LIMIT + 1);
    assert!(state.pending.contains("removed/repo#1"));
}

#[test]
fn a_longer_list_is_read_whole_and_never_revalidated() {
    let url = list_url("2026-11-11T13:55:00Z");
    let second = format!("{url}&page=2");
    let mut fake = Fake::default();
    fake.gets.insert(
        url.clone(),
        json!({"status": 200, "etag": "W/\"first\"", "link": format!("<{second}>; rel=\"next\""), "body": listing(&[1])}),
    );
    fake.gets.insert(
        second,
        json!({"status": 200, "etag": "W/\"second\"", "link": null, "body": listing(&[2])}),
    );
    let now = parse_time("2026-11-11T16:00:00Z").unwrap();
    let mut state = state_since(SINCE);
    for _ in 0..2 {
        fake.graphql.borrow_mut().push(json!({"repository": {
            "i0": node("Issue", 1, json!({"createdAt": IN})),
            "i1": node("Issue", 2, json!({"createdAt": IN})),
        }}));
        let diff = read(&fake, &scope(), &mut state, now).unwrap();
        assert_eq!((diff.requests, diff.revalidated), (3, 0));
        assert_eq!(diff.repositories[0].items.len(), 2);
        // A later page's change would leave the first page's ETag as it was.
        assert!(state.repositories[REPO].etag.is_none());
        assert_eq!(state.repositories[REPO].numbers, [1, 2]);
    }
    // So would a full page's successor: its ETag is not kept either.
    let numbers: Vec<u64> = (1..=PAGE_SIZE as u64).collect();
    fake.gets.insert(
        url,
        json!({"status": 200, "etag": "W/\"full\"", "link": null, "body": listing(&numbers)}),
    );
    for batch in numbers.chunks(DETAIL_BATCH) {
        let nodes: serde_json::Map<_, _> = batch
            .iter()
            .enumerate()
            .map(|(k, n)| (format!("i{k}"), node("Issue", *n, json!({"createdAt": IN}))))
            .collect();
        fake.graphql.borrow_mut().push(json!({"repository": nodes}));
    }
    let diff = read(&fake, &scope(), &mut state, now).unwrap();
    assert_eq!((diff.requests, diff.revalidated), (5, 0));
    assert_eq!(diff.repositories[0].items.len(), PAGE_SIZE);
    assert!(state.repositories[REPO].etag.is_none());
    assert_eq!(state.repositories[REPO].numbers, numbers);
}

#[test]
fn first_read_covers_the_first_window_and_a_commit_moves_the_watermark() {
    let now = parse_time("2026-11-11T16:00:00Z").unwrap();
    let later = parse_time("2026-11-11T17:00:00Z").unwrap();
    let mut fake = Fake::default();
    // The only list the fake answers: a window computed from the later
    // clock would ask for another and fail.
    fake.gets.insert(
        list_url("2026-11-10T16:00:00Z"),
        json!({"status": 200, "etag": "W/\"first\"", "link": null, "body": listing(&[1])}),
    );
    // Updated in the first window, before a window starting an hour later.
    let early = node(
        "Issue",
        1,
        json!({"createdAt": "2026-11-10T16:30:00Z", "updatedAt": "2026-11-10T16:30:00Z"}),
    );
    let mut state = State::default();
    for (clock, revalidated) in [(now, 0), (later, 1)] {
        fake.graphql
            .borrow_mut()
            .push(json!({"repository": {"i0": early}}));
        let diff = read(&fake, &scope(), &mut state, clock).unwrap();
        assert_eq!((diff.requests, diff.revalidated), (2, revalidated));
        assert_eq!(diff.repositories[0].since, "2026-11-10T16:00:00Z");
        assert_eq!(
            diff.repositories[0].items[0].changes,
            [Change::Opened].into()
        );
        // Not committed: the window's start is kept, the watermark is not set.
        assert_eq!(
            (
                state.repositories[REPO].since.as_deref(),
                state.repositories[REPO].start.as_deref()
            ),
            (None, Some("2026-11-10T16:00:00Z"))
        );
        // Persist and reload, as separate CLI invocations do.
        state = State::from_json(&state.to_json().unwrap()).unwrap();
        if revalidated == 1 {
            diff.commit(&mut state);
        }
    }
    assert_eq!(
        (
            state.repositories[REPO].since.as_deref(),
            state.repositories[REPO].start.as_deref()
        ),
        (Some("2026-11-11T17:00:00Z"), None)
    );
    // Committed: the next read starts SKEW before the watermark.
    fake.gets.insert(
        list_url("2026-11-11T16:55:00Z"),
        json!({"status": 200, "link": null, "body": listing(&[])}),
    );
    let diff = read(
        &fake,
        &scope(),
        &mut state,
        parse_time("2026-11-11T18:00:00Z").unwrap(),
    )
    .unwrap();
    assert_eq!(diff.repositories[0].since, "2026-11-11T16:55:00Z");
    assert!(diff.repositories[0].items.is_empty());
    let value = state.to_json().unwrap();
    assert_eq!(value["schema"], STATE_SCHEMA);
    assert_eq!(State::from_json(&value).unwrap(), state);
    assert!(
        State::from_json(&json!({"schema": "other"}))
            .unwrap_err()
            .to_string()
            .contains("not a changes state")
    );
    assert!(State::from_json(&json!({"schema": STATE_SCHEMA, "extra": 1})).is_err());
}

#[test]
fn list_refusals() {
    let url = list_url("2026-11-11T13:55:00Z");
    let now = parse_time("2026-11-11T16:00:00Z").unwrap();
    for (name, answer, error) in [
        (
            "a status that is not a list",
            json!({"status": 500, "link": null, "body": null}),
            "GitHub HTTP status 500",
        ),
        (
            "a page elsewhere",
            json!({"status": 200, "link": "<https://elsewhere.example/x>; rel=\"next\"", "body": listing(&[])}),
            "outside https://api.github.com/",
        ),
        (
            "a list without end",
            json!({"status": 200, "link": format!("<{url}>; rel=\"next\""), "body": listing(&[])}),
            "more than 10 pages",
        ),
        (
            "an issue without a number",
            json!({"status": 200, "link": null, "body": [{}]}),
            "an issue has no number",
        ),
    ] {
        let mut fake = Fake::default();
        fake.gets.insert(url.clone(), answer);
        let text = format!(
            "{:#}",
            read(&fake, &scope(), &mut state_since(SINCE), now).unwrap_err()
        );
        assert!(text.contains(error), "{name}: {text}");
    }
    // A listed item the detail read does not return.
    let mut fake = Fake::default();
    fake.gets.insert(
        url,
        json!({"status": 200, "link": null, "body": listing(&[7])}),
    );
    fake.graphql
        .borrow_mut()
        .push(json!({"repository": {"i0": null}}));
    let error = read(&fake, &scope(), &mut state_since(SINCE), now).unwrap_err();
    assert!(format!("{error:#}").contains("example/project#7 was listed but not read"));
}

#[test]
fn detail_query_aliases_each_number() {
    let query = detail_query(&[3, 10]);
    assert!(query.contains(
        "i0:issueOrPullRequest(number:3){...F} i1:issueOrPullRequest(number:10){...F} }}"
    ));
    assert!(query.ends_with("ConvertToDraftEvent{createdAt}}"));
}

#[test]
fn discover_table() {
    let listing = |repositories: Value| {
        let mut fake = Fake::default();
        fake.gets.insert(
            format!("{API_ROOT}orgs/o/repos?type=public&per_page=100"),
            json!({"status": 200, "link": null, "body": repositories}),
        );
        fake
    };
    let entry = |name: &str, fields: Value| {
        let mut entry = json!({"full_name": name, "fork": true, "private": false, "archived": false,
            "topics": [], "custom_properties": {}});
        for (key, value) in fields.as_object().unwrap() {
            entry[key] = value.clone();
        }
        entry
    };
    let midstream: Midstream =
        serde_json::from_value(json!({"owner": "o", "topic": "bot-midstream"})).unwrap();
    let fake = listing(json!([
        entry("o/topic", json!({"topics": ["bot-midstream"]})),
        entry("o/origin", json!({"custom_properties": {"origin": "fork"}})),
        entry("o/Followed", json!({"topics": ["bot-midstream"]})),
        entry("o/plain", json!({})),
        entry(
            "o/not-a-fork",
            json!({"fork": false, "topics": ["bot-midstream"]})
        ),
        entry(
            "o/private",
            json!({"private": true, "topics": ["bot-midstream"]})
        ),
        entry(
            "o/archived",
            json!({"archived": true, "topics": ["bot-midstream"]})
        ),
        entry(
            "o/other-origin",
            json!({"custom_properties": {"origin": "upstream"}})
        ),
    ]));
    let follow = ["o/followed".to_owned(), "O/Followed".to_owned()];
    let found = discover(&fake, &follow, Some(&midstream)).unwrap();
    assert_eq!(
        found
            .iter()
            .map(|f| (f.name.as_str(), f.midstream))
            .collect::<Vec<_>>(),
        [("o/followed", false), ("o/origin", true), ("o/topic", true)]
    );
    // Without a midstream owner, the follow list alone, and no listing.
    assert_eq!(discover(&Fake::default(), &follow, None).unwrap().len(), 1);
    for (name, fake, error) in [
        (
            "a foreign repository",
            listing(json!([entry("x/y", json!({"topics": ["bot-midstream"]}))])),
            "x/y is listed as o's",
        ),
        (
            "a repository without topics",
            listing(
                json!([{"full_name": "o/r", "fork": true, "private": false, "archived": false}]),
            ),
            "o/r has no topics",
        ),
    ] {
        let text = format!("{:#}", discover(&fake, &[], Some(&midstream)).unwrap_err());
        assert!(text.contains(error), "{name}: {text}");
    }
    assert!(
        discover(&Fake::default(), &["not a repo".to_owned()], None)
            .unwrap_err()
            .to_string()
            .contains("follow")
    );
}

#[test]
fn summary_keeps_to_its_lines() {
    let repository = |n: usize, items: usize| RepositoryDiff {
        repository: format!("o/r{n:02}"),
        midstream: false,
        since: SINCE.to_owned(),
        items: (0..items)
            .map(|i| Item {
                reference: format!("o/r{n:02}#{i}"),
                changes: [Change::Opened].into(),
                ..Item::default()
            })
            .collect(),
    };
    let diff = |changed: usize| Diff {
        schema: SCHEMA,
        now: IN.to_owned(),
        repositories: (0..changed + 1)
            .map(|n| repository(n, usize::from(n < changed)))
            .collect(),
        requests: 0,
        revalidated: 0,
    };
    let summary = diff(SUMMARY_LINES - 1).summary();
    assert_eq!(summary.lines().count(), SUMMARY_LINES);
    assert!(!summary.contains("more"), "{summary}");
    let summary = diff(SUMMARY_LINES + 5).summary();
    assert_eq!(summary.lines().count(), SUMMARY_LINES);
    assert!(
        summary.ends_with("o/r17: 1 items; opened 1\n+7 more repositories with 7 items\n"),
        "{summary}"
    );
}
