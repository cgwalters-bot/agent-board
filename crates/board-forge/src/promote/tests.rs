//! Scenario edits of public observations, not claims of live authorization.
use super::*;
use crate::{pass::Replay, poll::RestResponse};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const FIXTURE: &str = include_str!("../../../../fixtures/promote/public-2026-11-04.json");
const CONFIG: &str = include_str!("../../../../fixtures/promote/config.json");
const REPO: &str = "cgwalters-forge/composefs-rs";
const HEAD: &str = "866b6ce9050782c50eb0fec1c08b37cebec93dc6";
const PUSH: &str = "2026-10-22T03:25:48Z";
const COMMAND: &str = "2026-10-23T00:00:00Z";
const POLICY_FIXTURE: &str =
    include_str!("../../../../fixtures/promote/policy-public-2026-11-04.json");
const TRACKER: &str = "repos/cgwalters-forge/tracker";
const RECORD: &str =
    "repos/cgwalters-forge/tracker/contents/upstream-policy/composefs/composefs-rs.md";
/// The record's two recorded commits, oldest first.
const OLDER: &str = "220fbc83e8e46c5ea6614a285c3848ab253bd6db";
const NEWER: &str = "07834ad32644607d1cd79add4af025659c988104";

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

/// The PR's issue timeline, read only for an approval trigger.
fn timeline(replay: &mut Replay, events: Value) {
    replay.requests.push(json!({
        "request": {"get": format!("https://api.github.com/repos/{REPO}/issues/34/timeline?per_page=100&page=1")},
        "answer": {"status": 200, "link": null, "body": events}
    }));
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
    let gates = Replay::from_json(
        &serde_json::from_str(include_str!(
            "../../../../fixtures/promote/gates-public-2026-11-04.json"
        ))
        .unwrap(),
    )
    .unwrap();
    replay.requests.extend(gates.requests);
    let policy = Replay::from_json(&serde_json::from_str(POLICY_FIXTURE).unwrap()).unwrap();
    replay.requests.extend(policy.requests);
    // Hypothetical fresh base, bot-authored unsigned commit and safe text.
    body(&mut replay, "/compare/")["behind_by"] = json!(0);
    body(&mut replay, "/compare/")["commits"][0]["commit"] =
        json!({"message":"Implement the slice"});
    body(&mut replay, "/compare/")["commits"][0]["author"]["login"] = json!("cgwalters-bot");
    pull(&mut replay)["body"] = json!(format!("Part of {REPO}#31"));
    pull(&mut replay)["title"] = json!("Implement the slice");
    plan_comments(&mut replay, json!([]));
    replay
}

/// Shared pass/standalone test scenario: all permission and gate changes are
/// hypothetical edits of the public observations described in the fixture README.
pub(crate) fn awaiting_scenario() -> (Replay, Config) {
    let mut replay = scenario();
    pull(&mut replay)["draft"] = json!(true);
    *comments(&mut replay) = json!([]);
    sync_metadata(&mut replay);
    *reviews(&mut replay) = json!([{"id":1,"user":{"login":"cgwalters"},"state":"APPROVED","commit_id":HEAD,"submitted_at":COMMAND}]);
    timeline(&mut replay, json!([]));
    // The list observation is not a detail endpoint; serve the same scenario
    // pull at the independently re-read detail URL.
    let detail = pull(&mut replay).clone();
    replay.requests.push(json!({"request":{"get":format!("{API}repos/{REPO}/pulls/34")},"answer":{"status":200,"link":null,"body":detail}}));
    let mut config: Value = serde_json::from_str(CONFIG).unwrap();
    config["promote"]["onApproval"] = json!(true);
    let config = Config::parse(&config.to_string()).unwrap();
    discovery(&mut replay, &config, |_| {});
    (replay, config)
}

fn awaiting_pull(replay: &mut Replay) -> &mut Value {
    &mut replay
        .requests
        .iter_mut()
        .find(|e| {
            e["request"]["get"]
                .as_str()
                .is_some_and(|s| s.ends_with("/pulls/34"))
        })
        .unwrap()["answer"]["body"]
}

pub(crate) fn cancel_awaiting(replay: &mut Replay) {
    *comments(replay) = json!([{"id":1,"user":{"login":"cgwalters"},"created_at":COMMAND,"updated_at":COMMAND,"body":"/promote"}]);
    sync_metadata(replay);
    reviews(replay)[0]["state"] = json!("CHANGES_REQUESTED");
    reviews(replay)[0]["submitted_at"] = json!("2026-10-24T00:00:00Z");
}

pub(crate) fn discovery(replay: &mut Replay, config: &Config, edit: impl FnOnce(&mut Value)) {
    struct Answer(Value);
    impl Transport for Answer {
        fn graphql(&self, _: &str, _: Value) -> Result<Value> {
            Ok(self.0.clone())
        }
    }
    let mut answer: Value = serde_json::from_str(include_str!(
        "../../../../fixtures/promote/awaiting-public-2026-11-08.json"
    ))
    .unwrap();
    let p = &mut answer["repository"]["pullRequests"]["nodes"][0];
    p["number"] = json!(34);
    p["headRefOid"] = json!(HEAD);
    p["reviews"]["nodes"][0]["commit"]["oid"] = json!(HEAD);
    edit(&mut answer);
    let answer = Answer(answer);
    let recorder = crate::pass::Recorder::new(&answer);
    awaiting::candidates(&recorder, config, REPO).ok();
    let recorded =
        Replay::from_json(&serde_json::from_str(&recorder.fixture(replay.now).unwrap()).unwrap())
            .unwrap();
    for entry in recorded.requests {
        replay.requests.retain(|r| r["request"] != entry["request"]);
        replay.requests.push(entry);
    }
}

#[test]
fn awaiting_read_gates() {
    type Case = (&'static str, fn(&mut Replay), &'static str);
    for (name, edit, reason) in [
        (
            "behind",
            (|r: &mut Replay| body(r, "/compare/")["behind_by"] = json!(31)) as fn(&mut Replay),
            "needs publication rebase (behind_by=31)",
        ),
        (
            "needs-human",
            |r| awaiting_pull(r)["labels"] = json!([{"name":"needs-human"}]),
            "needs-human",
        ),
        (
            "policy missing",
            |r| {
                entry(r, RECORD)["answer"]["status"] = json!(404);
            },
            "policy missing",
        ),
        ("DCO", |_| {}, "DCO sign-off rewrite required"),
        (
            "bot fork",
            |r| {
                body(r, "/check-runs?")["check_runs"] = json!([]);
                body(r, "/check-runs?")["total_count"] = json!(0);
            },
            "bot-fork staging unverified",
        ),
    ] as [Case; 5]
    {
        let (mut replay, config) = awaiting_scenario();
        edit(&mut replay);
        assert_eq!(
            awaiting::candidates(&replay, &config, REPO).unwrap(),
            [34],
            "{name}"
        );
        let error = awaiting::Awaiting::new(&config)
            .reason(&replay, REPO, 34, replay.now)
            .unwrap_err();
        assert!(format!("{error:#}").contains(reason), "{name}: {error:#}");
    }
    let (mut replay, config) = awaiting_scenario();
    // Same-network midstream-owned head, no DCO: hypothetical ready candidate.
    body(&mut replay, "/check-runs?")["check_runs"] = json!([]);
    body(&mut replay, "/check-runs?")["total_count"] = json!(0);
    let old = format!("{API}repos/cgwalters-bot/composefs-rs/activity?");
    for e in &mut replay.requests {
        if let Some(url) = e["request"]["get"].as_str().filter(|s| s.starts_with(&old)) {
            e["request"]["get"] =
                json!(url.replace("repos/cgwalters-bot/", "repos/cgwalters-forge/"));
        }
    }
    awaiting_pull(&mut replay)["head"]["repo"]["full_name"] = json!(REPO);
    awaiting_pull(&mut replay)["head"]["repo"]["owner"]["login"] = json!("cgwalters-forge");
    for e in &mut replay.requests {
        if let Some(url) = e["request"]["get"]
            .as_str()
            .filter(|s| s.contains("/compare/") || s.contains("pulls?state=all&head="))
        {
            e["request"]["get"] = json!(url.replace("cgwalters-bot", "cgwalters-forge"));
        }
    }
    assert_eq!(
        awaiting::Awaiting::new(&config)
            .reason(&replay, REPO, 34, replay.now)
            .unwrap(),
        "ready to publish"
    );
}

#[test]
fn awaiting_discovery_is_not_permission() {
    let (mut replay, config) = awaiting_scenario();
    for (state, selected) in [
        ("APPROVED", true),
        ("CHANGES_REQUESTED", false),
        ("DISMISSED", false),
        ("COMMENTED", false),
    ] {
        discovery(&mut replay, &config, |data| {
            data["repository"]["pullRequests"]["nodes"][0]["reviews"]["nodes"][0]["state"] =
                json!(state)
        });
        assert_eq!(
            !awaiting::candidates(&replay, &config, REPO)
                .unwrap()
                .is_empty(),
            selected
        );
    }
    discovery(&mut replay, &config, |data| {
        data["repository"]["pullRequests"]["nodes"][0]["reviews"]["nodes"][0]["commit"]["oid"] =
            json!("0".repeat(40))
    });
    assert!(
        awaiting::candidates(&replay, &config, REPO)
            .unwrap()
            .is_empty()
    );
    discovery(&mut replay, &config, |data| {
        data["repository"]["pullRequests"]["pageInfo"]["hasNextPage"] = json!(true)
    });
    assert!(awaiting::candidates(&replay, &config, REPO).is_err());
}

#[test]
fn awaiting_cached_policy_read_failures_remain_failures() {
    let (mut replay, config) = awaiting_scenario();
    entry(&mut replay, RECORD)["answer"]["status"] = json!(502);
    let mut evaluator = awaiting::Awaiting::new(&config);
    for _ in 0..2 {
        let error = evaluator.reason(&replay, REPO, 34, replay.now).unwrap_err();
        assert!(error.is::<ReadFailure>(), "{error:#}");
        assert!(format!("{error:#}").contains("policy unreadable"));
    }
}

fn plan_comments(replay: &mut Replay, nodes: Value) {
    struct Answer(Value);
    impl Transport for Answer {
        fn graphql(&self, _: &str, _: Value) -> Result<Value> {
            Ok(self.0.clone())
        }
    }
    replay
        .requests
        .retain(|e| e["request"]["variables"]["number"] != 31);
    let answer = Answer(
        json!({"repository":{"issue":{"comments":{"nodes":nodes,"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}),
    );
    let recorder = crate::pass::Recorder::new(&answer);
    let issue =
        crate::parse_github_item(&format!("{REPO}#31"), None, crate::github::DEFAULT_HOST).unwrap();
    crate::plan::issue_comments(&recorder, &issue).unwrap();
    let recorded =
        Replay::from_json(&serde_json::from_str(&recorder.fixture(replay.now).unwrap()).unwrap())
            .unwrap();
    replay.requests.extend(recorded.requests);
}

fn candidate(human: bool) -> String {
    format!(
        "would promote {REPO}#34 -> composefs/composefs-rs (sign-off on 1 of 1 commits) (policy bot-ok); preview only; publication revalidates approval and all gates; stack=none (no plan); dco=required; head={HEAD} command=https://github.com/{REPO}/pull/34#issuecomment-1 at={COMMAND} human-text={human} push={PUSH}; staging verification required for bot-fork head"
    )
}

fn policy_hold(reason: &str) -> String {
    format!("hold promote {REPO}#34 head={HEAD}: {reason}")
}

fn text_evidence(replay: &mut Replay) -> &mut Value {
    &mut replay
        .requests
        .iter_mut()
        .find(|e| {
            e["request"]["graphql"].as_str().is_some()
                && e["answer"]["data"]["repository"]["pullRequest"]["userContentEdits"].is_object()
        })
        .unwrap()["answer"]["data"]["repository"]["pullRequest"]
}

/// Public endpoint shape, with explicitly hypothetical operator edits/rename.
fn human_scenario() -> Replay {
    let mut replay = scenario();
    set_current(&mut replay, "human-text");
    set_verdict(&mut replay, Some(OLDER), "human-text");
    comments(&mut replay)[0]["body"] = json!("/promote --human-text");
    activity(&mut replay)[0]["actor"] = json!({"login":"cgwalters"});
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../fixtures/promote/provenance-public-2026-11-04.json"
    ))
    .unwrap();
    assert_eq!(fixture["query"], provenance::QUERY);
    let mut answer = fixture["answer"].clone();
    let p = &mut answer["repository"]["pullRequest"];
    p["number"] = json!(34);
    p["url"] = json!(format!("https://github.com/{REPO}/pull/34"));
    p["headRefOid"] = json!(HEAD);
    p["title"] = pull(&mut replay)["title"].clone();
    p["body"] = pull(&mut replay)["body"].clone();
    p["lastEditedAt"] = json!(PUSH);
    p["editor"] = json!({"login":"cgwalters"});
    p["userContentEdits"] = json!({"totalCount":1,"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"editedAt":PUSH,"deletedAt":null,"editor":{"login":"cgwalters"}}]});
    p["timelineItems"] = json!({"totalCount":14,"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"id":"rename-1","createdAt":PUSH,"previousTitle":"Bot title","currentTitle":p["title"],"actor":{"login":"cgwalters"}}]});
    struct Answer(Value);
    impl Transport for Answer {
        fn graphql(&self, _: &str, _: Value) -> Result<Value> {
            Ok(self.0.clone())
        }
    }
    let answer = Answer(answer);
    let recorder = crate::pass::Recorder::new(&answer);
    recorder
        .graphql(
            provenance::QUERY,
            json!({"owner":"cgwalters-forge","name":"composefs-rs","number":34}),
        )
        .unwrap();
    replay.requests.extend(
        Replay::from_json(&serde_json::from_str(&recorder.fixture(replay.now).unwrap()).unwrap())
            .unwrap()
            .requests,
    );
    replay
}

pub(crate) fn awaiting_human_scenario() -> (Replay, Config) {
    let mut replay = human_scenario();
    let detail = pull(&mut replay).clone();
    replay.requests.push(json!({"request":{"get":format!("{API}repos/{REPO}/pulls/34")},"answer":{"status":200,"link":null,"body":detail}}));
    let config = Config::parse(CONFIG).unwrap();
    discovery(&mut replay, &config, |d| {
        d["repository"]["pullRequests"]["nodes"][0]["comments"]["nodes"] =
            json!([{"author":{"login":"cgwalters"},"body":"/promote --human-text"}])
    });
    (replay, config)
}

#[test]
fn awaiting_invalid_plan_is_a_semantic_hold() {
    let (mut replay, cfg) = awaiting_scenario();
    plan_comments(
        &mut replay,
        json!([{
            "url":format!("https://github.com/{REPO}/issues/31#issuecomment-99"),
            "author":{"login":"cgwalters"},"createdAt":COMMAND,"lastEditedAt":null,"editor":null,
            "body":"```agent-board-plan\n{}\n```"
        }]),
    );
    let error = awaiting::Awaiting::new(&cfg)
        .reason(&replay, REPO, 34, replay.now)
        .unwrap_err();
    assert!(error.is::<Held>(), "{error:#}");
}

fn human_candidate() -> String {
    candidate(true).replace("policy bot-ok", "policy human-text; provenance verified")
}

#[test]
fn human_text_provenance() {
    type Case = (&'static str, fn(&mut Replay), &'static str);
    let cases: &[Case] = &[
        ("valid", |_| {}, ""),
        (
            "marker",
            |r| comments(r)[0]["body"] = json!("/promote"),
            "selected command lacks --human-text",
        ),
        (
            "pusher",
            |r| activity(r)[0]["actor"]["login"] = json!("cgwalters-bot"),
            "current-head pusher is not the operator or is missing",
        ),
        (
            "missing pusher",
            |r| activity(r)[0]["actor"] = Value::Null,
            "current-head pusher is not the operator or is missing",
        ),
        (
            "body editor",
            |r| text_evidence(r)["editor"]["login"] = json!("cgwalters-bot"),
            "last body editor is not the operator or is missing",
        ),
        (
            "history editor",
            |r| {
                text_evidence(r)["userContentEdits"]["nodes"][0]["editor"]["login"] =
                    json!("visitor")
            },
            "last body editor is not the operator or is missing",
        ),
        (
            "missing editor",
            |r| text_evidence(r)["editor"] = Value::Null,
            "last body editor is not the operator or is missing",
        ),
        (
            "missing history editor",
            |r| text_evidence(r)["userContentEdits"]["nodes"][0]["editor"] = Value::Null,
            "deleted, ambiguous or unverifiable body edit history",
        ),
        (
            "deleted edit",
            |r| text_evidence(r)["userContentEdits"]["nodes"][0]["deletedAt"] = json!(COMMAND),
            "deleted, ambiguous or unverifiable body edit history",
        ),
        (
            "title renamer",
            |r| text_evidence(r)["timelineItems"]["nodes"][0]["actor"]["login"] = json!("visitor"),
            "last title renamer is not the operator or is missing",
        ),
        (
            "missing renamer",
            |r| text_evidence(r)["timelineItems"]["nodes"][0]["actor"] = Value::Null,
            "last title renamer is not the operator or is missing",
        ),
        (
            "later body",
            |r| {
                let p = text_evidence(r);
                p["lastEditedAt"] = json!(COMMAND);
                p["userContentEdits"]["nodes"][0]["editedAt"] = json!(COMMAND);
            },
            "body edit does not predate command; fresh command required",
        ),
        (
            "later title",
            |r| {
                text_evidence(r)["timelineItems"]["nodes"][0]["createdAt"] =
                    json!("2026-10-24T00:00:00Z")
            },
            "title rename does not predate command; fresh command required",
        ),
        (
            "missing edits",
            |r| text_evidence(r)["userContentEdits"] = Value::Null,
            "missing body edit history",
        ),
        (
            "no edit",
            |r| {
                let e = &mut text_evidence(r)["userContentEdits"];
                e["nodes"] = json!([]);
                e["totalCount"] = json!(0);
            },
            "missing body edit proof",
        ),
        (
            "truncated edits",
            |r| text_evidence(r)["userContentEdits"]["pageInfo"]["hasNextPage"] = json!(true),
            "incomplete body edit history",
        ),
        (
            "full edit page",
            |r| {
                let row = text_evidence(r)["userContentEdits"]["nodes"][0].clone();
                text_evidence(r)["userContentEdits"]["nodes"] = json!(vec![row; PAGE_SIZE]);
            },
            "incomplete body edit history",
        ),
        (
            "ambiguous edits",
            |r| {
                let row = text_evidence(r)["userContentEdits"]["nodes"][0].clone();
                let e = &mut text_evidence(r)["userContentEdits"];
                e["nodes"] = json!([row, row]);
                e["totalCount"] = json!(2);
            },
            "deleted, ambiguous or unverifiable body edit history",
        ),
        (
            "edit count",
            |r| text_evidence(r)["userContentEdits"]["totalCount"] = json!(2),
            "incomplete body edit history",
        ),
        (
            "missing timeline",
            |r| text_evidence(r)["timelineItems"] = Value::Null,
            "missing title rename timeline",
        ),
        (
            "no rename",
            |r| text_evidence(r)["timelineItems"]["nodes"] = json!([]),
            "missing title rename proof",
        ),
        (
            "truncated timeline",
            |r| text_evidence(r)["timelineItems"]["pageInfo"]["hasNextPage"] = json!(true),
            "incomplete title rename timeline",
        ),
        (
            "full timeline",
            |r| {
                let row = text_evidence(r)["timelineItems"]["nodes"][0].clone();
                text_evidence(r)["timelineItems"]["nodes"] = json!(vec![row; PAGE_SIZE]);
            },
            "incomplete title rename timeline",
        ),
        (
            "ambiguous renames",
            |r| {
                let row = text_evidence(r)["timelineItems"]["nodes"][0].clone();
                text_evidence(r)["timelineItems"]["nodes"] = json!([row, row]);
            },
            "ambiguous title rename timeline",
        ),
        (
            "attribution",
            |r| {
                pull(r)["body"] = json!(format!(
                    "Part of {REPO}#31\nGenerated-by: https://github.com/cgwalters/#llms"
                ))
            },
            "body still has Generated-by attribution",
        ),
        (
            "head disagreement",
            |r| text_evidence(r)["headRefOid"] = json!("a".repeat(40)),
            "REST/GraphQL PR/head/text disagreement",
        ),
        (
            "title disagreement",
            |r| text_evidence(r)["title"] = json!("Different"),
            "REST/GraphQL PR/head/text disagreement",
        ),
        (
            "body disagreement",
            |r| text_evidence(r)["body"] = json!("Different"),
            "REST/GraphQL PR/head/text disagreement",
        ),
        (
            "rename disagreement",
            |r| text_evidence(r)["timelineItems"]["nodes"][0]["currentTitle"] = json!("Different"),
            "title rename/current title disagreement",
        ),
        (
            "edit disagreement",
            |r| text_evidence(r)["lastEditedAt"] = Value::Null,
            "missing evidence time",
        ),
    ];
    let config = Config::parse(CONFIG).unwrap();
    for (name, edit, why) in cases {
        let mut replay = human_scenario();
        edit(&mut replay);
        let (lines, forge) = run(replay, &config);
        let expected = if why.is_empty() {
            human_candidate()
        } else {
            policy_hold(&format!("needs-text: {why}"))
        };
        assert_eq!(lines.unwrap(), [expected, REMAINING.into()], "{name}");
        assert_eq!(forge.writes.get(), 0, "{name}");
    }
}

fn exact<'a>(replay: &'a mut Replay, path: &str) -> &'a mut Value {
    let url = format!("{API}{path}");
    replay
        .requests
        .iter_mut()
        .find(|e| e["request"]["get"] == url)
        .unwrap()
}

/// The record as served now (None) or at a commit of its history.
fn record_path(at: Option<&str>) -> String {
    at.map_or(RECORD.into(), |sha| format!("{RECORD}?ref={sha}"))
}

fn edit_record(replay: &mut Replay, at: Option<&str>, edit: impl Fn(String) -> String) {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let file = &mut exact(replay, &record_path(at))["answer"]["body"];
    let text = String::from_utf8(
        STANDARD
            .decode(file["content"].as_str().unwrap().replace('\n', ""))
            .unwrap(),
    )
    .unwrap();
    let text = edit(text);
    file["size"] = json!(text.len());
    file["content"] = json!(STANDARD.encode(text));
}

fn set_verdict(replay: &mut Replay, at: Option<&str>, verdict: &'static str) {
    edit_record(replay, at, |t| {
        t.replacen("verdict: bot-ok", &format!("verdict: {verdict}"), 1)
    });
}

/// The newest version is the record served now; both change together.
fn set_current(replay: &mut Replay, verdict: &'static str) {
    set_verdict(replay, None, verdict);
    set_verdict(replay, Some(NEWER), verdict);
}

/// Scenario: the older version was stricter, so the newer one loosened it.
fn loosen(replay: &mut Replay) {
    set_verdict(replay, Some(OLDER), "human-only");
}

/// Scenario: NEWER authored by the operator, committed by `committer`.
fn operator_commit(replay: &mut Replay, verified: bool, committer: &str) {
    let commit = &mut exact(replay, &format!("{TRACKER}/commits/{NEWER}"))["answer"]["body"];
    commit["author"]["login"] = json!("cgwalters");
    commit["committer"]["login"] = json!(committer);
    commit["commit"]["verification"]["verified"] = json!(verified);
}

/// Scenario reads for a merged policy-repository PR whose head the operator
/// approved, merged as NEWER with `tree` (the approved head's tree is 'd's).
fn approved_pull(replay: &mut Replay, tree: char, review: &str) {
    let head = "c".repeat(40);
    let answer = |path: String, body: Value| json!({"request":{"get":format!("{API}{path}")},"answer":{"status":200,"link":null,"body":body}});
    *exact(
        replay,
        &format!("{TRACKER}/commits/{NEWER}/pulls?per_page=100&page=1"),
    )["answer"]["body"]
        .as_array_mut()
        .unwrap() = vec![
        json!({"number":7,"merged_at":COMMAND,"merge_commit_sha":NEWER,"base":{"ref":"main","repo":{"full_name":"cgwalters-forge/tracker"}},"head":{"sha":head}}),
    ];
    replay.requests.extend([
        answer(
            format!("{TRACKER}/pulls/7/reviews?per_page=100&page=1"),
            json!([
                {"user":{"login":"cgwalters"},"state":"APPROVED","commit_id":head},
                {"user":{"login":"cgwalters"},"state":review,"commit_id":head}
            ]),
        ),
        answer(
            format!("{TRACKER}/git/commits/{head}"),
            json!({"tree":{"sha":"d".repeat(40)}}),
        ),
        answer(
            format!("{TRACKER}/git/commits/{NEWER}"),
            json!({"tree":{"sha":tree.to_string().repeat(40)}}),
        ),
    ]);
}

/// The fixture's tree row for `tree`'s only entry: the record in the
/// invented leaf trees '2's (NEWER) and '4's (OLDER), a parent in the others.
fn tree_row<'a>(replay: &'a mut Replay, tree: &str) -> &'a mut Value {
    &mut exact(replay, &format!("{TRACKER}/git/trees/{tree}"))["answer"]["body"]["tree"][0]
}

/// Scenario: the record is a symlink in the leaf tree of '2's or '4's; the
/// contents API still serves its target's text as a file at the link's path.
fn symlink_at(replay: &mut Replay, leaf: char) {
    tree_row(replay, &leaf.to_string().repeat(40))["mode"] = json!("120000");
}

fn root_listing(replay: &mut Replay) -> &mut Vec<Value> {
    exact(replay, "repos/composefs/composefs-rs/contents")["answer"]["body"]
        .as_array_mut()
        .unwrap()
}

/// Two candidates for one upstream share one policy evaluation, pass or hold.
#[test]
fn policy_is_read_once_per_upstream() {
    let config = Config::parse(CONFIG).unwrap();
    for kind in ["bot-ok", "missing", "human-text"] {
        let missing = kind == "missing";
        let human = kind == "human-text";
        let mut replay = if human { human_scenario() } else { scenario() };
        // Scenario: #35 is a copy of #34 with its own comments and reviews.
        let mut second = pull(&mut replay).clone();
        second["number"] = json!(35);
        body(&mut replay, "pulls?state=open")
            .as_array_mut()
            .unwrap()
            .push(second);
        for needle in ["/issues/34/comments?", "/pulls/34/reviews?"] {
            let mut copy = entry(&mut replay, needle).clone();
            let url = copy["request"]["get"]
                .as_str()
                .unwrap()
                .replace("/34/", "/35/");
            copy["request"]["get"] = json!(url);
            replay.requests.push(copy);
        }
        let mut metadata = replay
            .requests
            .iter()
            .find(|e| e["request"]["variables"]["number"] == 34)
            .unwrap()
            .clone();
        metadata["request"]["variables"]["number"] = json!(35);
        let nodes =
            &mut metadata["answer"]["data"]["repository"]["pullRequest"]["comments"]["nodes"];
        nodes[0]["url"] = json!(format!("https://github.com/{REPO}/pull/35#issuecomment-1"));
        replay.requests.push(metadata);
        if human {
            // One upstream policy, but #35's last body editor is not operator.
            let mut evidence = replay
                .requests
                .iter()
                .find(|e| {
                    e["answer"]["data"]["repository"]["pullRequest"]["userContentEdits"].is_object()
                })
                .unwrap()
                .clone();
            evidence["request"]["variables"]["number"] = json!(35);
            let p = &mut evidence["answer"]["data"]["repository"]["pullRequest"];
            p["number"] = json!(35);
            p["url"] = json!(format!("https://github.com/{REPO}/pull/35"));
            p["editor"] = json!({"login":"visitor"});
            replay.requests.push(evidence);
        }
        if missing {
            exact(&mut replay, RECORD)["answer"] = json!({"status":404,"link":null,"body":null});
        }
        let (lines, forge) = run(replay, &config);
        let first = if missing {
            policy_hold(
                "policy missing: no record at cgwalters-forge/tracker:upstream-policy/composefs/composefs-rs.md",
            )
        } else if human {
            human_candidate()
        } else {
            candidate(false)
        };
        let second = if human {
            policy_hold("needs-text: last body editor is not the operator or is missing")
                .replace("#34", "#35")
        } else {
            first.replace("#34", "#35").replace("pull/34#", "pull/35#")
        };
        assert_eq!(lines.unwrap(), [first, second, REMAINING.into()]);
        let url = format!("{API}{RECORD}");
        assert_eq!(
            forge.reads.borrow().iter().filter(|u| **u == url).count(),
            1
        );
    }
}

#[test]
fn policy_gate() {
    type Case = (&'static str, fn(&mut Replay), String);
    let record = "cgwalters-forge/tracker:upstream-policy/composefs/composefs-rs.md";
    let sources = "composefs/composefs-rs:.claude, composefs/composefs-rs:.claude/CLAUDE.md, composefs/composefs-rs:.cursorrules, composefs/composefs-rs:.gemini, composefs/composefs-rs:AGENTS.md, composefs/composefs-rs:GOVERNANCE.md, composefs/composefs-rs:MAINTAINERS.md, composefs/composefs-rs:REVIEW.md, composefs/composefs-rs:SECURITY.md, bootc-dev/infra:common/AGENTS.md, bootc-dev/infra:common/REVIEW.md";
    let cases: Vec<Case> = vec![
        ("recorded bot-ok", |_| {}, candidate(false)),
        (
            "missing",
            |r| exact(r, RECORD)["answer"] = json!({"status":404,"link":null,"body":null}),
            policy_hold(&format!("policy missing: no record at {record}")),
        ),
        (
            "symlink",
            |r| symlink_at(r, '2'),
            policy_hold(&format!(
                "policy invalid: upstream-policy/composefs/composefs-rs.md has mode 120000 at {NEWER}, not a regular file"
            )),
        ),
        (
            // Its target's later edits, a tightening and loosening back
            // included, are outside the record's path history.
            "symlink in an earlier version",
            |r| symlink_at(r, '4'),
            policy_hold(&format!(
                "policy invalid: upstream-policy/composefs/composefs-rs.md has mode 120000 at {OLDER}, not a regular file"
            )),
        ),
        (
            "symlinked parent directory",
            |r| tree_row(r, &"1".repeat(40))["mode"] = json!("120000"),
            policy_hold(&format!(
                "policy history unreadable: upstream-policy/composefs/composefs-rs.md has a parent of mode 120000 at {NEWER}"
            )),
        ),
        (
            "version is not its tree's blob",
            |r| tree_row(r, &"2".repeat(40))["sha"] = json!("f".repeat(40)),
            policy_hold(&format!(
                "policy history unreadable: upstream-policy/composefs/composefs-rs.md at {NEWER} is not its tree's blob"
            )),
        ),
        (
            "directory",
            |r| exact(r, RECORD)["answer"]["body"] = json!([]),
            policy_hold(
                "policy invalid: upstream-policy/composefs/composefs-rs.md is not a regular file",
            ),
        ),
        (
            "unknown key",
            |r| {
                edit_record(r, None, |t| {
                    t.replacen("rebase: any", "rebase: any\nlicense: MIT", 1)
                })
            },
            policy_hold("policy invalid: unknown key \"license\""),
        ),
        (
            "unknown source key",
            |r| {
                edit_record(r, None, |t| {
                    t.replacen(
                        "    path: .claude\n",
                        "    path: .claude\n    ref: main\n",
                        1,
                    )
                })
            },
            policy_hold("policy invalid: unknown source key \"ref\""),
        ),
        (
            "no body",
            |r| edit_record(r, None, |t| t[..t.find("\n---\n").unwrap() + 5].into()),
            policy_hold("policy invalid: no body"),
        ),
        (
            "unknown verdict",
            |r| set_verdict(r, None, "bot-maybe"),
            policy_hold("policy invalid: unknown verdict \"bot-maybe\""),
        ),
        (
            "no front matter",
            |r| edit_record(r, None, |t| t.replacen("---\n", "", 1)),
            policy_hold("policy invalid: no front matter"),
        ),
        (
            "stale source sha",
            |r| {
                let agents = root_listing(r)
                    .iter_mut()
                    .find(|e| e["name"] == "AGENTS.md")
                    .unwrap();
                agents["sha"] = json!("0123456789abcdef0123456789abcdef01234567");
            },
            policy_hold(
                "policy stale: composefs/composefs-rs:AGENTS.md changed (61261301421a -> 0123456789ab)",
            ),
        ),
        (
            // Same id: a symlink's id is its target's name, not its text.
            "source became a symlink",
            |r| {
                root_listing(r)
                    .iter_mut()
                    .find(|e| e["name"] == "AGENTS.md")
                    .unwrap()["type"] = json!("symlink")
            },
            policy_hold(
                "policy stale: composefs/composefs-rs:AGENTS.md is a symlink, not a file or directory",
            ),
        ),
        (
            "source gone",
            |r| {
                *exact(r, "repos/bootc-dev/infra/contents/common")["answer"]["body"]
                    .as_array_mut()
                    .unwrap() = vec![]
            },
            policy_hold(
                "policy stale: bootc-dev/infra:common/AGENTS.md is gone, bootc-dev/infra:common/REVIEW.md is gone",
            ),
        ),
        (
            "unlisted new file",
            |r| {
                root_listing(r).push(json!({"name":"CONTRIBUTING.md","path":"CONTRIBUTING.md","sha":"a".repeat(40),"type":"file"}))
            },
            policy_hold(
                "policy stale: composefs/composefs-rs:CONTRIBUTING.md is not in the record",
            ),
        ),
        (
            "unlisted symlink",
            |r| {
                root_listing(r).push(json!({"name":"copilot-instructions.md","path":"copilot-instructions.md","sha":"a".repeat(40),"type":"symlink"}))
            },
            policy_hold(
                "policy stale: composefs/composefs-rs:copilot-instructions.md is not in the record",
            ),
        ),
        (
            "unlisted AI file in the owner's .github",
            |r| {
                exact(r, "repos/composefs/.github/contents")["answer"] = json!({"status":200,"link":null,"body":[{"name":"docs","path":"docs","sha":"b".repeat(40),"type":"dir"}]});
                r.requests.extend([".github", "doc"].map(|d| json!({"request":{"get":format!("{API}repos/composefs/.github/contents/{d}")},"answer":{"status":404,"link":null,"body":null}})));
                r.requests.push(json!({"request":{"get":format!("{API}repos/composefs/.github/contents/docs")},"answer":{"status":200,"link":null,"body":[{"name":"using-AI.md","path":"docs/using-AI.md","sha":"a".repeat(40),"type":"file"}]}}));
            },
            policy_hold("policy stale: composefs/.github:docs/using-AI.md is not in the record"),
        ),
        (
            "unreadable listing",
            |r| {
                exact(r, "repos/composefs/composefs-rs/contents/.github")["answer"]["status"] =
                    json!(403)
            },
            policy_hold(
                "policy unreadable: listing composefs/composefs-rs:.github: GET repos/composefs/composefs-rs/contents/.github: HTTP 403",
            ),
        ),
        (
            "truncated listing",
            |r| {
                let row = root_listing(r)[0].clone();
                *root_listing(r) = vec![row; 1000];
            },
            policy_hold(
                "policy unreadable: listing composefs/composefs-rs:: listing may be truncated",
            ),
        ),
        (
            "truncated commit list",
            |r| {
                let commits = exact(
                    r,
                    &format!(
                        "{TRACKER}/commits?path=upstream-policy%2Fcomposefs%2Fcomposefs-rs.md&per_page=100&page=1"
                    ),
                );
                let row = commits["answer"]["body"][0].clone();
                commits["answer"]["body"] = json!(vec![row; PAGE_SIZE]);
            },
            policy_hold("policy history unreadable: listing may be truncated"),
        ),
        (
            "loosened by a non-operator",
            loosen,
            policy_hold(&format!("policy loosened by cgwalters-bot at {NEWER}")),
        ),
        (
            "loosened by an unverified operator commit",
            |r| {
                loosen(r);
                operator_commit(r, false, "cgwalters");
            },
            policy_hold(&format!("policy loosened by cgwalters at {NEWER}")),
        ),
        (
            // The bot signs with its own key and sets the operator as author.
            "loosened by a verified commit with an operator author only",
            |r| {
                loosen(r);
                operator_commit(r, true, "cgwalters-bot");
            },
            policy_hold(&format!(
                "policy loosened by cgwalters-bot (author cgwalters) at {NEWER}"
            )),
        ),
        (
            "loosened by a verified operator commit",
            |r| {
                loosen(r);
                operator_commit(r, true, "cgwalters");
            },
            candidate(false),
        ),
        (
            "loosened through an operator-approved PR",
            |r| {
                loosen(r);
                approved_pull(r, 'd', "COMMENTED");
            },
            candidate(false),
        ),
        (
            "loosened through a PR merged with more than approved",
            |r| {
                loosen(r);
                approved_pull(r, 'e', "COMMENTED");
            },
            policy_hold(&format!("policy loosened by cgwalters-bot at {NEWER}")),
        ),
        (
            "loosened through a PR whose approval was withdrawn",
            |r| {
                loosen(r);
                approved_pull(r, 'd', "DISMISSED");
            },
            policy_hold(&format!("policy loosened by cgwalters-bot at {NEWER}")),
        ),
        (
            "human-only",
            |r| set_current(r, "human-only"),
            policy_hold(&format!(
                "policy human-only: the bot may not post there; sources: {sources}"
            )),
        ),
        (
            "human-text",
            |r| set_current(r, "human-text"),
            policy_hold("needs-text: selected command lacks --human-text"),
        ),
        (
            "no-go",
            |r| set_current(r, "no-go"),
            policy_hold(&format!(
                "policy no-go: the bot may not post there; sources: {sources}"
            )),
        ),
        (
            "changed during the read",
            |r| set_verdict(r, None, "no-go"),
            policy_hold("policy history unreadable: the record changed during the read"),
        ),
    ];
    let config = Config::parse(CONFIG).unwrap();
    for (name, edit, expected) in cases {
        let mut replay = scenario();
        edit(&mut replay);
        let (lines, forge) = run(replay, &config);
        assert_eq!(lines.unwrap(), [expected, REMAINING.into()], "{name}");
        // Each policy read happens at most once in a run.
        let reads = forge.reads.borrow();
        let policy: Vec<_> = reads
            .iter()
            .filter(|u| u.contains("/contents") || u.contains("cgwalters-forge/tracker"))
            .collect();
        assert_eq!(
            policy.len(),
            policy.iter().collect::<BTreeSet<_>>().len(),
            "{name}: {policy:?}"
        );
    }
}

fn hold(reason: &str) -> String {
    format!("hold promote {REPO}#34 head={HEAD} command=unverified: {reason}")
}

fn gate_hold(reason: &str) -> String {
    format!(
        "hold promote {REPO}#34 head={HEAD} command=https://github.com/{REPO}/pull/34#issuecomment-1 at={COMMAND} human-text=false push={PUSH}: {reason}"
    )
}

fn upstream(replay: &mut Replay) -> &mut Value {
    body(replay, "composefs/composefs-rs/pulls?state=all")
}

fn upstream_pr() -> Value {
    json!({"number":70,"state":"open","merged_at":null,"base":{"ref":"main","repo":{"full_name":"composefs/composefs-rs"}},"head":{"sha":HEAD,"ref":"dispatch/implement/agent-run-39714560993","repo":{"full_name":"cgwalters-bot/composefs-rs","owner":{"login":"cgwalters-bot"}}}})
}

fn signed_dag(replay: &mut Replay) {
    let c = body(replay, "/compare/");
    let base = c["merge_base_commit"]["sha"].clone();
    let commit = |sha: String, parents: Vec<Value>| json!({"sha":sha,"parents":parents,"author":{"login":"cgwalters-bot"},"commit":{"message":"Implement\n\nSigned-off-by: Preview fixture <preview@example.invalid>"}});
    c["ahead_by"] = json!(3);
    c["total_commits"] = json!(3);
    c["commits"] = json!([
        commit("a".repeat(40), vec![json!({"sha":base})]),
        commit("b".repeat(40), vec![json!({"sha":base})]),
        commit(
            HEAD.into(),
            vec![json!({"sha":"a".repeat(40)}), json!({"sha":"b".repeat(40)})]
        )
    ]);
}

fn plan_node(lower: Option<u64>) -> Value {
    let mut plan = json!({"schema":"agent-board/job-plan/v1","repo":REPO,"item":31,"kind":"implement","runner":"hosted","pool":"openai","brief":"Implement slice"});
    if let Some(lower) = lower {
        plan["stacks_on"] = json!(lower);
    }
    json!({"url":format!("https://github.com/{REPO}/issues/31#issuecomment-2"),"author":{"login":"cgwalters-bot"},"editor":null,"lastEditedAt":null,"createdAt":COMMAND,"body":format!("```agent-board-plan\n{plan}\n```")})
}

#[test]
fn plan_absence_is_distinct_from_unreadable_or_invalid_plan() {
    let config = Config::parse(CONFIG).unwrap();
    for case in [
        "no stacks_on",
        "missing edit time",
        "missing editor",
        "foreign edit",
        "invalid plan",
        "missing pageInfo",
    ] {
        let mut replay = scenario();
        let mut node = plan_node(None);
        match case {
            "missing edit time" => {
                node.as_object_mut().unwrap().remove("lastEditedAt");
            }
            "missing editor" => {
                node.as_object_mut().unwrap().remove("editor");
            }
            "foreign edit" => {
                node["lastEditedAt"] = json!(COMMAND);
                node["editor"] = json!({"login":"visitor"});
            }
            "invalid plan" => {
                node["body"] = json!("```agent-board-plan\n{}\n```");
            }
            _ => {}
        }
        plan_comments(&mut replay, json!([node]));
        if case == "missing pageInfo" {
            replay
                .requests
                .iter_mut()
                .find(|e| e["request"]["variables"]["number"] == 31)
                .unwrap()["answer"]["data"]["repository"]["issue"]["comments"]["pageInfo"] =
                Value::Null;
        }
        let lines = run(replay, &config).0.unwrap();
        if case == "no stacks_on" {
            assert_eq!(
                lines,
                [
                    candidate(false).replace("stack=none (no plan)", "stack=none"),
                    REMAINING.into()
                ]
            );
        } else {
            assert!(lines[0].starts_with("hold promote"), "{case}: {lines:?}");
        }
    }
}

#[test]
fn candidate_read_gates() {
    type Case = (&'static str, fn(&mut Replay), String);
    let cases: Vec<Case> = vec![
        ("pass", |_| {}, candidate(false)),
        (
            "already promoted",
            |r| *upstream(r) = json!([upstream_pr()]),
            format!(
                "already promoted {REPO}#34 -> composefs/composefs-rs#70; head={HEAD} command=https://github.com/{REPO}/pull/34#issuecomment-1 at={COMMAND} human-text=false push={PUSH}; staging verification required for bot-fork head"
            ),
        ),
        (
            "different head",
            |r| {
                let mut p = upstream_pr();
                p["head"]["sha"] = json!("a".repeat(40));
                *upstream(r) = json!([p]);
            },
            gate_hold("upstream composefs/composefs-rs#70 has a different head"),
        ),
        (
            "closed",
            |r| {
                let mut p = upstream_pr();
                p["state"] = json!("closed");
                *upstream(r) = json!([p]);
            },
            gate_hold("upstream composefs/composefs-rs#70 is closed/merged"),
        ),
        (
            "merged",
            |r| {
                let mut p = upstream_pr();
                p["state"] = json!("closed");
                p["merged_at"] = json!(COMMAND);
                *upstream(r) = json!([p]);
            },
            gate_hold("upstream composefs/composefs-rs#70 is closed/merged"),
        ),
        (
            "truncated upstream",
            |r| *upstream(r) = json!(vec![upstream_pr(); PAGE_SIZE]),
            gate_hold("listing may be truncated"),
        ),
        (
            "behind",
            |r| body(r, "/compare/")["behind_by"] = json!(1),
            gate_hold("needs publication rebase (behind_by=1)"),
        ),
        (
            "zero ahead",
            |r| {
                let c = body(r, "/compare/");
                c["ahead_by"] = json!(0);
                c["total_commits"] = json!(0);
                c["commits"] = json!([]);
            },
            gate_hold("already upstream (zero ahead)"),
        ),
        (
            "empty body",
            |r| pull(r)["body"] = json!(""),
            gate_hold("cannot tie fork PR to one issue deterministically: empty body"),
        ),
        (
            "ambiguous issue",
            |r| pull(r)["body"] = json!(format!("Part of {REPO}#31\nPart of {REPO}#32")),
            gate_hold("cannot tie fork PR to one issue deterministically"),
        ),
        (
            "bare title",
            |r| pull(r)["title"] = json!("Implement #31"),
            gate_hold("text has a bare #N reference that changes repository upstream"),
        ),
        (
            "bare body",
            |r| pull(r)["body"] = json!("Part of #31"),
            gate_hold("text has a bare #N reference that changes repository upstream"),
        ),
        (
            "closing title",
            |r| pull(r)["title"] = json!(format!("Fixes {REPO}#31")),
            gate_hold(&format!("text has a closing reference to {REPO}#31")),
        ),
        (
            "closing body",
            |r| pull(r)["body"] = json!(format!("Closes https://github.com/{REPO}/issues/31")),
            gate_hold(&format!("text has a closing reference to {REPO}#31")),
        ),
        (
            "third party",
            |r| body(r, "/compare/")["commits"][0]["author"]["login"] = json!("visitor"),
            gate_hold(&format!(
                "dco=required: missing sign-off on 1 of 1 commits: {HEAD} is by neither bot nor operator"
            )),
        ),
        (
            "merge commit",
            |r| {
                signed_dag(r);
                body(r, "/compare/")["commits"][2]["commit"]["message"] = json!("Unsigned merge");
            },
            gate_hold(&format!(
                "dco=required: missing sign-off on 1 of 3 commits: {HEAD} is a merge commit"
            )),
        ),
        (
            "fully signed merge DAG",
            signed_dag,
            candidate(false).replace("sign-off on 1 of 1", "sign-off on 0 of 3"),
        ),
        (
            "signed",
            |r| {
                body(r, "/compare/")["commits"][0]["commit"]["message"] =
                    json!("Implement\n\nSigned-off-by: Preview fixture <preview@example.invalid>")
            },
            candidate(false).replace("sign-off on 1 of 1", "sign-off on 0 of 1"),
        ),
        (
            "no DCO",
            |r| {
                body(r, "/check-runs?")["check_runs"]
                    .as_array_mut()
                    .unwrap()
                    .last_mut()
                    .unwrap()["name"] = json!("Other check");
            },
            candidate(false)
                .replace("sign-off on 1 of 1 commits", "no DCO")
                .replace("dco=required", "dco=not-required"),
        ),
        (
            "rule DCO",
            |r| {
                body(r, "/check-runs?")["check_runs"]
                    .as_array_mut()
                    .unwrap()
                    .last_mut()
                    .unwrap()["name"] = json!("Other check");
                body(r, "/rules/branches/")[2]["parameters"]["required_status_checks"][0]["context"] =
                    json!("DCO");
            },
            candidate(false),
        ),
    ];
    let config = Config::parse(CONFIG).unwrap();
    for (name, edit, expected) in cases {
        let mut replay = scenario();
        edit(&mut replay);
        let (lines, forge) = run(replay, &config);
        // The policy is read only for a candidate that passed every other gate.
        assert_eq!(
            forge.reads.borrow().iter().any(|u| u.contains(TRACKER)),
            expected.starts_with("would promote"),
            "{name}"
        );
        assert_eq!(lines.unwrap(), [expected, REMAINING.into()], "{name}");
    }
    for needle in [
        "composefs/composefs-rs/pulls?state=all",
        "/compare/",
        "/rules/branches/",
        "/branches/main",
        "/check-runs?",
    ] {
        let mut replay = scenario();
        entry(&mut replay, needle)["answer"]["status"] = json!(403);
        let lines = run(replay, &config).0.unwrap();
        assert!(
            lines[0].starts_with("hold promote") && lines[0].contains("HTTP 403"),
            "{needle}: {lines:?}"
        );
    }
    let mut replay = scenario();
    comments(&mut replay)[0]["body"] = json!("No command");
    sync_metadata(&mut replay);
    let (_, forge) = run(replay, &config);
    assert!(
        !forge
            .reads
            .borrow()
            .iter()
            .any(|s| s.contains("/compare/") || s.contains("state=all"))
    );
}

#[test]
fn accepted_plan_is_the_only_stack_source() {
    let config = Config::parse(CONFIG).unwrap();
    for state in [
        "open fork PR",
        "open upstream PR",
        "closed unmerged",
        "merged",
        "merged different base",
        "merged different head",
    ] {
        let mut replay = scenario();
        plan_comments(&mut replay, json!([plan_node(Some(30))]));
        let mut parent = pull(&mut replay).clone();
        parent["number"] = json!(33);
        parent["body"] = json!("Part of #30");
        parent["head"]["ref"] = json!("dispatch/parent");
        parent["state"] = json!(if state == "closed unmerged" {
            "closed"
        } else {
            "open"
        });
        replay.requests.push(json!({"request":{"get":format!("{API}repos/{REPO}/pulls?state=all&sort=created&direction=asc&per_page=100&page=1")},"answer":{"status":200,"link":null,"body":[parent]}}));
        let prs = if state.starts_with("merged") || state == "open upstream PR" {
            let mut p = upstream_pr();
            p["head"]["ref"] = json!("dispatch/parent");
            if state.starts_with("merged") {
                p["state"] = json!("closed");
                p["merged_at"] = json!(COMMAND);
            }
            if state == "merged different base" {
                p["base"]["ref"] = json!("release");
            }
            if state == "merged different head" {
                p["head"]["sha"] = json!("a".repeat(40));
            }
            json!([p])
        } else {
            json!([])
        };
        replay.requests.push(json!({"request":{"get":format!("{API}repos/composefs/composefs-rs/pulls?state=all&head=cgwalters-bot%3Adispatch%2Fparent&per_page=100&page=1")},"answer":{"status":200,"link":null,"body":prs}}));
        let lines = run(replay, &config).0.unwrap();
        let expected = match state {
            "merged different base" => gate_hold(
                "stack parent composefs/composefs-rs#70: merged into a different base, not main",
            ),
            "merged different head" => gate_hold(&format!(
                "stack parent composefs/composefs-rs#70: merged head differs from fork parent {REPO}#33"
            )),
            "merged" => candidate(false).replace(
                "stack=none (no plan)",
                &format!("stack={REPO}#33 -> composefs/composefs-rs#70 merged"),
            ),
            "open upstream PR" => {
                gate_hold("stack parent composefs/composefs-rs#70: open upstream PR")
            }
            "closed unmerged" => gate_hold(&format!(
                "stack parent {REPO}#33: closed unmerged; rebase by hand"
            )),
            _ => gate_hold(&format!("stack parent {REPO}#33: open fork PR")),
        };
        assert_eq!(lines, [expected, REMAINING.into()], "{state}");
    }
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

// Publication scenarios edit the same public recording. None is live write or
// staging evidence. The fake records every attempted mutation in order.
struct PublishingForge {
    replay: Replay,
    writes: RefCell<Vec<(String, String, Value)>>,
    fresh: Value,
    moved: bool,
    returned_head: bool,
    fail: Option<usize>,
    writer: String,
    returned_number: bool,
    /// The head repository's `bot/promote/*` branch: absent until a fake
    /// rewrite pushes it, or a scenario plants it.
    promote_ref: Rc<RefCell<Option<String>>>,
    /// Upstream PRs listed for that branch.
    promoted: RefCell<Vec<Value>>,
}

/// A rewrite whose head is a function of the upstream base alone, so a base
/// advance gives another head on the same branch, as the real replay does.
struct FakeRewrite {
    conflict: bool,
    /// Every compare prepared: a rebuild is one Git run.
    prepared: Vec<Value>,
    pushed: Rc<RefCell<Option<String>>>,
}

impl FakeRewrite {
    fn new(forge: &PublishingForge) -> Self {
        FakeRewrite {
            conflict: false,
            prepared: vec![],
            pushed: Rc::clone(&forge.promote_ref),
        }
    }

    fn head(base: &str) -> String {
        base[..1].repeat(40)
    }
}

impl rewrite::Rewrite for FakeRewrite {
    fn prepare(
        &mut self,
        _: &Config,
        _: &pass::promote::Fork,
        pull: &Value,
        compare: &Value,
    ) -> Result<(String, String)> {
        self.prepared.push(compare.clone());
        ensure!(!self.conflict, "rebase conflict");
        Ok((
            Self::head(compare["base_commit"]["sha"].as_str().unwrap()),
            rewrite::branch(pull["head"]["sha"].as_str().unwrap()),
        ))
    }
    fn publish(&mut self, _: &str, _: &str, head: &str) -> Result<()> {
        let mut pushed = self.pushed.borrow_mut();
        if let Some(existing) = pushed.as_deref() {
            ensure!(
                existing == head,
                "promotion branch already exists at another head"
            );
        }
        *pushed = Some(head.to_owned());
        Ok(())
    }
}

impl Rest for PublishingForge {
    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
        let value = if url == format!("{API}user") {
            Some(json!({"login":self.writer}))
        } else if url == format!("{API}repos/{REPO}/pulls/34") {
            Some(self.fresh.clone())
        } else if url == format!("{API}repos/{REPO}/pulls/35") {
            let mut replay = Replay {
                now: self.replay.now,
                requests: self.replay.requests.clone(),
            };
            Some(body(&mut replay, "pulls?state=open")[1].clone())
        } else if url.contains("/pulls?state=all&head=cgwalters-bot%3Abot%2Fpromote") {
            Some(json!(self.promoted.borrow().clone()))
        } else if url.contains("/git/ref/heads/bot%2Fpromote") {
            let Some(sha) = self.promote_ref.borrow().clone() else {
                return Err(anyhow::Error::from(crate::github::NotFound)
                    .context(format!("GET {url}: GitHub HTTP status 404")));
            };
            Some(json!({"object":{"sha":sha}}))
        } else if url.contains("/git/ref/heads/") {
            Some(json!({"object":{"sha":if self.moved { "a".repeat(40) } else { HEAD.into() }}}))
        } else {
            None
        };
        if let Some(body) = value {
            return Ok(RestResponse {
                status: 200,
                body,
                etag: None,
                last_modified: None,
                link: None,
            });
        }
        self.replay.get(url, validator)
    }

    fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        let mut writes = self.writes.borrow_mut();
        writes.push((method.into(), url.into(), body.clone()));
        if self.fail == Some(writes.len()) {
            anyhow::bail!("simulated lost response");
        }
        if url.contains("/pulls") {
            // An update answers with the PR it edits; a creation with a new one.
            let mut result = self
                .promoted
                .borrow()
                .iter()
                .find(|p| url.ends_with(&format!("/pulls/{}", p["number"])))
                .cloned()
                .unwrap_or_else(upstream_pr);
            result["base"] = json!({"ref":"main","repo":{"full_name":"composefs/composefs-rs"}});
            result["title"] = body["title"].clone();
            result["body"] = body["body"].clone();
            if let Some(head) = body["head"].as_str() {
                result["head"]["ref"] = json!(head.split_once(':').unwrap().1);
                if head.contains(":bot/promote") {
                    result["head"]["sha"] = json!(self.promote_ref.borrow().clone());
                }
            }
            if self.returned_head {
                result["head"]["sha"] = json!("b".repeat(40));
            }
            if self.returned_number {
                result["number"] = json!(71);
            }
            Ok((200, result))
        } else {
            Ok((200, json!({"id":99})))
        }
    }
}

impl Transport for PublishingForge {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.replay.graphql(query, variables)
    }
}

fn publication() -> (PublishingForge, Config) {
    let mut replay = scenario();
    *reviews(&mut replay) = json!([{"id":5,"user":{"login":"cgwalters"},"state":"APPROVED","submitted_at":COMMAND,"commit_id":HEAD}]);
    body(&mut replay, "/compare/")["commits"][0]["commit"]["message"] =
        json!("Implement\n\nSigned-off-by: Preview fixture <preview@example.invalid>");
    let fresh = pull(&mut replay).clone();
    let forge = PublishingForge {
        replay,
        fresh,
        writes: RefCell::new(vec![]),
        moved: false,
        returned_head: false,
        fail: None,
        writer: "cgwalters-bot".into(),
        returned_number: false,
        promote_ref: Rc::new(RefCell::new(None)),
        promoted: RefCell::new(vec![]),
    };
    let mut config = Config::parse(CONFIG).unwrap();
    config.promote.as_mut().unwrap().publish = true;
    (forge, config)
}

/// A second hypothetical PR on another branch, derived from exactly the
/// same scenario reads, as the next candidate after #34.
fn with_second(forge: &mut PublishingForge) {
    fn second(value: &mut Value) {
        match value {
            Value::String(text) => {
                *text = text
                    .replace("/34", "/35")
                    .replace("agent-run-39714560993", "agent-run-second")
            }
            Value::Array(values) => values.iter_mut().for_each(second),
            Value::Object(values) => {
                if values.get("number") == Some(&json!(34)) {
                    values.insert("number".into(), json!(35));
                }
                values.values_mut().for_each(second);
            }
            _ => {}
        }
    }
    let mut requests = forge.replay.requests.clone();
    requests.iter_mut().for_each(second);
    forge.replay.requests.extend(requests);
    let mut next = forge.fresh.clone();
    second(&mut next);
    body(&mut forge.replay, "pulls?state=open")
        .as_array_mut()
        .unwrap()
        .push(next);
}

/// The fork receipt and upstream PR the fake forge accepted from a rewritten
/// publication's writes, kept for the next invocation.
fn keep_rewritten(forge: &mut PublishingForge, writes: &[(String, String, Value)]) {
    let mut pr = upstream_pr();
    pr["head"]["sha"] = json!(forge.promote_ref.borrow().clone().unwrap());
    pr["head"]["ref"] = json!(rewrite::branch(HEAD));
    pr["title"] = writes[1].2["title"].clone();
    pr["body"] = writes[1].2["body"].clone();
    forge.promoted.borrow_mut().push(pr);
    comments(&mut forge.replay).as_array_mut().unwrap().push(json!({"id":99,"user":{"login":"cgwalters-bot"},"created_at":COMMAND,"updated_at":COMMAND,"body":writes[2].2["body"]}));
    sync_metadata(&mut forge.replay);
}

/// Upstream's base advances: a rebuild today would give another head. The
/// new base commit's checks read as the old one's did.
fn advance_base(forge: &mut PublishingForge) {
    let old = body(&mut forge.replay, "composefs-rs/branches/main")["commit"]["sha"]
        .as_str()
        .unwrap()
        .to_owned();
    let new = "d".repeat(40);
    let mut checks = entry(&mut forge.replay, &format!("commits/{old}/check-runs")).clone();
    let key = checks["request"]["get"]
        .as_str()
        .unwrap()
        .replace(&old, &new);
    checks["request"]["get"] = json!(key);
    forge.replay.requests.push(checks);
    body(&mut forge.replay, "composefs-rs/branches/main")["commit"]["sha"] = json!(new);
}

/// Scenario edits of recorded public responses: a hypothetical successful
/// publication followed by fork closure. This is not live promotion evidence.
pub(crate) fn handoff_scenario() -> (Replay, Config) {
    let mut replay = scenario();
    let mut fork = pull(&mut replay).clone();
    fork["state"] = json!("closed");
    fork["body"] = json!(format!("Closes {REPO}#31"));
    *pull(&mut replay) = fork.clone();
    replay.requests.push(json!({"request":{"get":format!("{API}repos/{REPO}/pulls/34")},"answer":{"status":200,"link":null,"body":fork.clone()}}));
    let receipt = handoff::success(REPO, &fork, &fork, "composefs/composefs-rs", 70).unwrap();
    *comments(&mut replay) = json!([{"id":99,"user":{"login":"cgwalters-bot"},"created_at":COMMAND,"updated_at":COMMAND,"body":receipt}]);
    sync_metadata(&mut replay);
    replay.requests.push(json!({"request":{"get":format!("{API}repos/{REPO}/issues/31/timeline?per_page=100&page=1")},"answer":{"status":200,"link":null,"body":[{"event":"cross-referenced","source":{"issue":{"html_url":format!("https://github.com/{REPO}/pull/34"),"pull_request":{}}}}]}}));
    let mut pr = upstream_pr();
    pr["base"] = json!({"ref":"main","repo":{"full_name":"composefs/composefs-rs"}});
    pr["user"] = json!({"login":"cgwalters-bot"});
    pr["body"] = fork["body"].clone();
    pr["merged"] = json!(false);
    pr["merged_at"] = Value::Null;
    replay.requests.push(json!({"request":{"get":format!("{API}repos/composefs/composefs-rs/pulls/70")},"answer":{"status":200,"link":null,"body":pr}}));
    let config = Config::parse(CONFIG).unwrap();
    discovery(&mut replay, &config, |answer| {
        answer["repository"]["pullRequests"]["nodes"] = json!([])
    });
    (replay, config)
}

#[test]
fn publish_creates_or_updates_exact_text_and_one_fork_receipt() {
    for existing in [false, true] {
        let (mut forge, config) = publication();
        if existing {
            let mut pr = upstream_pr();
            pr["base"] = json!({"ref":"main","repo":{"full_name":"composefs/composefs-rs"}});
            *upstream(&mut forge.replay) = json!([pr]);
            comments(&mut forge.replay).as_array_mut().unwrap().push(json!({"id":99,"user":{"login":"cgwalters-bot"},"created_at":COMMAND,"updated_at":COMMAND,"body":"<!-- agent-board-promote-status -->"}));
            sync_metadata(&mut forge.replay);
        }
        let lines = apply(&forge, &config, forge.replay.now, 3).unwrap();
        assert_eq!(
            lines,
            [format!(
                "promoted {REPO}#34 -> https://github.com/composefs/composefs-rs/pull/70 head={HEAD}"
            )]
        );
        let writes = forge.writes.borrow();
        assert_eq!(writes.len(), 3);
        let publication = &writes[1];
        assert_eq!(publication.0, if existing { "PATCH" } else { "POST" });
        assert_eq!(
            publication.1,
            format!(
                "{API}repos/composefs/composefs-rs/pulls{}",
                if existing { "/70" } else { "" }
            )
        );
        assert_eq!(publication.2["title"], forge.fresh["title"]);
        assert_eq!(publication.2["body"], forge.fresh["body"]);
        if existing {
            assert_eq!(publication.2.as_object().unwrap().len(), 2);
        } else {
            assert_eq!(publication.2["draft"], true);
            assert_eq!(publication.2["maintainer_can_modify"], false);
            assert_eq!(
                publication.2["head"],
                "cgwalters-bot:dispatch/implement/agent-run-39714560993"
            );
        }
        assert!(writes.last().unwrap().1.ends_with("/issues/comments/99"));
        assert!(
            writes.last().unwrap().2["body"]
                .as_str()
                .unwrap()
                .contains("Published approved head")
        );
        assert!(writes.iter().all(|(_, url, _)| !url.contains("/git/")));
        let receipt = writes.last().unwrap().2["body"].as_str().unwrap();
        let record = crate::plan::fenced_json(receipt, "agent-board-promoted")
            .unwrap()
            .unwrap();
        assert_eq!(record["schema"], "agent-board/promoted/v1");
        assert_eq!(record["fork_repository"], REPO);
        assert_eq!(record["fork_pr"], 34);
        assert_eq!(record["approved_oid"], HEAD);
        assert_eq!(record["upstream_repository"], "composefs/composefs-rs");
        assert_eq!(record["upstream_pr"], 70);
        assert_eq!(record["source_repository"], REPO);
        assert_eq!(record["source_issue"], 31);
    }
}

#[test]
fn interrupted_publication_never_emits_handoff_authority() {
    for case in [
        "lost response",
        "changed returned head",
        "receipt write failed",
    ] {
        let (mut forge, config) = publication();
        match case {
            "lost response" => forge.fail = Some(2),
            "changed returned head" => forge.returned_head = true,
            _ => forge.fail = Some(3),
        }
        assert!(
            apply(&forge, &config, forge.replay.now, 3).is_err(),
            "{case}"
        );
        let writes = forge.writes.borrow();
        if case != "receipt write failed" {
            assert!(
                writes.iter().all(|(_, _, payload)| !payload["body"]
                    .as_str()
                    .unwrap_or("")
                    .contains("```agent-board-promoted")),
                "{case}"
            );
        }
        // Even a failed final PATCH cannot make a pending comment authoritative.
        assert!(
            !writes[0].2["body"]
                .as_str()
                .unwrap()
                .contains("```agent-board-promoted")
        );
    }
}

#[test]
fn publication_rebases_and_signs_independently_of_dco() {
    for dco in [false, true] {
        for behind in [0, 1] {
            for conflict in [false, true] {
                let (mut forge, config) = publication();
                if !dco {
                    *body(&mut forge.replay, "/rules/branches/") = json!([]);
                    body(&mut forge.replay, "/check-runs")["check_runs"] = json!([]);
                    body(&mut forge.replay, "/check-runs")["total_count"] = json!(0);
                }
                let compare = body(&mut forge.replay, "/compare/");
                compare["behind_by"] = json!(behind);
                let mut first = compare["commits"][0].clone();
                first["sha"] = json!("a".repeat(40));
                first["commit"]["message"] = json!("First unsigned");
                let mut second = first.clone();
                second["sha"] = json!(HEAD);
                second["parents"] = json!([{"sha":"a".repeat(40)}]);
                compare["commits"] = json!([first, second]);
                compare["ahead_by"] = json!(2);
                compare["total_commits"] = json!(2);
                let mut git = FakeRewrite::new(&forge);
                git.conflict = conflict;
                let lines =
                    publish::apply_with(&forge, &config, forge.replay.now, 4, &mut git).unwrap();
                assert_eq!(git.prepared.len(), 1);
                assert_eq!(git.prepared[0]["commits"].as_array().unwrap().len(), 2);
                assert_eq!(forge.promote_ref.borrow().is_some(), !conflict, "{lines:?}");
                assert_eq!(forge.writes.borrow().len(), if conflict { 0 } else { 3 });
                assert!(
                    lines[0].starts_with(if conflict { "hold promote" } else { "promoted" }),
                    "{lines:?}"
                );
                if !conflict {
                    let (mut replay, config) = handoff_scenario();
                    let receipt = forge.writes.borrow().last().unwrap().2["body"].clone();
                    for entry in &mut replay.requests {
                        if entry["request"]["get"]
                            .as_str()
                            .is_some_and(|p| p.ends_with("/pulls/34"))
                        {
                            entry["answer"]["body"] = forge.fresh.clone();
                        }
                    }
                    comments(&mut replay)[0]["body"] = receipt;
                    sync_metadata(&mut replay);
                    let upstream = body(&mut replay, "/pulls/70");
                    upstream["body"] = forge.fresh["body"].clone();
                    upstream["head"]["sha"] = json!(forge.promote_ref.borrow().clone());
                    upstream["head"]["ref"] = json!(rewrite::branch(HEAD));
                    let facts = crate::pass::Facts {
                        issue: crate::parse_github_item(
                            &format!("{REPO}#31"),
                            None,
                            crate::github::DEFAULT_HOST,
                        )
                        .unwrap(),
                        open: true,
                        promotion: None,
                        created: replay.now,
                        updated: None,
                        author: Some(config.operator.clone()),
                        labels: vec![],
                        labels_complete: true,
                        assignees: Some(vec![]),
                        blocked_by: 0,
                        pulls: vec![],
                        pulls_complete: true,
                        parent: None,
                        priority: None,
                        status: None,
                        why: None,
                    };
                    let key = crate::pass::key(&facts.issue);
                    let mut items = BTreeMap::from([(key.clone(), facts)]);
                    handoff::enrich(&replay, &config, &mut items).unwrap();
                    assert!(matches!(
                        items[&key].promotion,
                        Some(crate::pass::Promotion::Open(_))
                    ));
                    // A subsequent change to the published head is still refused.
                    body(&mut replay, "/pulls/70")["head"]["sha"] = json!("d".repeat(40));
                    let notices = handoff::enrich(&replay, &config, &mut items).unwrap();
                    assert_eq!(notices.len(), 1);
                    assert!(notices[0].contains("live upstream handoff identity mismatch"));
                    assert_eq!(items[&key].promotion, Some(crate::pass::Promotion::Unknown));
                }
            }
        }
    }
}

#[test]
fn publication_refusals_never_publish_an_unapproved_head() {
    for case in [
        "disabled",
        "cap",
        "no approval",
        "old approval",
        "change request",
        "draft",
        "needs human",
        "policy",
        "DCO rewrite",
        "stale command",
        "fresh text",
        "moved",
        "returned head",
        "write failure",
        "closed upstream",
        "different upstream head",
        "different head repository",
        "different returned number",
        "wrong writer",
    ] {
        let (mut forge, mut config) = publication();
        match case {
            "disabled" => config.promote.as_mut().unwrap().publish = false,
            "no approval" => *reviews(&mut forge.replay) = json!([]),
            "old approval" => reviews(&mut forge.replay)[0]["commit_id"] = json!("a".repeat(40)),
            "change request" => reviews(&mut forge.replay)[0]["state"] = json!("CHANGES_REQUESTED"),
            "draft" => pull(&mut forge.replay)["draft"] = json!(true),
            "needs human" => pull(&mut forge.replay)["labels"] = json!([{"name":"needs-human"}]),
            "policy" => set_verdict(&mut forge.replay, None, "no-go"),
            "DCO rewrite" => {
                body(&mut forge.replay, "/compare/")["commits"][0]["commit"]["message"] =
                    json!("Unsigned")
            }
            "stale command" => comments(&mut forge.replay)[0]["body"] = json!("No command"),
            "fresh text" => forge.fresh["title"] = json!("Changed after approval"),
            "moved" => forge.moved = true,
            "returned head" => forge.returned_head = true,
            "write failure" => forge.fail = Some(2),
            "wrong writer" => forge.writer = "cgwalters".into(),
            "different head repository" => {
                let mut pr = upstream_pr();
                pr["head"]["repo"]["full_name"] = json!("cgwalters-bot/another-fork");
                *upstream(&mut forge.replay) = json!([pr]);
            }
            "different returned number" => {
                forge.returned_number = true;
                *upstream(&mut forge.replay) = json!([upstream_pr()]);
            }
            "closed upstream" | "different upstream head" => {
                let mut pr = upstream_pr();
                if case == "closed upstream" {
                    pr["state"] = json!("closed");
                } else {
                    pr["head"]["sha"] = json!("a".repeat(40));
                }
                *upstream(&mut forge.replay) = json!([pr]);
            }
            _ => {}
        }
        let result = apply(
            &forge,
            &config,
            forge.replay.now,
            if case == "cap" { 2 } else { 3 },
        );
        assert!(
            result.is_err()
                || result
                    .as_ref()
                    .unwrap()
                    .iter()
                    .all(|l| !l.starts_with("promoted ")),
            "{case}: {result:?}"
        );
        let writes = forge.writes.borrow();
        assert_eq!(
            writes.len(),
            match case {
                "moved" => 2,
                "returned head" | "write failure" | "different returned number" => 3,
                _ => 0,
            },
            "{case}: {writes:?}"
        );
    }
}

#[test]
fn publication_human_text_requires_provenance_and_preserves_bytes() {
    for valid in [false, true] {
        let (mut forge, config) = publication();
        let approved = reviews(&mut forge.replay).clone();
        let signed = body(&mut forge.replay, "/compare/")["commits"].clone();
        forge.replay = human_scenario();
        *reviews(&mut forge.replay) = approved;
        body(&mut forge.replay, "/compare/")["commits"] = signed;
        if !valid {
            activity(&mut forge.replay)[0]["actor"]["login"] = json!("someone-else");
        }
        let result = apply(&forge, &config, forge.replay.now, 3).unwrap();
        if valid {
            assert!(result[0].starts_with("promoted "));
            let writes = forge.writes.borrow();
            assert_eq!(writes[1].2["body"], forge.fresh["body"]);
            assert_eq!(writes[1].2["title"], forge.fresh["title"]);
        } else {
            assert!(result[0].contains("pusher"));
            assert!(forge.writes.borrow().is_empty());
        }
    }
}

#[test]
fn publication_caps_and_existing_receipt_are_bounded() {
    for cap in [0, 1, 2, 3, 100] {
        let (mut forge, config) = publication();
        comments(&mut forge.replay).as_array_mut().unwrap().push(json!({"id":99,"user":{"login":"cgwalters-bot"},"created_at":COMMAND,"updated_at":COMMAND,"body":"Previous attempt: outcome unknown. <!-- agent-board-promote-status -->"}));
        sync_metadata(&mut forge.replay);
        // The second candidate is never evaluated or published this invocation.
        let mut second = forge.fresh.clone();
        second["number"] = json!(35);
        body(&mut forge.replay, "pulls?state=open")
            .as_array_mut()
            .unwrap()
            .push(second);
        let lines = apply(&forge, &config, forge.replay.now, cap).unwrap();
        assert_eq!(lines.len(), 1);
        if cap < 3 {
            assert!(lines[0].starts_with("held by --max-writes"));
            assert!(forge.writes.borrow().is_empty());
        } else {
            let writes = forge.writes.borrow();
            assert_eq!(writes.len(), 3);
            assert!(writes[0].1.ends_with("/issues/comments/99"));
            assert!(writes[1].1.ends_with("/pulls"));
            assert!(writes[2].1.ends_with("/issues/comments/99"));
        }
    }
}

#[test]
fn confirmed_publication_does_not_starve_the_next_candidate() {
    let (mut forge, config) = publication();
    with_second(&mut forge);
    let first = apply(&forge, &config, forge.replay.now, 3).unwrap();
    assert!(first[0].contains("#34 ->"));
    // Retain what the fake forge accepted: upstream PR and final fork receipt.
    let writes = forge.writes.take();
    let mut published = upstream_pr();
    published["title"] = writes[1].2["title"].clone();
    published["body"] = writes[1].2["body"].clone();
    *upstream(&mut forge.replay) = json!([published]);
    comments(&mut forge.replay).as_array_mut().unwrap().push(json!({"id":99,"user":{"login":"cgwalters-bot"},"created_at":COMMAND,"updated_at":COMMAND,"body":writes[2].2["body"]}));
    sync_metadata(&mut forge.replay);
    let second = apply(&forge, &config, forge.replay.now, 3).unwrap();
    assert_eq!(
        second[0],
        format!("already promoted {REPO}#34 -> composefs/composefs-rs#70 head={HEAD}")
    );
    assert!(second[1].contains("#35 ->"), "{second:?}");
    assert_eq!(forge.writes.borrow().len(), 3);
    assert!(forge.writes.borrow()[0].1.contains("/issues/35/comments"));
}

#[test]
fn confirmed_rewritten_publication_is_not_rebuilt_after_an_upstream_advance() {
    let (mut forge, config) = publication();
    body(&mut forge.replay, "/compare/")["behind_by"] = json!(1);
    let mut git = FakeRewrite::new(&forge);
    let first = publish::apply_with(&forge, &config, forge.replay.now, 4, &mut git).unwrap();
    assert!(first[0].starts_with("promoted "), "{first:?}");
    let writes = forge.writes.take();
    keep_rewritten(&mut forge, &writes);
    advance_base(&mut forge);
    // The receipt decides: no rebuild, no writes, and the next candidate's turn.
    let second = publish::apply_with(&forge, &config, forge.replay.now, 4, &mut git).unwrap();
    assert_eq!(
        second,
        [format!(
            "already promoted {REPO}#34 -> composefs/composefs-rs#70 head={HEAD}"
        )]
    );
    assert_eq!(git.prepared.len(), 1);
    assert!(forge.writes.borrow().is_empty());
    // Changed fork text updates the published PR, still without a rebuild.
    let edited = json!(format!("Part of {REPO}#31\n\nEdited after publication."));
    forge.fresh["body"] = edited.clone();
    pull(&mut forge.replay)["body"] = edited.clone();
    let third = publish::apply_with(&forge, &config, forge.replay.now, 4, &mut git).unwrap();
    assert!(third[0].starts_with("promoted "), "{third:?}");
    assert_eq!(git.prepared.len(), 1);
    let writes = forge.writes.borrow();
    assert_eq!(writes.len(), 3);
    assert!(writes[0].1.ends_with("/issues/comments/99"));
    assert_eq!(
        (writes[1].0.as_str(), writes[1].1.as_str()),
        (
            "PATCH",
            &*format!("{API}repos/composefs/composefs-rs/pulls/70")
        )
    );
    assert_eq!(writes[1].2["body"], edited);
    let receipt = writes[2].2["body"].as_str().unwrap();
    let record = crate::plan::fenced_json(receipt, "agent-board-promoted")
        .unwrap()
        .unwrap();
    assert_eq!(record["published_oid"], FakeRewrite::head("b41c"));
    assert_eq!(record["published_branch"], rewrite::branch(HEAD));
}

#[test]
fn partial_publication_resumes_or_republishes_after_an_upstream_advance() {
    for case in [
        "pushed and created",
        "pushed only",
        "nothing pushed",
        "branch at another head",
        "unrecorded branch at another head",
        "record edited by another",
        "original branch published meanwhile",
    ] {
        let (mut forge, config) = publication();
        body(&mut forge.replay, "/compare/")["behind_by"] = json!(1);
        let mut git = FakeRewrite::new(&forge);
        // The branch is pushed, then the upstream POST's response is lost.
        forge.fail = Some(2);
        assert!(publish::apply_with(&forge, &config, forge.replay.now, 4, &mut git).is_err());
        let writes = forge.writes.take();
        let receipt = writes[2].2["body"].as_str().unwrap().to_owned();
        assert!(receipt.contains("was not confirmed"), "{case}: {receipt}");
        assert!(receipt.contains("```agent-board-promoting"), "{case}");
        let pushed = forge.promote_ref.borrow().clone().unwrap();
        assert_eq!(pushed, FakeRewrite::head("b41c"));
        let body = match case {
            "unrecorded branch at another head" => {
                format!(
                    "Promoting approved head {HEAD} to composefs/composefs-rs.\n\n{}",
                    handoff::STATUS
                )
            }
            _ => receipt,
        };
        comments(&mut forge.replay).as_array_mut().unwrap().push(json!({"id":99,"user":{"login":"cgwalters-bot"},"created_at":COMMAND,"updated_at":COMMAND,"body":body}));
        sync_metadata(&mut forge.replay);
        match case {
            "pushed and created" => {
                let mut pr = upstream_pr();
                pr["head"]["sha"] = json!(pushed);
                pr["head"]["ref"] = json!(rewrite::branch(HEAD));
                forge.promoted.borrow_mut().push(pr);
            }
            "nothing pushed" => *forge.promote_ref.borrow_mut() = None,
            "branch at another head" | "unrecorded branch at another head" => {
                *forge.promote_ref.borrow_mut() = Some("e".repeat(40))
            }
            "record edited by another" => {
                // A collaborator's edit of the bot's record steers nothing.
                let edited = "2026-10-23T01:00:00Z";
                comments(&mut forge.replay)[1]["updated_at"] = json!(edited);
                let metadata = &mut comment_metadata(&mut forge.replay)[1];
                metadata["lastEditedAt"] = json!(edited);
                metadata["editor"] = json!({"login":"visitor"});
            }
            "original branch published meanwhile" => {
                *upstream(&mut forge.replay) = json!([upstream_pr()]);
            }
            _ => {}
        }
        forge.fail = None;
        advance_base(&mut forge);
        let lines = publish::apply_with(&forge, &config, forge.replay.now, 4, &mut git).unwrap();
        let writes = forge.writes.borrow();
        let held = match case {
            // Never force-updated, and held before any write.
            "branch at another head" | "unrecorded branch at another head" => {
                Some("publication branch")
            }
            "record edited by another" => Some("untrusted handoff author/edit evidence"),
            "original branch published meanwhile" => {
                Some("existing upstream PR needs manual migration")
            }
            _ => None,
        };
        if let Some(why) = held {
            assert!(
                lines[0].starts_with(&format!("hold promote {REPO}#34: {why}")),
                "{case}: {lines:?}"
            );
            assert!(writes.is_empty(), "{case}");
            continue;
        }
        assert!(lines[0].starts_with("promoted "), "{case}: {lines:?}");
        assert_eq!(writes.len(), 3, "{case}");
        assert!(writes[0].1.ends_with("/issues/comments/99"), "{case}");
        let rebuilt = case == "nothing pushed";
        // A republication rebuilds on today's base under a new pending record.
        assert_eq!(git.prepared.len(), if rebuilt { 2 } else { 1 }, "{case}");
        let published = if rebuilt {
            FakeRewrite::head("d")
        } else {
            pushed.clone()
        };
        let pending = crate::plan::fenced_json_as(
            writes[0].2["body"].as_str().unwrap(),
            "agent-board-promoting",
            "record",
        )
        .unwrap()
        .unwrap();
        assert_eq!(pending["published_oid"], published, "{case}");
        assert_eq!(
            writes[1].0,
            if case == "pushed and created" {
                "PATCH"
            } else {
                "POST"
            },
            "{case}"
        );
        let record = crate::plan::fenced_json(
            writes[2].2["body"].as_str().unwrap(),
            "agent-board-promoted",
        )
        .unwrap()
        .unwrap();
        assert_eq!(record["published_oid"], published, "{case}");
        assert_eq!(record["published_branch"], rewrite::branch(HEAD), "{case}");
    }
}

#[test]
fn a_held_candidate_does_not_stop_the_next() {
    let (mut forge, config) = publication();
    with_second(&mut forge);
    // #34's fresh re-read disagrees with the listing; #35 is unaffected.
    forge.fresh["title"] = json!("Changed after approval");
    let lines = apply(&forge, &config, forge.replay.now, 3).unwrap();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].starts_with(&format!(
            "hold promote {REPO}#34: fork PR title changed before publication"
        )),
        "{lines:?}"
    );
    assert!(lines[1].contains("#35 ->"), "{lines:?}");
    let writes = forge.writes.borrow();
    assert_eq!(writes.len(), 3);
    assert!(writes[0].1.contains("/issues/35/comments"));
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

/// A synthetic live client over a scenario, not a recorded observation: a
/// 404 answer is the live client's typed absence, a read of `refuse` its
/// authentication failure, and a write is counted and refused.
struct Live<'a> {
    replay: &'a Replay,
    refuse: Option<&'a str>,
    writes: Cell<usize>,
}

impl Rest for Live<'_> {
    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
        if self
            .refuse
            .is_some_and(|path| url == format!("{API}{path}"))
        {
            return Err(anyhow::Error::from(crate::github::AuthenticationRequired)
                .context(format!("GET {url}: GitHub HTTP status 401")));
        }
        let response = self.replay.get(url, validator)?;
        if response.status == 404 {
            return Err(anyhow::Error::from(crate::github::NotFound)
                .context(format!("GET {url}: GitHub HTTP status 404")));
        }
        Ok(response)
    }
    fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
        self.writes.set(self.writes.get() + 1);
        anyhow::bail!("mutation attempted")
    }
}

impl Transport for Live<'_> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.replay.graphql(query, variables)
    }
}

/// Previews `replay` through [`Live`] and the CLI's recording, then replays
/// that recording at the same clock: both decisions, and the recording.
fn round_trip(
    replay: &Replay,
    config: &Config,
    refuse: Option<&str>,
) -> (Result<Vec<String>>, Result<Vec<String>>, Value) {
    let live = Live {
        replay,
        refuse,
        writes: Cell::new(0),
    };
    let recorder = crate::pass::Recorder::new(&live);
    let recorded = preview(&recorder, config, replay.now);
    assert_eq!(live.writes.get(), 0);
    let fixture: Value =
        serde_json::from_str(&recording(&recorder.fixture(replay.now).unwrap()).unwrap()).unwrap();
    let (replayed, _) = run(Replay::from_json(&fixture).unwrap(), config);
    (recorded, replayed, fixture)
}

/// The policy scan's optional repository, recorded as absent (404).
const ABSENT_SCAN: &str = "repos/composefs/.github/contents";

fn recorded_answer<'a>(fixture: &'a Value, path: &str) -> Option<&'a Value> {
    let url = format!("{API}{path}");
    fixture["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["request"]["get"] == url)
        .map(|e| &e["answer"])
}

#[test]
fn recording_replays_the_live_decision() {
    type Case = (&'static str, fn() -> Replay, bool, String);
    let cases: Vec<Case> = vec![
        ("bot-ok", scenario, false, candidate(false)),
        ("human-text", human_scenario, false, human_candidate()),
        (
            "human-text held",
            || {
                let mut replay = human_scenario();
                text_evidence(&mut replay)["editor"] = json!({"login":"visitor"});
                replay
            },
            false,
            policy_hold("needs-text: last body editor is not the operator or is missing"),
        ),
        (
            "policy missing",
            || {
                let mut replay = scenario();
                exact(&mut replay, RECORD)["answer"]["status"] = json!(404);
                replay
            },
            false,
            policy_hold(
                "policy missing: no record at cgwalters-forge/tracker:upstream-policy/composefs/composefs-rs.md",
            ),
        ),
        (
            "drafted after approval",
            || {
                let mut replay = scenario();
                *comments(&mut replay) = json!([]);
                sync_metadata(&mut replay);
                *reviews(&mut replay) = json!([{"id":5,"user":{"login":"cgwalters"},"state":"APPROVED","submitted_at":COMMAND,"commit_id":HEAD}]);
                pull(&mut replay)["draft"] = json!(true);
                timeline(
                    &mut replay,
                    json!([{"event":"convert_to_draft","created_at":"2026-10-24T00:00:00Z"}]),
                );
                replay
            },
            true,
            hold("no operator command or approval after observed push"),
        ),
    ];
    for (name, scenario, on_approval, expected) in cases {
        let mut config = Config::parse(CONFIG).unwrap();
        config.promote.as_mut().unwrap().on_approval = on_approval;
        let replay = scenario();
        let (recorded, replayed, fixture) = round_trip(&replay, &config, None);
        let expected = vec![expected, REMAINING.into()];
        assert_eq!(recorded.unwrap(), expected, "{name}");
        assert_eq!(replayed.unwrap(), expected, "{name}");
        // Typed absence of a missing record, or of an optional scan source
        // once the record is read; the approval hold comes before both.
        let absent = if name == "policy missing" {
            RECORD
        } else {
            ABSENT_SCAN
        };
        if !on_approval {
            assert_eq!(
                recorded_answer(&fixture, absent),
                Some(&json!({"absent":true})),
                "{name}"
            );
        }
    }
}

/// A transport or authentication failure is neither absence nor recorded:
/// its replay fails on the unrecorded read instead of passing a gate.
#[test]
fn recording_keeps_no_failure() {
    let config = Config::parse(CONFIG).unwrap();
    // A tree of the record's history, read after the policy scan.
    let tree = format!("{TRACKER}/git/trees/{}", "2".repeat(40));
    let (recorded, replayed, fixture) = round_trip(&scenario(), &config, Some(&tree));
    let recorded = recorded.unwrap();
    assert!(recorded[0].starts_with("hold promote"), "{recorded:?}");
    assert!(recorded[0].contains("status 401"), "{recorded:?}");
    assert_eq!(recorded_answer(&fixture, &tree), None);
    // Every kept REST answer is a response or typed absence, never a failure.
    for e in fixture["requests"].as_array().unwrap() {
        let answer = &e["answer"];
        assert!(answer.get("error").is_none(), "{e}");
        if e["request"]["get"].is_string() {
            assert!(
                answer["status"].is_u64() || *answer == json!({"absent":true}),
                "{e}"
            );
        }
    }
    let replayed = replayed.unwrap();
    assert!(
        replayed[0].contains("the replay did not record"),
        "{replayed:?}"
    );
    // Replay itself gives back the live client's type, not a status.
    let replay = Replay::from_json(&fixture).unwrap();
    let Err(error) = replay.get(&format!("{API}{ABSENT_SCAN}"), None) else {
        panic!("recorded absence replayed as a response");
    };
    assert!(error.is::<crate::github::NotFound>(), "{error:#}");
}

#[test]
fn on_approval_accepts_a_current_head_approval_after_the_push() {
    type Case = (&'static str, fn(&mut Replay), Option<&'static str>);
    let none = "no operator command or approval after observed push";
    let approve = |r: &mut Replay| {
        *comments(r) = json!([]);
        *reviews(r) = json!([{"id":5,"user":{"login":"cgwalters"},"state":"APPROVED","submitted_at":COMMAND,"commit_id":HEAD}]);
        pull(r)["draft"] = json!(true);
        timeline(r, json!([{"event": "labeled", "created_at": PUSH}]));
    };
    // Draft, ready at the push, and converted to draft again at `at`.
    fn drafted(r: &mut Replay, at: &str) {
        r.requests.pop();
        timeline(
            r,
            json!([{"event": "convert_to_draft", "created_at": "2026-10-22T00:00:00Z"}, {"event": "ready_for_review", "created_at": PUSH}, {"event": "convert_to_draft", "created_at": at}]),
        );
    }
    let cases: Vec<Case> = vec![
        ("approved draft", |_| {}, None),
        // Approving a ready fork PR may mean merging it into the fork alone.
        ("ready", |r| pull(r)["draft"] = json!(false), Some(none)),
        // ... and must not count once the PR is converted to draft later.
        (
            "ready, approved, then draft",
            |r| drafted(r, "2026-10-24T00:00:00Z"),
            Some(none),
        ),
        (
            "draft before approval",
            |r| drafted(r, "2026-10-22T12:00:00Z"),
            None,
        ),
        (
            "simultaneous draft",
            |r| drafted(r, COMMAND),
            Some("approval and draft conversion have ambiguous ordering"),
        ),
        (
            "stale command",
            |r| {
                *comments(r) = json!([{"id": 1, "user": {"login": "cgwalters"}, "created_at": "2026-10-21T00:00:00Z", "updated_at": "2026-10-21T00:00:00Z", "body": "/promote"}]);
            },
            None,
        ),
        (
            "comment-only review after",
            |r| {
                reviews(r).as_array_mut().unwrap().push(json!({"id":6,"user":{"login":"cgwalters"},"state":"COMMENTED","submitted_at":"2026-10-24T00:00:00Z","commit_id":HEAD}));
            },
            None,
        ),
        (
            "command wins",
            |r| {
                *comments(r) = json!([{"id": 1, "user": {"login": "cgwalters"}, "created_at": COMMAND, "updated_at": COMMAND, "body": "/promote"}]);
            },
            Some("command"),
        ),
        (
            "before push",
            |r| reviews(r)[0]["submitted_at"] = json!("2026-10-21T00:00:00Z"),
            Some(none),
        ),
        (
            "simultaneous push",
            |r| reviews(r)[0]["submitted_at"] = json!(PUSH),
            Some("approval and push have ambiguous ordering"),
        ),
        (
            "old head",
            |r| reviews(r)[0]["commit_id"] = json!("a".repeat(40)),
            Some(none),
        ),
        (
            "other reviewer",
            |r| reviews(r)[0]["user"]["login"] = json!("visitor"),
            Some(none),
        ),
        (
            "later change request",
            |r| {
                reviews(r).as_array_mut().unwrap().push(json!({"id":6,"user":{"login":"cgwalters"},"state":"CHANGES_REQUESTED","submitted_at":"2026-10-24T00:00:00Z","commit_id":HEAD}));
            },
            Some(none),
        ),
    ];
    let mut config = Config::parse(CONFIG).unwrap();
    config.promote.as_mut().unwrap().on_approval = true;
    let review = format!("https://github.com/{REPO}/pull/34#pullrequestreview-5");
    for (name, edit, held) in cases {
        let mut replay = scenario();
        approve(&mut replay);
        edit(&mut replay);
        sync_metadata(&mut replay);
        let (result, _) = run(replay, &config);
        let expected = match held {
            None => candidate(false).replace(
                &format!("https://github.com/{REPO}/pull/34#issuecomment-1"),
                &review,
            ),
            Some("command") => candidate(false),
            Some(reason) => hold(reason),
        };
        assert_eq!(result.unwrap(), [expected, REMAINING.into()], "{name}");
    }
    // Off, the same approval is no trigger and a draft still holds.
    let config = Config::parse(CONFIG).unwrap();
    let mut replay = scenario();
    approve(&mut replay);
    sync_metadata(&mut replay);
    assert_eq!(
        run(replay, &config).0.unwrap(),
        [hold("draft"), REMAINING.into()]
    );
    // Publication of an approved draft without a command, still as a draft.
    let (mut forge, mut config) = publication();
    config.promote.as_mut().unwrap().on_approval = true;
    *comments(&mut forge.replay) = json!([]);
    sync_metadata(&mut forge.replay);
    pull(&mut forge.replay)["draft"] = json!(true);
    forge.fresh["draft"] = json!(true);
    timeline(&mut forge.replay, json!([]));
    let lines = apply(&forge, &config, forge.replay.now, 3).unwrap();
    assert_eq!(
        lines,
        [format!(
            "promoted {REPO}#34 -> https://github.com/composefs/composefs-rs/pull/70 head={HEAD}"
        )]
    );
    let writes = forge.writes.borrow();
    let created = writes
        .iter()
        .find(|(method, url, _)| method == "POST" && url.ends_with("composefs-rs/pulls"))
        .unwrap();
    assert_eq!(created.2["draft"], true);
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
fn human_text_uses_the_pusher_returning_to_an_old_oid() {
    let config = Config::parse(CONFIG).unwrap();
    let pushed = "2026-10-22T12:00:00Z";
    for pusher in [Some("cgwalters"), Some("cgwalters-bot"), None] {
        let mut replay = human_scenario();
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
        back["actor"] = pusher.map_or(Value::Null, |login| json!({"login":login}));
        *activity(&mut replay) = json!([back, away, original]);
        let expected = if pusher == Some("cgwalters") {
            human_candidate().replace(PUSH, pushed)
        } else {
            policy_hold("needs-text: current-head pusher is not the operator or is missing")
        };
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
