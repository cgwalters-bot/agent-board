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
        "would promote {REPO}#34 -> composefs/composefs-rs (sign-off on 1 of 1 commits) (policy bot-ok); remaining gates: publication, first-write revalidation, unchanged-text publication; no writes implemented; stack=none (no plan); dco=required; head={HEAD} command=https://github.com/{REPO}/pull/34#issuecomment-1 at={COMMAND} human-text={human} push={PUSH}; staging verification required for bot-fork head"
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
    json!({"number":70,"state":"open","merged_at":null,"base":{"ref":"main"},"head":{"sha":HEAD,"ref":"dispatch/implement/agent-run-39714560993","repo":{"owner":{"login":"cgwalters-bot"}}}})
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
            gate_hold("needs rebase and a fresh /promote (behind_by=1)"),
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
