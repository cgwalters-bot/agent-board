//! Scenario edits of public observations, not claims of live authorization.
use super::*;
use crate::{pass::Replay, poll::RestResponse};
use serde_json::json;
use std::cell::{Cell, RefCell};

const FIXTURE: &str = include_str!("../../../../fixtures/promote/public-2026-11-04.json");
const CONFIG: &str = include_str!("../../../../fixtures/promote/config.json");
const REPO: &str = "cgwalters-forge/composefs-rs";
const HEAD: &str = "866b6ce9050782c50eb0fec1c08b37cebec93dc6";
const PUSH: &str = "2026-10-22T03:25:48Z";
const COMMAND: &str = "2026-10-23T00:00:00Z";

fn entry<'a>(replay: &'a mut Replay, needle: &str) -> &'a mut Value {
    replay
        .requests
        .iter_mut()
        .find(|e| {
            e["request"]["get"]
                .as_str()
                .is_some_and(|s| s.contains(needle))
        })
        .unwrap()
}

fn body<'a>(replay: &'a mut Replay, needle: &str) -> &'a mut Value {
    &mut entry(replay, needle)["answer"]["body"]
}

fn activity(replay: &mut Replay) -> &mut Value {
    body(
        replay,
        "activity?ref=refs%2Fheads%2Fdispatch%2Fimplement%2Fagent-run-39714560993",
    )
}
fn pull(replay: &mut Replay) -> &mut Value {
    &mut body(replay, "pulls?state=open")[0]
}
fn comments(replay: &mut Replay) -> &mut Value {
    body(replay, "/issues/34/comments?")
}

fn comment_metadata(replay: &mut Replay) -> &mut Value {
    &mut replay
        .requests
        .iter_mut()
        .find(|e| e["request"]["variables"]["number"] == 34)
        .unwrap()["answer"]["data"]["repository"]["pullRequest"]["comments"]["nodes"]
}

// Scenario edits must agree across both independent comment observations.
fn sync_metadata(replay: &mut Replay) {
    let nodes: Vec<_> = comments(replay).as_array().unwrap().iter().map(|c| json!({
        "url": format!("https://github.com/{REPO}/pull/34#issuecomment-{}", c["id"].as_u64().unwrap()),
        "author": {"login": c["user"]["login"]}, "createdAt": c["created_at"],
        "lastEditedAt": null, "editor": null
    })).collect();
    *comment_metadata(replay) = json!(nodes);
}
fn reviews(replay: &mut Replay) -> &mut Value {
    body(replay, "/pulls/34/reviews?")
}

fn scenario() -> Replay {
    let mut replay = Replay::from_json(&serde_json::from_str(FIXTURE).unwrap()).unwrap();
    // Keep one observed bot-fork PR; readiness and command are scenario edits.
    let selected = body(&mut replay, "pulls?state=open")
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["number"] == 34)
        .unwrap()
        .clone();
    *body(&mut replay, "pulls?state=open") = json!([selected]);
    pull(&mut replay)["draft"] = json!(false);
    pull(&mut replay)["labels"] = json!([]);
    *comments(&mut replay) = json!([{"id": 1, "user": {"login": "cgwalters"}, "created_at": COMMAND, "updated_at": COMMAND, "body": "/promote"}]);
    sync_metadata(&mut replay);
    replay
}

fn candidate(human: bool) -> String {
    format!(
        "candidate promote {REPO}#34 head={HEAD} command=https://github.com/{REPO}/pull/34#issuecomment-1 at={COMMAND} human-text={human} push={PUSH}; staging verification required for bot-fork head"
    )
}

fn hold(reason: &str) -> String {
    format!("hold promote {REPO}#34 head={HEAD} command=unverified: {reason}")
}

struct Forge {
    replay: Replay,
    reads: RefCell<Vec<String>>,
    writes: Cell<usize>,
}

impl Rest for Forge {
    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
        self.reads.borrow_mut().push(url.into());
        self.replay.get(url, validator)
    }
    fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
        self.writes.set(self.writes.get() + 1);
        anyhow::bail!("mutation attempted")
    }
}

impl Transport for Forge {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.reads.borrow_mut().push(format!("{API}graphql"));
        self.replay.graphql(query, variables)
    }
}

fn run(replay: Replay, config: &Config) -> (Result<Vec<String>>, Forge) {
    let forge = Forge {
        replay,
        reads: RefCell::new(vec![]),
        writes: Cell::new(0),
    };
    let result = preview(&forge, config, forge.replay.now);
    assert_eq!(forge.writes.get(), 0);
    (result, forge)
}

#[test]
fn recorded_scenarios() {
    type Case = (&'static str, fn(&mut Replay), String);
    let none = "no operator command after observed push";
    let evidence = format!(
        "command=https://github.com/{REPO}/pull/34#issuecomment-1 at={COMMAND} human-text=false push={PUSH}"
    );
    let cases: Vec<Case> = vec![
        ("after push", |_| {}, candidate(false)),
        (
            "human-text",
            |r| comments(r)[0]["body"] = json!("/promote --human-text"),
            candidate(true),
        ),
        (
            "before push",
            |r| {
                comments(r)[0]["created_at"] = json!("2026-10-21T00:00:00Z");
                comments(r)[0]["updated_at"] = json!("2026-10-21T00:00:00Z");
            },
            hold(&format!(
                "stale command=https://github.com/{REPO}/pull/34#issuecomment-1 at=2026-10-21T00:00:00Z push={PUSH}"
            )),
        ),
        (
            "review only",
            |r| {
                *comments(r) = json!([]);
                *reviews(r) = json!([{"id": 5, "user": {"login":"cgwalters"}, "state":"APPROVED", "submitted_at":COMMAND, "commit_id":HEAD}]);
            },
            hold(none),
        ),
        (
            "other author",
            |r| comments(r)[0]["user"]["login"] = json!("visitor"),
            hold(none),
        ),
        (
            "quoted",
            |r| comments(r)[0]["body"] = json!("> /promote"),
            hold(none),
        ),
        (
            "fenced",
            |r| comments(r)[0]["body"] = json!("```text\n/promote\n```"),
            hold(none),
        ),
        (
            "HTML",
            |r| comments(r)[0]["body"] = json!("<!--\n/promote\n-->"),
            hold(none),
        ),
        (
            "multiline link title",
            |r| comments(r)[0]["body"] = json!("[example](https://example.com \"\n/promote\n\")"),
            hold(none),
        ),
        (
            "prose and command",
            |r| comments(r)[0]["body"] = json!("Please proceed.\n\n/promote"),
            hold(none),
        ),
        (
            "indented code",
            |r| comments(r)[0]["body"] = json!("    /promote"),
            hold(none),
        ),
        (
            "edited",
            |r| comments(r)[0]["updated_at"] = json!("2026-10-24T00:00:00Z"),
            hold("edited operator comment has unverifiable authorship"),
        ),
        ("draft", |r| pull(r)["draft"] = json!(true), hold("draft")),
        (
            "needs-human",
            |r| pull(r)["labels"] = json!([{"name":"Needs-Human"}]),
            hold("needs-human"),
        ),
        (
            "missing activity",
            |r| *activity(r) = json!([]),
            hold("missing branch activity"),
        ),
        ("missing comments", |r| *comments(r) = json!([]), hold(none)),
        (
            "missing author",
            |r| comments(r)[0]["user"] = Value::Null,
            hold("missing comment author"),
        ),
        (
            "out of order comments",
            |r| {
                let mut old = comments(r)[0].clone();
                old["id"] = json!(2);
                old["created_at"] = json!(PUSH);
                comments(r).as_array_mut().unwrap().push(old);
            },
            hold("out-of-order comments"),
        ),
        (
            "ambiguous command",
            |r| comments(r)[0]["body"] = json!("/promote\n/promote --human-text"),
            hold("ambiguous command lines"),
        ),
        (
            "simultaneous push",
            |r| {
                comments(r)[0]["created_at"] = json!(PUSH);
                comments(r)[0]["updated_at"] = json!(PUSH);
            },
            hold("command and push have ambiguous ordering"),
        ),
    ];
    let config = Config::parse(CONFIG).unwrap();
    for (name, edit, expected) in cases {
        let mut replay = scenario();
        edit(&mut replay);
        sync_metadata(&mut replay);
        let (result, _) = run(replay, &config);
        assert_eq!(result.unwrap(), [expected, REMAINING.into()], "{name}");
    }
    for state in ["DISMISSED", "CHANGES_REQUESTED"] {
        let mut replay = scenario();
        *reviews(&mut replay) = json!([{"id":5,"user":{"login":"cgwalters"},"state":state,"submitted_at":"2026-10-24T00:00:00Z"}]);
        let reason = if state == "DISMISSED" {
            "operator dismissal has no proved cancellation time"
        } else {
            "operator change request cancels command"
        };
        assert_eq!(
            run(replay, &config).0.unwrap(),
            [hold(&format!("{evidence}: {reason}")), REMAINING.into()]
        );
    }
}

#[test]
fn authenticated_edit_evidence_is_required_even_with_matching_rest_seconds() {
    let config = Config::parse(CONFIG).unwrap();
    for failure in [
        "same-second edit",
        "missing edit time",
        "missing editor",
        "missing node",
        "wrong identity",
        "unreadable",
        "truncated",
    ] {
        let mut replay = scenario();
        match failure {
            "same-second edit" => {
                comment_metadata(&mut replay)[0]["lastEditedAt"] = json!(COMMAND);
                comment_metadata(&mut replay)[0]["editor"] = json!({"login":"visitor"});
            }
            "missing edit time" => {
                comment_metadata(&mut replay)[0]
                    .as_object_mut()
                    .unwrap()
                    .remove("lastEditedAt");
            }
            "missing editor" => {
                comment_metadata(&mut replay)[0]
                    .as_object_mut()
                    .unwrap()
                    .remove("editor");
            }
            "missing node" => *comment_metadata(&mut replay) = json!([]),
            "wrong identity" => {
                comment_metadata(&mut replay)[0]["url"] =
                    json!("https://github.com/other/repo/pull/34#issuecomment-1")
            }
            _ => {
                let e = replay
                    .requests
                    .iter_mut()
                    .find(|e| e["request"]["variables"]["number"] == 34)
                    .unwrap();
                if failure == "unreadable" {
                    e["answer"] = json!({"errors":[{"message":"unavailable"}]});
                } else {
                    e["answer"]["data"]["repository"]["pullRequest"]["comments"]["pageInfo"]["hasNextPage"] =
                        json!(true);
                }
            }
        }
        let lines = run(replay, &config).0.unwrap();
        assert!(lines[0].starts_with("hold promote"), "{failure}: {lines:?}");
        if failure == "same-second edit" {
            assert!(lines[0].contains("edited operator comment has unverifiable authorship"));
        }
    }
}

#[test]
fn malformed_review_pagination_cannot_hide_a_cancellation() {
    let config = Config::parse(CONFIG).unwrap();
    for link in [
        "<https://api.github.com/next>;rel=\"next\"",
        "<https://api.github.com/next>; rel=\"next\"; junk",
        "<https://api.github.com/next>; rel=\"next prev\"",
        "garbage; rel=\"last\"",
    ] {
        let mut replay = scenario();
        // The first spelling is valid; the other forms must fail closed. None
        // may silently turn an incomplete review list into authorization.
        entry(&mut replay, "/pulls/34/reviews?")["answer"]["link"] = json!(link);
        let lines = run(replay, &config).0.unwrap();
        assert!(lines[0].starts_with("hold promote"), "{link}: {lines:?}");
    }
}

#[test]
fn force_push_back_to_old_oid_needs_a_new_command() {
    let config = Config::parse(CONFIG).unwrap();
    for (pushed, expected) in [
        (
            "2026-10-22T12:00:00Z",
            candidate(false).replace(PUSH, "2026-10-22T12:00:00Z"),
        ),
        (
            "2026-10-24T00:00:00Z",
            hold(&format!(
                "stale command=https://github.com/{REPO}/pull/34#issuecomment-1 at={COMMAND} push=2026-10-24T00:00:00Z"
            )),
        ),
    ] {
        let mut replay = scenario();
        let original = activity(&mut replay)[0].clone();
        let mut away = original.clone();
        away["activity_type"] = json!("force_push");
        away["timestamp"] = json!("2026-10-22T06:00:00Z");
        away["before"] = json!(HEAD);
        away["after"] = json!("a".repeat(40));
        let mut back = away.clone();
        back["timestamp"] = json!(pushed);
        back["before"] = json!("a".repeat(40));
        back["after"] = json!(HEAD);
        *activity(&mut replay) = json!([back, away, original]);
        assert_eq!(
            run(replay, &config).0.unwrap(),
            [expected, REMAINING.into()]
        );
    }
}

#[test]
fn incomplete_or_unreadable_is_never_empty_success() {
    let config = Config::parse(CONFIG).unwrap();
    for needle in [
        "pulls?state=open",
        "/issues/34/comments?",
        "activity?ref=refs%2Fheads%2Fdispatch%2Fimplement%2Fagent-run-39714560993",
    ] {
        for failure in ["status", "link", "full", "missing"] {
            let mut replay = scenario();
            let entry = entry(&mut replay, needle);
            match failure {
                "status" => entry["answer"]["status"] = json!(403),
                "link" => {
                    entry["answer"]["link"] = json!("<https://attacker.invalid/>; rel=\"next\"")
                }
                "full" => {
                    let row = entry["answer"]["body"][0].clone();
                    entry["answer"]["body"] = vec![row; PAGE_SIZE].into();
                }
                _ => entry["answer"]["body"] = Value::Null,
            }
            let (result, forge) = run(replay, &config);
            if needle == "pulls?state=open" {
                assert!(result.is_err(), "{failure}");
            } else {
                assert!(result.unwrap()[0].starts_with("hold promote"), "{failure}");
            }
            assert!(forge.reads.borrow().iter().all(|u| u.starts_with(API)));
        }
    }
    let mut replay = scenario();
    for entry in &mut replay.requests {
        if entry["request"]["get"] == format!("{API}repos/{REPO}") {
            entry["answer"]["body"]["parent"]["full_name"] = json!("other/repo");
            entry["answer"]["body"]["source"]["full_name"] = json!("other/repo");
        }
    }
    assert_eq!(
        run(replay, &config).0.unwrap(),
        [hold("fork-network mismatch"), REMAINING.into()]
    );
}

#[test]
fn complete_pagination_and_stable_order() {
    let mut replay = scenario();
    let first = entry(&mut replay, "pulls?state=open").clone();
    let mut second = first.clone();
    let url = first["request"]["get"].as_str().unwrap();
    second["request"]["get"] = json!(url.replace("&page=1", "&page=2"));
    // Two observed PRs, returned in reverse numeric order across pages.
    let observed: Value = serde_json::from_str(FIXTURE).unwrap();
    let p9 = observed["requests"][1]["answer"]["body"][0].clone();
    second["answer"]["body"] = json!([p9]);
    entry(&mut replay, "pulls?state=open")["answer"]["link"] = json!(format!(
        "<{}>; rel=\"next\"",
        second["request"]["get"].as_str().unwrap()
    ));
    replay.requests.push(second);
    let config = Config::parse(CONFIG).unwrap();
    assert_eq!(run(replay, &config).0.unwrap(), ["hold promote cgwalters-forge/composefs-rs#9 head=821891d51ce3f1130fd8a3cbfe440e644389c50c command=unverified: draft".into(), candidate(false), REMAINING.into()]);
}

#[test]
fn off_reads_nothing() {
    for promote in [None, Some(json!({"forks":{}}))] {
        let mut value: Value = serde_json::from_str(CONFIG).unwrap();
        value.as_object_mut().unwrap().remove("promote");
        if let Some(promote) = promote {
            value["promote"] = promote;
        }
        let config = Config::parse(&value.to_string()).unwrap();
        let (result, forge) = run(scenario(), &config);
        assert_eq!(result.unwrap(), ["promote: off"]);
        assert!(forge.reads.borrow().is_empty());
    }
}

#[test]
fn identity_and_activity_fail_closed() {
    type Case = (&'static str, fn(&mut Replay), &'static str);
    let config = Config::parse(CONFIG).unwrap();
    let cases: &[Case] = &[
        (
            "head branch",
            |r| pull(r)["head"]["ref"] = json!("../../escape?query"),
            "invalid branch",
        ),
        (
            "head repository",
            |r| pull(r)["head"]["repo"]["full_name"] = json!("bad/path/extra"),
            "invalid repository",
        ),
        (
            "head owner",
            |r| pull(r)["head"]["repo"]["owner"]["login"] = json!("visitor"),
            "head owner mismatch",
        ),
        (
            "branch prefix",
            |r| pull(r)["head"]["ref"] = json!("main"),
            "not a bot-owned branch",
        ),
        (
            "author",
            |r| pull(r)["user"]["login"] = json!("visitor"),
            "not bot-authored",
        ),
        (
            "activity head",
            |r| activity(r)[0]["after"] = json!("f".repeat(40)),
            "ambiguous or out-of-order branch activity",
        ),
        (
            "activity branch",
            |r| activity(r)[0]["ref"] = json!("refs/heads/other"),
            "ambiguous or out-of-order branch activity",
        ),
        (
            "activity future",
            |r| activity(r)[0]["timestamp"] = json!("2027-01-01T00:00:00Z"),
            "ambiguous or out-of-order branch activity",
        ),
        (
            "activity chain",
            |r| activity(r)[0]["before"] = json!("f".repeat(40)),
            "incomplete branch activity",
        ),
    ];
    for (name, edit, reason) in cases {
        let mut replay = scenario();
        edit(&mut replay);
        let (result, forge) = run(replay, &config);
        let line = &result.unwrap()[0];
        assert!(
            line.starts_with("hold promote") && line.contains(reason),
            "{name}: {line}"
        );
        assert!(
            forge
                .reads
                .borrow()
                .iter()
                .all(|u| !u.contains("escape") && !u.contains("bad/path"))
        );
    }
}

#[test]
fn reviews_cancel_but_never_trigger_or_restore_permission() {
    let config = Config::parse(CONFIG).unwrap();
    for (author, state, submitted, cancelled) in [
        ("cgwalters", "APPROVED", "2026-10-24T00:00:00Z", false),
        ("cgwalters", "COMMENTED", "2026-10-24T00:00:00Z", false),
        (
            "visitor",
            "CHANGES_REQUESTED",
            "2026-10-24T00:00:00Z",
            false,
        ),
        (
            "cgwalters",
            "CHANGES_REQUESTED",
            "2026-10-22T12:00:00Z",
            false,
        ),
        ("cgwalters", "CHANGES_REQUESTED", COMMAND, true),
    ] {
        let mut replay = scenario();
        *reviews(&mut replay) =
            json!([{"id":5,"user":{"login":author},"state":state,"submitted_at":submitted}]);
        let lines = run(replay, &config).0.unwrap();
        if cancelled {
            assert!(lines[0].contains("operator change request cancels command"));
        } else {
            assert_eq!(lines, [candidate(false), REMAINING.into()]);
        }
    }
    let mut replay = scenario();
    *reviews(&mut replay) = json!([
        {"id":5,"user":{"login":"cgwalters"},"state":"CHANGES_REQUESTED","submitted_at":"2026-10-24T00:00:00Z"},
        {"id":6,"user":{"login":"cgwalters"},"state":"APPROVED","submitted_at":"2026-10-25T00:00:00Z"}
    ]);
    assert!(run(replay, &config).0.unwrap()[0].contains("operator change request cancels command"));
}

#[test]
fn comments_and_activity_paginate_completely_or_hit_the_bound() {
    let config = Config::parse(CONFIG).unwrap();
    for needle in [
        "/issues/34/comments?",
        "activity?ref=refs%2Fheads%2Fdispatch%2Fimplement%2Fagent-run-39714560993",
    ] {
        let mut replay = scenario();
        if needle.starts_with("activity") {
            let mut pushed = activity(&mut replay)[0].clone();
            pushed["before"] = pushed["after"].clone();
            pushed["activity_type"] = json!("push");
            pushed["timestamp"] = json!("2026-10-22T12:00:00Z");
            activity(&mut replay)
                .as_array_mut()
                .unwrap()
                .insert(0, pushed);
        } else {
            let mut earlier = comments(&mut replay)[0].clone();
            earlier["id"] = json!(2);
            earlier["user"]["login"] = json!("visitor");
            earlier["created_at"] = json!(PUSH);
            comments(&mut replay)
                .as_array_mut()
                .unwrap()
                .insert(0, earlier);
            sync_metadata(&mut replay);
        }
        let first = entry(&mut replay, needle);
        let last = first["answer"]["body"]
            .as_array_mut()
            .unwrap()
            .pop()
            .unwrap();
        let mut second = first.clone();
        second["request"]["get"] = json!(
            first["request"]["get"]
                .as_str()
                .unwrap()
                .replace("&page=1", "&page=2")
                .replace("?per_page=100&page=1", "?per_page=100&page=2")
        );
        second["answer"]["body"] = json!([last]);
        first["answer"]["link"] = json!(format!(
            "<{}>; rel=\"next\"",
            second["request"]["get"].as_str().unwrap()
        ));
        replay.requests.push(second);
        let expected = if needle.starts_with("activity") {
            candidate(false).replace(PUSH, "2026-10-22T12:00:00Z")
        } else {
            candidate(false)
        };
        assert_eq!(
            run(replay, &config).0.unwrap(),
            [expected, REMAINING.into()]
        );
    }
    let mut replay = scenario();
    let mut page = entry(&mut replay, "pulls?state=open").clone();
    entry(&mut replay, "pulls?state=open")["answer"]["link"] =
        json!("<https://api.github.com/next>; rel=\"next\"");
    page["answer"]["link"] = json!("<https://api.github.com/next>; rel=\"next\"");
    let first_url = page["request"]["get"].as_str().unwrap().to_owned();
    for n in 2..=MAX_PAGES {
        page["request"]["get"] = json!(first_url.replace("&page=1", &format!("&page={n}")));
        replay.requests.push(page.clone());
    }
    assert_eq!(
        run(replay, &config).0.unwrap_err().to_string(),
        "listing exceeds 10 pages"
    );
}
