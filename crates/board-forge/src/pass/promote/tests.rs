use super::*;
use crate::pass::tests::{answer, preview};

const CONFIG: &str = include_str!("../../../../../fixtures/pass/promote-config.json");
/// A fork pull request approved on its head, with a `bot-ok` policy record
/// whose source is current and an upstream that requires DCO, as a
/// `pass --record` of the stage would keep it: the shapes are GitHub's, the
/// values synthetic, since the recording could not be made where this was
/// written (no network); the first live `--record` replaces it.
const FIXTURE: &str = include_str!("../../../../../fixtures/pass/promote-2026-10-24.json");

const FORK: &str = "cgwalters-forge/bootc";
const UPSTREAM: &str = "bootc-dev/bootc";
const BRANCH: &str = "dispatch/1030-root-filesystem";
const BASE: &str = "1111111111111111111111111111111111111111";
const FIRST: &str = "2222222222222222222222222222222222222222";
const HEAD: &str = "3333333333333333333333333333333333333333";
const OLDER: &str = "4444444444444444444444444444444444444444";
const TRAILER: &str = "Signed-off-by: Operator Example <operator@example.com>";
const COUNT: &str = "promote: 1 approved of 2 open pull request(s) of cgwalters-bot in 1 fork(s)";
const WOULD: &str = "promote cgwalters-forge/bootc#5 -> bootc-dev/bootc: would open a PR (sign-off on 1 of 2 commits)";

fn config(edit: Value) -> Config {
    let mut value: Value = serde_json::from_str(CONFIG).unwrap();
    if let Some(edits) = edit.as_object() {
        for (key, item) in edits {
            match (value.get_mut(key), item.as_object()) {
                (Some(Value::Object(section)), Some(items)) => {
                    section.extend(items.clone());
                }
                _ => {
                    value[key] = item.clone();
                }
            }
        }
    }
    Config::parse(&value.to_string()).unwrap()
}

fn replay() -> Replay {
    Replay::from_json(&serde_json::from_str(FIXTURE).unwrap()).unwrap()
}

/// The stage's lines.
fn stage(report: &Report) -> Vec<&str> {
    (report.lines.iter().map(String::as_str))
        .filter(|l| l.contains("promote") || l.contains("notice"))
        .collect()
}

fn get(url: &str, answer: Value) -> Value {
    json!({"request": {"get": format!("{API_ROOT}{url}")},
        "answer": {"status": 200, "link": null, "body": answer}})
}

fn graphql(query: &str, variables: Value, data: Value) -> Value {
    json!({"request": graphql_key(query, &variables), "answer": {"data": data}})
}

fn review(id: u64, who: &str, state: &str, commit: &str, body: &str, at: &str) -> Value {
    json!({"id": id, "user": {"login": who}, "state": state, "commit_id": commit, "body": body,
        "submitted_at": at, "html_url": format!("https://github.com/{FORK}/pull/5#pullrequestreview-{id}")})
}

fn reviews(replay: &mut Replay, list: Vec<Value>) {
    answer(replay, "pulls/5/reviews")["body"] = list.into();
}

/// Base64 of `text`, as the contents API answers a file.
fn encode(text: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut buffer = [0u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let n = u32::from_be_bytes([0, buffer[0], buffer[1], buffer[2]]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn record_text(verdict: &str) -> String {
    format!(
        "---\nverdict: {verdict}\nsources:\n  - repo: {UPSTREAM}\n    path: CONTRIBUTING.md\n    sha: cccccccccccccccccccccccccccccccccccccccc\n---\nThe project's rules, quoted.\n"
    )
}

fn set_record(replay: &mut Replay, text: &str) {
    answer(replay, "contents/upstream-policy/bootc-dev/bootc.md")["body"]["content"] =
        encode(text).into();
}

/// The upstream wants no DCO: no rule, no check run on its base or on its
/// last pull requests.
fn no_dco(replay: &mut Replay) {
    answer(replay, "rules/branches/main")["body"] = json!([]);
    replay.requests.push(get(
        &format!("repos/{UPSTREAM}/commits/main/check-runs?per_page=100"),
        json!({"total_count": 1, "check_runs": [{"name": "build", "app": {"slug": "github-actions"}}]}),
    ));
    replay.requests.push(get(
        &format!(
            "repos/{UPSTREAM}/pulls?state=all&sort=created&direction=desc&per_page={DCO_PULLS}"
        ),
        json!([{"number": 2599, "head": {"sha": "9999999999999999999999999999999999999999"}}]),
    ));
    replay.requests.push(get(
        &format!("repos/{UPSTREAM}/commits/9999999999999999999999999999999999999999/check-runs?per_page=100"),
        json!({"total_count": 0, "check_runs": []}),
    ));
}

/// The policy is `human-text`, with who GitHub shows pushed the head,
/// last edited the body and last renamed the title, before the approval.
fn human_text(replay: &mut Replay, pusher: &str, editor: &str, renamer: &str) {
    set_record(replay, &record_text("human-text"));
    replay.requests.push(get(
        &format!("repos/cgwalters-bot/bootc/activity?ref=refs/heads/{BRANCH}&per_page=100"),
        json!([{"before": FIRST, "after": HEAD, "activity_type": "force_push", "actor": {"login": pusher}}]),
    ));
    let renamed = match renamer.is_empty() {
        true => json!([]),
        false => json!([{"createdAt": "2026-10-24T09:59:00Z", "actor": {"login": renamer}}]),
    };
    *text_answer(replay) = json!({"data": {"repository": {"pullRequest": {
        "lastEditedAt": "2026-10-24T10:00:00Z", "editor": {"login": editor},
        "timelineItems": {"nodes": renamed}}}}});
}

/// The recorded answer about who last changed the pull request's text.
fn text_answer(replay: &mut Replay) -> &mut Value {
    answer(replay, &format!(r#""graphql":"{}""#, fnv(TEXT_QUERY)))
}

/// The body without the bot's line, with `--human-text` in the approval.
fn human_written(replay: &mut Replay) {
    let pulls = &mut answer(replay, "pulls?state=open")["body"];
    pulls[0]["body"] = "The operator's own words.\n\nCloses bootc-dev/bootc#1030.".into();
    reviews(
        replay,
        vec![review(
            3400000001,
            "cgwalters",
            "APPROVED",
            HEAD,
            "Fine.\n--human-text",
            "2026-10-24T10:12:00Z",
        )],
    );
}

/// A plan on bootc-dev/bootc#1030 that stacks on cgwalters-forge/bootc#1029,
/// whose pull requests are `parents`, and the upstream pull requests from
/// the lower branch `upstream`.
fn stacked(replay: &mut Replay, parents: Vec<Value>, upstream: Vec<Value>) {
    let plan = json!({"schema": JOB_PLAN_SCHEMA, "repo": FORK, "item": 1030, "kind": "implement",
        "runner": "hosted", "brief": "Read the type.", "stacks_on": 1029});
    let comments = answer(replay, r#""number":1030,"owner":"bootc-dev""#);
    comments["data"]["repository"]["issue"]["comments"]["nodes"] = json!([{
        "url": "https://github.com/bootc-dev/bootc/issues/1030#issuecomment-1",
        "body": format!("```{PLAN_FENCE}\n{plan}\n```"),
        "createdAt": "2026-10-22T00:00:00Z", "lastEditedAt": null,
        "author": {"login": "cgwalters"}, "editor": null,
    }]);
    replay.requests.push(graphql(
        LOWER_QUERY,
        json!({"owner": "cgwalters-forge", "name": "bootc", "number": 1029}),
        json!({"repository": {"issue": {"closedByPullRequestsReferences": {
            "nodes": parents.clone(), "totalCount": parents.len()}}}}),
    ));
    replay.requests.push(get(
        &format!(
            "repos/{UPSTREAM}/pulls?state=all&head=cgwalters-bot:dispatch/1029-lower&per_page=10"
        ),
        upstream.into(),
    ));
}

fn parent(state: &str) -> Value {
    json!({"number": 4, "state": state, "url": format!("https://github.com/{FORK}/pull/4"),
        "headRefName": "dispatch/1029-lower", "headRepository": {"nameWithOwner": "cgwalters-bot/bootc"},
        "author": {"login": "cgwalters-bot"}, "repository": {"nameWithOwner": FORK}})
}

fn upstream_pull(number: u64, state: &str, merged: bool, head: &str) -> Value {
    json!({"number": number, "state": state, "html_url": format!("https://github.com/{UPSTREAM}/pull/{number}"),
        "merged_at": if merged { json!("2026-10-23T00:00:00Z") } else { Value::Null },
        "head": {"sha": head}})
}

/// The recorded preview: the one approved pull request would open a pull
/// request with the sign-off on the commit that lacks it, five writes.
#[test]
fn replay_previews_the_promotion() {
    let report = preview(&config(json!({})), &replay());
    assert_eq!(stage(&report), [COUNT, WOULD]);
    assert_eq!(report.summary, "summary: dispatched 0, writes 5, held 0");
    assert_eq!(report.failures, 0);
}

type Edit = Box<dyn Fn(&mut Replay)>;

/// Each edit of the recording, the stage's lines (each the start of one),
/// the writes a preview plans and how many operations fail.
#[test]
fn replay_scenarios() {
    let hold = |why: &str| format!("hold promote cgwalters-forge/bootc#5: {why}");
    let stale = "hold promote cgwalters-forge/bootc#5: cgwalters-forge/tracker's upstream-policy/bootc-dev/bootc.md is stale: ";
    let cases: Vec<(&str, Edit, Vec<String>, usize, usize)> = vec![
        (
            "approval of an older head: head-moved, one notice",
            Box::new(|r| reviews(r, vec![review(1, "cgwalters", "APPROVED", OLDER, "", "2026-10-24T10:12:00Z")])),
            vec![COUNT.into(), hold("cgwalters approved 4444444, but the head is now 3333333")],
            1,
            0,
        ),
        (
            "approval dismissed",
            Box::new(|r| reviews(r, vec![review(1, "cgwalters", "DISMISSED", HEAD, "", "2026-10-24T10:12:00Z")])),
            vec!["promote: 0 approved of 2 open".into()],
            0,
            0,
        ),
        (
            "a later change request cancels the approval",
            Box::new(|r| {
                reviews(r, vec![
                    review(1, "cgwalters", "APPROVED", HEAD, "", "2026-10-24T10:12:00Z"),
                    review(2, "cgwalters", "CHANGES_REQUESTED", HEAD, "Not yet.", "2026-10-24T10:20:00Z"),
                ])
            }),
            vec!["promote: 0 approved of 2 open".into()],
            0,
            0,
        ),
        (
            "a comment review after the approval leaves it",
            Box::new(|r| {
                reviews(r, vec![
                    review(1, "cgwalters", "APPROVED", HEAD, "", "2026-10-24T10:12:00Z"),
                    review(2, "cgwalters", "COMMENTED", HEAD, "One more thought.", "2026-10-24T10:20:00Z"),
                ])
            }),
            vec![COUNT.into(), WOULD.into()],
            5,
            0,
        ),
        (
            "an approval by anyone but the operator is ignored",
            Box::new(|r| reviews(r, vec![review(1, "someone", "APPROVED", HEAD, "", "2026-10-24T10:12:00Z")])),
            vec!["promote: 0 approved of 2 open".into()],
            0,
            0,
        ),
        (
            "a draft is never promoted",
            Box::new(|r| answer(r, "pulls?state=open")["body"][0]["draft"] = true.into()),
            vec!["promote: 0 approved of 1 open".into()],
            0,
            0,
        ),
        (
            "needs-human holds it",
            Box::new(|r| {
                answer(r, "pulls?state=open")["body"][0]["labels"] = json!([{"name": "needs-human"}]);
            }),
            vec!["promote: 0 approved of 1 open".into()],
            0,
            0,
        ),
        (
            "policy record missing",
            Box::new(|r| answer(r, "contents/upstream-policy/bootc-dev/bootc.md")["status"] = 404.into()),
            vec![hold("no contribution-policy record for bootc-dev/bootc at cgwalters-forge/tracker's upstream-policy/bootc-dev/bootc.md on main")],
            1,
            0,
        ),
        (
            "policy record stale: a source changed",
            Box::new(|r| {
                answer(r, "contents/CONTRIBUTING.md")["body"]["sha"] =
                    "ffffffffffffffffffffffffffffffffffffffff".into();
            }),
            vec![format!("{stale}bootc-dev/bootc's CONTRIBUTING.md changed from ccccccc to fffffff")],
            1,
            0,
        ),
        (
            "policy record stale: a contribution file it does not list",
            Box::new(|r| {
                answer(r, "contents/docs")["body"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"name": "AI-POLICY.md", "path": "docs/AI-POLICY.md", "type": "file"}));
            }),
            vec![format!("{stale}bootc-dev/bootc's docs/AI-POLICY.md is not in the record")],
            1,
            0,
        ),
        (
            "policy record invalid",
            Box::new(|r| set_record(r, &record_text("maybe"))),
            vec![hold("cgwalters-forge/tracker's upstream-policy/bootc-dev/bootc.md is invalid: verdict \"maybe\" is not one of bot-ok, human-text, human-only, no-go")],
            1,
            0,
        ),
        (
            "policy record not a file",
            Box::new(|r| answer(r, "contents/upstream-policy/bootc-dev/bootc.md")["body"]["type"] = "symlink".into()),
            vec![hold("cgwalters-forge/tracker's upstream-policy/bootc-dev/bootc.md is not a regular file")],
            1,
            0,
        ),
        (
            "a verdict loosened in a commit by someone other than the operator, whatever its author says",
            Box::new(|r| {
                let older = "abababababababababababababababababababab";
                answer(r, "commits?path=upstream-policy")["body"] = json!([
                    {"sha": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee", "author": {"login": "cgwalters"},
                        "committer": {"login": "someone"}, "commit": {"verification": {"verified": true}}},
                    {"sha": older, "committer": {"login": "cgwalters"}, "commit": {"verification": {"verified": true}}},
                ]);
                r.requests.push(get(
                    &format!("repos/cgwalters-forge/tracker/contents/upstream-policy/bootc-dev/bootc.md?ref={older}"),
                    json!({"type": "file", "content": encode(&record_text("human-only"))}),
                ));
            }),
            vec![hold("upstream-policy/bootc-dev/bootc.md's verdict was loosened from human-only to bot-ok in eeeeeee committed by someone, not by cgwalters in a commit GitHub verified")],
            1,
            0,
        ),
        (
            "a verdict loosened by the operator, verified",
            Box::new(|r| {
                let older = "abababababababababababababababababababab";
                answer(r, "commits?path=upstream-policy")["body"] = json!([
                    {"sha": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee", "committer": {"login": "cgwalters"},
                        "commit": {"verification": {"verified": true}}},
                    {"sha": older, "committer": {"login": "cgwalters"}, "commit": {"verification": {"verified": true}}},
                ]);
                r.requests.push(get(
                    &format!("repos/cgwalters-forge/tracker/contents/upstream-policy/bootc-dev/bootc.md?ref={older}"),
                    json!({"type": "file", "content": encode(&record_text("no-go"))}),
                ));
            }),
            vec![WOULD.into()],
            5,
            0,
        ),
        (
            "no-go: held, quoting the record once",
            Box::new(|r| set_record(r, &record_text("no-go"))),
            vec![hold("bootc-dev/bootc's contribution policy is no-go: the bot may not open pull requests there")],
            1,
            0,
        ),
        (
            "human-text without the marker, the bot's text",
            Box::new(|r| human_text(r, "cgwalters-bot", "cgwalters-bot", "")),
            vec![hold("bootc-dev/bootc's contribution policy wants a person's text, and the approval does not say `--human-text`; the description still has the bot's Generated-by line; GitHub shows cgwalters-bot pushed the head, not cgwalters; the description was last edited by cgwalters-bot, not cgwalters; the title was last set by the bot, not cgwalters")],
            1,
            0,
        ),
        (
            "human-text with the marker, someone else pushed",
            Box::new(|r| {
                human_text(r, "someone", "cgwalters", "cgwalters");
                human_written(r);
            }),
            vec![hold("bootc-dev/bootc's contribution policy wants a person's text, and GitHub shows someone pushed the head, not cgwalters")],
            1,
            0,
        ),
        (
            "human-text with the marker and the operator's text",
            Box::new(|r| {
                human_text(r, "cgwalters", "cgwalters", "cgwalters");
                human_written(r);
            }),
            vec![WOULD.into()],
            5,
            0,
        ),
        (
            "stacked on an open fork pull request",
            Box::new(|r| stacked(r, vec![parent("OPEN")], vec![])),
            vec![hold("it stacks on cgwalters-forge/bootc#1029, whose pull request https://github.com/cgwalters-forge/bootc/pull/4 is still open")],
            1,
            0,
        ),
        (
            "stacked on a slice with an open upstream pull request",
            Box::new(|r| stacked(r, vec![parent("CLOSED")], vec![upstream_pull(2500, "open", false, FIRST)])),
            vec![hold("it stacks on cgwalters-forge/bootc#1029, whose upstream pull request https://github.com/bootc-dev/bootc/pull/2500 is not merged yet")],
            1,
            0,
        ),
        (
            "stacked on a slice closed with nothing upstream",
            Box::new(|r| stacked(r, vec![parent("CLOSED")], vec![upstream_pull(2500, "closed", false, FIRST)])),
            vec![hold("it stacks on cgwalters-forge/bootc#1029, whose pull request https://github.com/cgwalters-forge/bootc/pull/4 was closed with nothing merged upstream")],
            1,
            0,
        ),
        (
            "stacked on a slice merged upstream: goes ahead",
            Box::new(|r| stacked(r, vec![parent("CLOSED")], vec![upstream_pull(2500, "closed", true, FIRST)])),
            vec![WOULD.into()],
            5,
            0,
        ),
        (
            "two closing issues: the first's lower slice merged, the second's still open",
            Box::new(|r| {
                stacked(r, vec![parent("CLOSED")], vec![upstream_pull(2500, "closed", true, FIRST)]);
                let closing = &mut answer(r, &fnv(CLOSING_QUERY))["data"]["repository"]["pullRequest"]["closingIssuesReferences"];
                closing["nodes"].as_array_mut().unwrap().push(json!({"number": 1041, "repository": {"nameWithOwner": UPSTREAM}}));
                closing["totalCount"] = 2.into();
                let mut second = (r.requests.iter())
                    .find(|e| e["request"].to_string().contains(r#""number":1030,"owner":"bootc-dev""#))
                    .unwrap()
                    .clone();
                second["request"]["variables"]["number"] = 1041.into();
                let plan = json!({"schema": JOB_PLAN_SCHEMA, "repo": FORK, "item": 1041, "kind": "implement",
                    "runner": "hosted", "brief": "Read the type.", "stacks_on": 1040});
                second["answer"]["data"]["repository"]["issue"]["comments"]["nodes"][0]["body"] =
                    format!("```{PLAN_FENCE}\n{plan}\n```").into();
                r.requests.push(second);
                let mut lower = parent("OPEN");
                lower["number"] = 9.into();
                lower["url"] = format!("https://github.com/{FORK}/pull/9").into();
                r.requests.push(graphql(
                    LOWER_QUERY,
                    json!({"owner": "cgwalters-forge", "name": "bootc", "number": 1040}),
                    json!({"repository": {"issue": {"closedByPullRequestsReferences": {
                        "nodes": [lower], "totalCount": 1}}}}),
                ));
            }),
            vec![hold("it stacks on cgwalters-forge/bootc#1040, whose pull request https://github.com/cgwalters-forge/bootc/pull/9 is still open")],
            1,
            0,
        ),
        (
            "upstream pull request open with the same head: link and close",
            Box::new(|r| {
                answer(r, "pulls?state=all&head=cgwalters-bot:dispatch/1030")["body"] =
                    json!([upstream_pull(2600, "open", false, HEAD)]);
            }),
            vec!["promote cgwalters-forge/bootc#5 -> bootc-dev/bootc: https://github.com/bootc-dev/bootc/pull/2600 is open already; would link and close".into()],
            2,
            0,
        ),
        (
            "upstream pull request open with another head",
            Box::new(|r| {
                answer(r, "pulls?state=all&head=cgwalters-bot:dispatch/1030")["body"] =
                    json!([upstream_pull(2600, "open", false, OLDER)]);
            }),
            vec![hold("https://github.com/bootc-dev/bootc/pull/2600 is open from this branch with head 4444444, not the approved 3333333")],
            1,
            0,
        ),
        (
            "upstream pull request from this branch closed",
            Box::new(|r| {
                answer(r, "pulls?state=all&head=cgwalters-bot:dispatch/1030")["body"] =
                    json!([upstream_pull(2600, "closed", false, HEAD)]);
            }),
            vec![hold("https://github.com/bootc-dev/bootc/pull/2600 from this branch was closed upstream; it is not reopened")],
            1,
            0,
        ),
        (
            "behind upstream: no rebase here",
            Box::new(|r| answer(r, "compare/main")["body"]["behind_by"] = 3.into()),
            vec![hold("3 commit(s) behind bootc-dev/bootc main; this stage does not rebase")],
            1,
            0,
        ),
        (
            "zero commits ahead",
            Box::new(|r| {
                let compare = &mut answer(r, "compare/main")["body"];
                compare["ahead_by"] = 0.into();
                compare["commits"] = json!([]);
            }),
            vec![hold("no commits ahead of bootc-dev/bootc main; merged already?")],
            1,
            0,
        ),
        (
            "the branch moved between the pull request read and the compare",
            Box::new(|r| answer(r, "compare/main")["body"]["commits"][1]["sha"] = OLDER.into()),
            vec!["promote error: cgwalters-forge/bootc#5: cgwalters-forge/bootc#5's branch is at 4444444, not its head 3333333: it moved while being read".into()],
            0,
            1,
        ),
        (
            "a merge commit in a series that needs a sign-off",
            Box::new(|r| {
                answer(r, "compare/main")["body"]["commits"][1]["parents"] =
                    json!([{"sha": FIRST}, {"sha": OLDER}]);
            }),
            vec![hold("commit 3333333 is a merge commit, which the sign-off cannot rewrite")],
            1,
            0,
        ),
        (
            "an already signed merge commit after one that needs a sign-off",
            Box::new(|r| {
                let commits = &mut answer(r, "compare/main")["body"]["commits"];
                commits[0]["commit"]["message"] = "install: Read the root filesystem type\n\nGenerated-by: https://github.com/cgwalters/#llms".into();
                commits[1]["commit"]["message"] = format!("Merge main\n\n{TRAILER}").into();
                commits[1]["parents"] = json!([{"sha": FIRST}, {"sha": OLDER}]);
            }),
            vec![hold("commit 3333333 is a merge commit, which the sign-off cannot rewrite")],
            1,
            0,
        ),
        (
            "a third party's commit without the sign-off",
            Box::new(|r| {
                let commit = &mut answer(r, "compare/main")["body"]["commits"][1];
                commit["author"] = json!({"login": "visitor"});
                commit["commit"]["author"] = json!({"name": "A Visitor", "email": "visitor@example.org", "date": "2026-10-22T18:01:00Z"});
            }),
            vec![hold("commit 3333333 by A Visitor <visitor@example.org> lacks the operator's sign-off, which only the bot's and the operator's commits get")],
            1,
            0,
        ),
        (
            "no DCO upstream: nothing is rewritten",
            Box::new(no_dco),
            vec!["promote cgwalters-forge/bootc#5 -> bootc-dev/bootc: would open a PR (no sign-off needed)".into()],
            3,
            0,
        ),
        (
            "no DCO upstream, with more pull requests than its sample",
            Box::new(|r| {
                no_dco(r);
                answer(r, "sort=created&direction=desc")["link"] =
                    r#"<https://api.github.com/repositories/1/pulls?state=all&page=2>; rel="next""#.into();
            }),
            vec!["promote cgwalters-forge/bootc#5 -> bootc-dev/bootc: would open a PR (no sign-off needed)".into()],
            3,
            0,
        ),
        (
            "a bare #N in the text",
            Box::new(|r| {
                answer(r, "pulls?state=open")["body"][0]["body"] = "Closes #1030.".into();
            }),
            vec![hold("its text refers to #1030, which bootc-dev/bootc would read as its own issue")],
            1,
            0,
        ),
        (
            "an empty description",
            Box::new(|r| answer(r, "pulls?state=open")["body"][0]["body"] = Value::Null),
            vec![hold("its title or description is empty")],
            1,
            0,
        ),
        (
            "the fork's parent is not the configured upstream",
            Box::new(|r| {
                let repo = &mut answer(r, "repos/cgwalters-forge/bootc\"")["body"];
                repo["parent"]["full_name"] = "other/bootc".into();
                repo["source"]["full_name"] = "other/bootc".into();
            }),
            vec!["promote error: cgwalters-forge/bootc: its parent is other/bootc and its network root other/bootc, not bootc-dev/bootc".into()],
            0,
            1,
        ),
        (
            "the head's repository is outside the upstream's network",
            Box::new(|r| {
                let repo = &mut answer(r, "repos/cgwalters-bot/bootc\"")["body"];
                repo["parent"]["full_name"] = "other/bootc".into();
                repo["source"]["full_name"] = "other/bootc".into();
            }),
            vec![hold("its head is in cgwalters-bot/bootc, which is not a fork of bootc-dev/bootc")],
            1,
            0,
        ),
        (
            "the upstream is archived",
            Box::new(|r| answer(r, "repos/bootc-dev/bootc\"")["body"]["archived"] = true.into()),
            vec![hold("bootc-dev/bootc is archived")],
            1,
            0,
        ),
        (
            "a notice said already is not said again",
            Box::new(|r| {
                reviews(r, vec![review(1, "cgwalters", "APPROVED", OLDER, "", "2026-10-24T10:12:00Z")]);
                answer(r, "issues/5/comments")["body"] = json!([{"id": 1, "html_url": "u",
                    "user": {"login": "cgwalters-bot"}, "body": format!("Not promoted.\n{}", marker(&notice_key(HEAD_MOVED, 1, HEAD)))}]);
            }),
            vec![hold("cgwalters approved 4444444, but the head is now 3333333")],
            0,
            0,
        ),
        (
            "refused already for this approval and head: not tried again",
            Box::new(|r| {
                answer(r, "issues/5/comments")["body"] = json!([{"id": 1, "html_url": "https://github.com/cgwalters-forge/bootc/pull/5#issuecomment-1",
                    "user": {"login": "cgwalters-bot"}, "created_at": "2026-10-24T09:00:00Z", "updated_at": "2026-10-24T09:00:00Z",
                    "body": format!("Not promoted.\n{}", marker(&notice_key(REFUSED, 3400000001, HEAD)))}]);
            }),
            vec![hold("refused already for this approval and head, https://github.com/cgwalters-forge/bootc/pull/5#issuecomment-1; a new approval tries again")],
            0,
            0,
        ),
        (
            "a second approved pull request waits, in order of approval",
            Box::new(|r| {
                answer(r, "pulls/6/reviews")["body"] =
                    json!([review(2, "cgwalters", "APPROVED", "6666666666666666666666666666666666666666", "", "2026-10-24T11:00:00Z")]);
            }),
            vec![
                "promote: 2 approved of 2 open".into(),
                WOULD.into(),
                "hold promote cgwalters-forge/bootc#6: waits; one pull request is promoted a pass".into(),
            ],
            5,
            0,
        ),
        (
            "an earlier approval goes first",
            Box::new(|r| {
                answer(r, "pulls/6/reviews")["body"] =
                    json!([review(2, "cgwalters", "APPROVED", OLDER, "", "2026-10-24T09:00:00Z")]);
            }),
            vec![
                "hold promote cgwalters-forge/bootc#6: cgwalters approved 4444444, but the head is now 6666666".into(),
                WOULD.into(),
            ],
            6,
            0,
        ),
        (
            "an unreadable review list is an error, not no approval",
            Box::new(|r| answer(r, "pulls/5/reviews")["status"] = 500.into()),
            vec!["promote error: cgwalters-forge/bootc#5: reading cgwalters-forge/bootc#5's reviews: GitHub HTTP status 500".into()],
            0,
            1,
        ),
        (
            "a marker in anyone else's comment is not the bot's notice",
            Box::new(|r| {
                reviews(r, vec![review(1, "cgwalters", "APPROVED", OLDER, "", "2026-10-24T10:12:00Z")]);
                answer(r, "issues/5/comments")["body"] = json!([{"id": 1, "html_url": "u",
                    "user": {"login": "visitor"}, "body": format!("Not promoted.\n{}", marker(&notice_key(HEAD_MOVED, 1, HEAD)))},
                    {"id": 2, "html_url": "v", "user": {"login": "visitor"},
                    "body": marker(&notice_key(REFUSED, 1, OLDER))}]);
            }),
            vec![hold("cgwalters approved 4444444, but the head is now 3333333")],
            1,
            0,
        ),
        (
            "a marker edited into the bot's comment is not the bot's notice",
            Box::new(|r| {
                answer(r, "issues/5/comments")["body"] = json!([{"id": 1, "html_url": "u",
                    "user": {"login": "cgwalters-bot"}, "created_at": "2026-10-24T09:00:00Z",
                    "updated_at": "2026-10-24T10:30:00Z",
                    "body": format!("Not promoted.\n{}", marker(&notice_key(REFUSED, 3400000001, HEAD)))}]);
            }),
            vec![WOULD.into()],
            5,
            0,
        ),
        (
            "a sign-off record edited into the bot's comment recovers nothing",
            Box::new(|r| {
                reviews(r, vec![review(1, "cgwalters", "APPROVED", OLDER, "", "2026-10-24T10:12:00Z")]);
                let record = json!({"schema": RECEIPT_SCHEMA, "kind": "signoff", "approval": 1, "approved": OLDER, "signed": HEAD});
                answer(r, "issues/5/comments")["body"] = json!([{"id": 1, "html_url": "u",
                    "user": {"login": "cgwalters-bot"}, "created_at": "2026-10-24T09:00:00Z",
                    "updated_at": "2026-10-24T10:30:00Z",
                    "body": format!("Signing off.\n\n```{PROMOTE_MARKER}\n{record}\n```\n")}]);
            }),
            vec![hold("cgwalters approved 4444444, but the head is now 3333333")],
            1,
            0,
        ),
        (
            "the description was edited after the approval by someone else",
            Box::new(|r| {
                *text_answer(r) = json!({"data": {"repository": {"pullRequest": {
                    "lastEditedAt": "2026-10-24T10:30:00Z", "editor": {"login": "cgwalters-bot"},
                    "timelineItems": {"nodes": []}}}}});
            }),
            vec![hold("its description was edited after the approval by cgwalters-bot")],
            1,
            0,
        ),
        (
            "the title was changed after the approval by someone else",
            Box::new(|r| {
                *text_answer(r) = json!({"data": {"repository": {"pullRequest": {
                    "lastEditedAt": null, "editor": null,
                    "timelineItems": {"nodes": [{"createdAt": "2026-10-24T10:30:00Z", "actor": {"login": "someone"}}]}}}}});
            }),
            vec![hold("its title was changed after the approval by someone")],
            1,
            0,
        ),
        (
            "the operator's own edits after the approval are theirs to make",
            Box::new(|r| {
                *text_answer(r) = json!({"data": {"repository": {"pullRequest": {
                    "lastEditedAt": "2026-10-24T10:30:00Z", "editor": {"login": "cgwalters"},
                    "timelineItems": {"nodes": [{"createdAt": "2026-10-24T10:31:00Z", "actor": {"login": "cgwalters"}}]}}}}});
            }),
            vec![WOULD.into()],
            5,
            0,
        ),
        (
            "a pull request whose head fork is gone holds itself, not the fork",
            Box::new(|r| {
                answer(r, "pulls?state=open")["body"][1]["head"]["repo"] = Value::Null;
            }),
            vec![
                "hold promote cgwalters-forge/bootc#6: it cannot be read: cgwalters-forge/bootc#6's head repository: invalid repository \"\"".into(),
                WOULD.into(),
            ],
            5,
            0,
        ),
        (
            "a head on the head repository's default branch is never promoted",
            Box::new(|r| {
                answer(r, "pulls?state=open")["body"][0]["head"]["ref"] = "main".into();
            }),
            vec![hold("its head is cgwalters-bot/bootc's default branch main, which is never rewritten or promoted")],
            1,
            0,
        ),
        (
            "a head on the fork's own default branch is never promoted",
            Box::new(|r| {
                let head = &mut answer(r, "pulls?state=open")["body"][0]["head"];
                head["ref"] = "main".into();
                head["repo"]["full_name"] = FORK.into();
            }),
            vec![
                "hold promote cgwalters-forge/bootc#5: its head is the fork's default branch main, which is never rewritten or promoted".into(),
                "promote: 0 approved of 2 open".into(),
            ],
            0,
            0,
        ),
    ];
    for (name, edit, lines, writes, failures) in cases {
        let mut replay = replay();
        edit(&mut replay);
        let report = preview(&config(json!({})), &replay);
        let got = stage(&report);
        for line in &lines {
            assert!(
                got.iter().any(|l| l.starts_with(line.as_str())),
                "{name}: no line {line:?} in {got:#?}"
            );
        }
        assert_eq!(
            report.summary,
            format!("summary: dispatched 0, writes {writes}, held 0"),
            "{name}: {got:#?}"
        );
        assert_eq!(report.failures, failures, "{name}: {got:#?}");
    }
}

/// An edit of a read's answer by its URL and how many times it was read.
type Late = Box<dyn Fn(&str, usize, &mut Value)>;

/// The recording as a forge that takes the stage's writes: commits written
/// anew, the branch moved, pull requests opened and closed, comments and
/// labels added, each showing in later reads. `fail` refuses the first
/// write whose `METHOD URL` contains it before it is made. `late` edits
/// each read's answer by its URL and how many times it was read.
struct Fork {
    replay: RefCell<Replay>,
    writes: RefCell<Vec<(String, String, Value)>>,
    fail: RefCell<Option<String>>,
    /// What the upstream answers a pull request with, instead of 201.
    refuse_pull: Option<(u16, &'static str)>,
    reads: RefCell<BTreeMap<String, usize>>,
    late: Option<Late>,
    ids: Cell<u64>,
    /// `REPO/BRANCH` to its sha, as the refs stand now.
    refs: RefCell<BTreeMap<String, String>>,
    /// Every commit known, by sha, as `compare` lists one.
    commits: RefCell<BTreeMap<String, Value>>,
    /// A push by someone else to this sha just before the ref update.
    push_first: RefCell<Option<String>>,
    /// A push by someone else to this sha as the pull request is opened.
    push_at_pull: RefCell<Option<String>>,
}

impl Fork {
    fn new() -> Self {
        let replay = replay();
        let compare = replay
            .requests
            .iter()
            .find(|e| e["request"].to_string().contains("compare/main"))
            .unwrap();
        let commits = compare["answer"]["body"]["commits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (text(c, "/sha").to_owned(), c.clone()))
            .collect();
        Self {
            replay: RefCell::new(replay),
            writes: RefCell::new(vec![]),
            fail: RefCell::new(None),
            refuse_pull: None,
            reads: RefCell::new(BTreeMap::new()),
            late: None,
            ids: Cell::new(0),
            refs: RefCell::new(BTreeMap::new()),
            commits: RefCell::new(commits),
            push_first: RefCell::new(None),
            push_at_pull: RefCell::new(None),
        }
    }

    fn edit(&self, f: impl FnOnce(&mut Replay)) {
        f(&mut self.replay.borrow_mut());
    }

    fn writes(&self) -> Vec<(String, String)> {
        (self.writes.borrow().iter())
            .map(|(m, u, _)| (m.clone(), u.trim_start_matches(API_ROOT).to_owned()))
            .collect()
    }

    fn bodies(&self, contains: &str) -> Vec<Value> {
        (self.writes.borrow().iter())
            .filter(|(m, u, _)| format!("{m} {u}").contains(contains))
            .map(|(_, _, b)| b.clone())
            .collect()
    }

    fn pulls(&self) -> Vec<Value> {
        let replay = self.replay.borrow();
        let entry = replay
            .requests
            .iter()
            .find(|e| e["request"].to_string().contains("pulls?state=open"))
            .unwrap();
        entry["answer"]["body"].as_array().unwrap().clone()
    }

    /// The sha `REPO/BRANCH` is at: as the pull request list has it, until a
    /// ref update moves it.
    fn sha(&self, repo: &str, branch: &str) -> Option<String> {
        if let Some(sha) = self.refs.borrow().get(&format!("{repo}/{branch}")) {
            return Some(sha.clone());
        }
        self.pulls()
            .iter()
            .find(|p| text(p, "/head/repo/full_name") == repo && text(p, "/head/ref") == branch)
            .map(|p| text(p, "/head/sha").to_owned())
    }

    /// The commits from `base` to `head`, oldest first, by their parents.
    fn chain(&self, base: &str, head: &str) -> Option<Vec<Value>> {
        let commits = self.commits.borrow();
        let mut out = Vec::new();
        let mut at = head.to_owned();
        while at != base {
            let commit = commits.get(&at)?;
            at = text(commit, "/parents/0/sha").to_owned();
            out.push(commit.clone());
        }
        out.reverse();
        Some(out)
    }

    fn write(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        let path = url.trim_start_matches(API_ROOT).to_owned();
        let parts: Vec<&str> = path.split('/').collect();
        let id = self.ids.get() + 1;
        self.ids.set(id);
        if method == "POST" && path.ends_with("/git/commits") {
            let sha = format!("{id:0>40}");
            let commit = json!({"sha": sha, "parents": [{"sha": body["parents"][0]}],
                "author": {"login": "cgwalters-bot"},
                "commit": {"tree": {"sha": body["tree"]}, "author": body["author"],
                    "committer": body["committer"], "message": body["message"]}});
            self.commits.borrow_mut().insert(sha.clone(), commit);
            return Ok((
                201,
                json!({"sha": sha, "tree": {"sha": body["tree"]}, "parents": [{"sha": body["parents"][0]}],
                    "author": body["author"], "committer": body["committer"], "message": body["message"]}),
            ));
        }
        if method == "POST" && path.ends_with("/pulls") {
            if let Some((status, message)) = self.refuse_pull {
                return Ok((status, json!({"message": message})));
            }
            let upstream = format!("{}/{}", parts[1], parts[2]);
            let (owner, branch) = text(body, "/head").split_once(':').unwrap();
            if let Some(sha) = self.push_at_pull.borrow_mut().take() {
                self.move_ref(&format!("{owner}/bootc"), branch, &sha);
            }
            let head = self
                .pulls()
                .iter()
                .find(|p| {
                    text(p, "/head/repo/full_name").starts_with(owner)
                        && text(p, "/head/ref") == branch
                })
                .map(|p| text(p, "/head/sha").to_owned())
                .unwrap_or_default();
            let number = 2600 + id;
            let opened = json!({"number": number, "state": "open", "merged_at": null,
                "html_url": format!("https://github.com/{upstream}/pull/{number}"), "head": {"sha": head}});
            let needle = format!("pulls?state=all&head={}&per_page", text(body, "/head"));
            let mut replay = self.replay.borrow_mut();
            for entry in &mut replay.requests {
                if entry["request"].to_string().contains(&needle) {
                    entry["answer"]["body"] = json!([opened]);
                }
            }
            return Ok((201, opened));
        }
        if method == "POST" && path.ends_with("/comments") {
            let html_url = format!(
                "https://github.com/{}/{}/pull/{}#issuecomment-{id}",
                parts[1], parts[2], parts[4]
            );
            let comment = json!({"id": id, "html_url": html_url, "body": body["body"],
                "user": {"login": "cgwalters-bot"}});
            let needle = format!("issues/{}/comments?per_page", parts[4]);
            let mut replay = self.replay.borrow_mut();
            for entry in &mut replay.requests {
                if entry["request"].to_string().contains(&needle) {
                    entry["answer"]["body"]
                        .as_array_mut()
                        .unwrap()
                        .push(comment.clone());
                }
            }
            return Ok((201, json!({"id": id, "html_url": html_url})));
        }
        if method == "POST" && path.ends_with("/labels") {
            let number: u64 = parts[4].parse()?;
            let mut replay = self.replay.borrow_mut();
            for entry in &mut replay.requests {
                if entry["request"].to_string().contains("pulls?state=open") {
                    for pull in entry["answer"]["body"].as_array_mut().unwrap() {
                        if pull["number"] == number {
                            for label in body["labels"].as_array().unwrap() {
                                pull["labels"]
                                    .as_array_mut()
                                    .unwrap()
                                    .push(json!({"name": label}));
                            }
                        }
                    }
                }
            }
            return Ok((200, json!([])));
        }
        if method == "PATCH" && parts.len() == 5 && parts[3] == "pulls" {
            let number: u64 = parts[4].parse()?;
            let mut replay = self.replay.borrow_mut();
            for entry in &mut replay.requests {
                if entry["request"].to_string().contains("pulls?state=open") {
                    entry["answer"]["body"]
                        .as_array_mut()
                        .unwrap()
                        .retain(|p| p["number"] != number);
                }
                if entry["request"]
                    .to_string()
                    .contains("pulls?state=all&head=")
                {
                    for pull in entry["answer"]["body"].as_array_mut().unwrap() {
                        if pull["number"] == number {
                            pull["state"] = body["state"].clone();
                        }
                    }
                }
            }
            return Ok((200, json!({"number": number, "state": body["state"]})));
        }
        bail!("unexpected write {method} {url}")
    }

    /// Moves `repo`'s `branch` to `sha`, showing in the pull request list
    /// and the compare of that branch.
    fn move_ref(&self, repo: &str, branch: &str, sha: &str) {
        let owner = repo.split('/').next().unwrap();
        self.refs
            .borrow_mut()
            .insert(format!("{repo}/{branch}"), sha.to_owned());
        let chain = self.chain(BASE, sha);
        let mut replay = self.replay.borrow_mut();
        for entry in &mut replay.requests {
            let request = entry["request"].to_string();
            if request.contains("pulls?state=open") {
                for pull in entry["answer"]["body"].as_array_mut().unwrap() {
                    if text(pull, "/head/repo/full_name") == repo
                        && text(pull, "/head/ref") == branch
                    {
                        pull["head"]["sha"] = sha.into();
                    }
                }
            }
            if let (true, Some(chain)) =
                (request.contains(&format!("...{owner}:{branch}?")), &chain)
            {
                entry["answer"]["body"]["ahead_by"] = chain.len().into();
                entry["answer"]["body"]["commits"] = chain.clone().into();
            }
        }
    }

    /// `updateRefs` with its lease: after `push_first`'s push, if any, the
    /// ref must be at `before`, or GitHub answers its generic error and
    /// moves nothing.
    fn update_ref(&self, variables: &Value) -> Result<Value> {
        self.writes
            .borrow_mut()
            .push(("GRAPHQL".into(), "updateRefs".into(), variables.clone()));
        self.ids.set(self.ids.get() + 1);
        ensure!(variables["repo"] == "R_kgDOBotBootc", "unknown repository");
        let repo = "cgwalters-bot/bootc";
        let branch = text(variables, "/name")
            .strip_prefix("refs/heads/")
            .context("not a branch")?
            .to_owned();
        if let Some(sha) = self.push_first.borrow_mut().take() {
            self.move_ref(repo, &branch, &sha);
        }
        ensure!(
            self.sha(repo, &branch).as_deref() == Some(text(variables, "/before")),
            "Something went wrong while executing your query"
        );
        self.move_ref(repo, &branch, text(variables, "/after"));
        Ok(json!({"updateRefs": {"clientMutationId": null}}))
    }
}

impl Broker for Fork {
    fn usage(&self, url: &str) -> Result<Value> {
        self.replay.borrow().usage(url)
    }
}

impl Rest for Fork {
    fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
        let nth = {
            let mut reads = self.reads.borrow_mut();
            let n = reads.entry(url.to_owned()).or_default();
            *n += 1;
            *n
        };
        let path = url.trim_start_matches(API_ROOT);
        let parts: Vec<&str> = path.split(['/', '?']).collect();
        let mut body = if let Some((repo, branch)) = path.split_once("/git/ref/heads/") {
            let repo = repo.trim_start_matches("repos/");
            let sha = self.sha(repo, branch).context("no such branch")?;
            json!({"ref": format!("refs/heads/{branch}"), "object": {"sha": sha}})
        } else if parts.len() == 5 && parts[3] == "pulls" && parts[4].parse::<u64>().is_ok() {
            let number: u64 = parts[4].parse()?;
            match self.pulls().into_iter().find(|p| p["number"] == number) {
                Some(pull) => pull,
                None => {
                    json!({"number": number, "state": "closed", "draft": false, "head": {"sha": ""}})
                }
            }
        } else if parts.len() > 4 && parts[3] == "compare" && parts[1] == "cgwalters-bot" {
            let (base, head) = parts[4].split_once("...").unwrap();
            let chain = self.chain(base, head).context("no such series")?;
            json!({"ahead_by": chain.len(), "behind_by": 0, "merge_base_commit": {"sha": base}, "commits": chain})
        } else {
            let response = self.replay.borrow().get(url, None)?;
            if response.status != 200 {
                return Ok(response);
            }
            response.body
        };
        if let Some(late) = &self.late {
            late(url, nth, &mut body);
        }
        Ok(RestResponse {
            status: 200,
            etag: None,
            last_modified: None,
            link: None,
            body,
        })
    }

    fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        let mut fail = self.fail.borrow_mut();
        if fail
            .as_deref()
            .is_some_and(|f| format!("{method} {url}").contains(f))
        {
            *fail = None;
            bail!("connection reset during {method}");
        }
        drop(fail);
        self.writes
            .borrow_mut()
            .push((method.into(), url.into(), body.clone()));
        self.write(method, url, body)
    }
}

impl Transport for Fork {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        if query == UPDATE_REF {
            return self.update_ref(&variables);
        }
        self.replay.borrow().graphql(query, variables)
    }
}

fn apply(fork: &Fork, config: &Config, writes: Option<usize>) -> Report {
    let limits = Limits {
        dispatch: Some(0),
        writes,
        ..Limits::default()
    };
    let now = fork.replay.borrow().now;
    run(
        fork,
        fork,
        config,
        State::default(),
        (Mode::Local, limits),
        now,
    )
    .unwrap()
}

const OPENED: &str = "promote cgwalters-forge/bootc#5 -> bootc-dev/bootc: opened https://github.com/bootc-dev/bootc/pull/2604 (sign-off on 1 of 2 commits)";

/// An applying pass writes the signed commit, the record, the ref, the
/// upstream pull request, the receipt and the close, in that order and as
/// asked; a second pass finds nothing to do.
#[test]
fn apply_promotes_with_the_signoff() {
    let fork = Fork::new();
    let config = config(json!({}));
    let report = apply(&fork, &config, Some(5));
    assert_eq!(stage(&report), [COUNT, OPENED], "{:#?}", report.lines);
    assert_eq!(
        report.summary,
        "summary: dispatched 0/0, writes 5/5, held 0"
    );
    assert_eq!(report.failures, 0);
    let writes = fork.writes();
    let expected = [
        ("POST", "repos/cgwalters-bot/bootc/git/commits"),
        ("POST", "repos/cgwalters-forge/bootc/issues/5/comments"),
        ("GRAPHQL", "updateRefs"),
        ("POST", "repos/bootc-dev/bootc/pulls"),
        ("POST", "repos/cgwalters-forge/bootc/issues/5/comments"),
        ("PATCH", "repos/cgwalters-forge/bootc/pulls/5"),
    ];
    let got: Vec<(&str, &str)> = writes
        .iter()
        .map(|(m, u)| (m.as_str(), u.as_str()))
        .collect();
    assert_eq!(got, expected);
    let commit = &fork.bodies("git/commits")[0];
    assert_eq!(commit["tree"], "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    assert_eq!(commit["parents"], json!([FIRST]));
    assert_eq!(
        commit["author"],
        json!({"name": "cgwalters-bot", "email": "cgwalters-bot@users.noreply.github.com", "date": "2026-10-22T18:01:00Z"})
    );
    assert_eq!(
        commit["committer"],
        json!({"name": "Operator Example", "email": "operator@example.com", "date": "2026-10-24T12:00:00Z"})
    );
    assert_eq!(
        commit["message"],
        format!(
            "install: Keep xfs as the default for a blank disk\n\nGenerated-by: https://github.com/cgwalters/#llms\n{TRAILER}"
        )
    );
    let signed = format!("{:0>40}", 1);
    let record = text(&fork.bodies("issues/5/comments")[0], "/body").to_owned();
    assert!(
        record.starts_with("Adding cgwalters's sign-off to 1 of 2 commits on their approval https://github.com/cgwalters-forge/bootc/pull/5#pullrequestreview-3400000001: the approved head 3333333 becomes 0000000.\n\n```agent-board-promote\n"),
        "{record}"
    );
    let detail = fenced_json(&record, PROMOTE_MARKER).unwrap().unwrap();
    assert_eq!(
        detail,
        json!({"schema": RECEIPT_SCHEMA, "kind": "signoff", "approval": 3400000001u64, "approved": HEAD, "signed": signed})
    );
    assert!(record.ends_with(&format!("```\n\n{FOOTER}\n")), "{record}");
    assert_eq!(
        fork.bodies("updateRefs")[0],
        json!({"repo": "R_kgDOBotBootc", "name": format!("refs/heads/{BRANCH}"), "before": HEAD, "after": signed})
    );
    let opened = &fork.bodies("bootc-dev/bootc/pulls")[0];
    assert_eq!(
        opened["title"],
        "install: Read the root filesystem type from the target disk"
    );
    assert_eq!(
        opened["head"],
        "cgwalters-bot:dispatch/1030-root-filesystem"
    );
    assert_eq!(opened["base"], "main");
    assert_eq!(opened["draft"], false);
    assert_eq!(opened["maintainer_can_modify"], true);
    assert_eq!(
        opened["body"],
        format!(
            "The installer assumed xfs for the root filesystem whatever the target disk already carried. Read the type from the disk's existing root partition when there is one, and keep xfs as the default for a blank disk.\n\nCloses bootc-dev/bootc#1030.\n\n`Signed-off-by: Operator Example` was added to each commit on cgwalters's approval (https://github.com/cgwalters-forge/bootc/pull/5#pullrequestreview-3400000001).\n\n{FOOTER}"
        )
    );
    let receipt = text(&fork.bodies("issues/5/comments")[1], "/body").to_owned();
    assert!(
        receipt.starts_with("Opened upstream as https://github.com/bootc-dev/bootc/pull/2604, from cgwalters-bot:dispatch/1030-root-filesystem at 0000000. Closing this review draft.\n\n```agent-board-promote\n"),
        "{receipt}"
    );
    assert_eq!(
        fenced_json(&receipt, PROMOTE_MARKER).unwrap().unwrap(),
        json!({"schema": RECEIPT_SCHEMA, "kind": "promoted", "approval": 3400000001u64, "head": signed,
            "upstream": "https://github.com/bootc-dev/bootc/pull/2604"})
    );
    assert_eq!(fork.bodies("pulls/5")[0], json!({"state": "closed"}));
    // Idempotence: the fork pull request is closed, so nothing is read or
    // written for it again.
    let second = apply(&fork, &config, Some(5));
    assert_eq!(
        stage(&second),
        ["promote: 0 approved of 1 open pull request(s) of cgwalters-bot in 1 fork(s)"]
    );
    assert_eq!(fork.writes().len(), 6);
}

/// An upstream pull request open with the approved head already: an
/// applying pass links and closes the fork pull request, two writes, and
/// opens nothing.
#[test]
fn apply_links_and_closes_behind_an_upstream_pull_request() {
    let fork = Fork::new();
    fork.edit(|r| {
        answer(r, "pulls?state=all&head=cgwalters-bot:dispatch/1030")["body"] =
            json!([upstream_pull(2600, "open", false, HEAD)]);
    });
    let report = apply(&fork, &config(json!({})), Some(2));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    assert_eq!(
        stage(&report),
        [
            COUNT,
            "promote cgwalters-forge/bootc#5 -> bootc-dev/bootc: https://github.com/bootc-dev/bootc/pull/2600 is open already; linked and closed"
        ]
    );
    assert_eq!(
        report.summary,
        "summary: dispatched 0/0, writes 2/2, held 0"
    );
    let writes: Vec<String> = fork
        .writes()
        .iter()
        .map(|(m, u)| format!("{m} {u}"))
        .collect();
    assert_eq!(
        writes,
        [
            "POST repos/cgwalters-forge/bootc/issues/5/comments",
            "PATCH repos/cgwalters-forge/bootc/pulls/5",
        ]
    );
    let receipt = text(&fork.bodies("comments")[0], "/body").to_owned();
    assert!(
        receipt.starts_with("Opened upstream as https://github.com/bootc-dev/bootc/pull/2600, from cgwalters-bot:dispatch/1030-root-filesystem at 3333333."),
        "{receipt}"
    );
    assert!(fork.pulls().iter().all(|p| p["number"] != 5));
}

/// A link-and-close step whose close fails after its receipt counts whole
/// against the cap, so the next ready pull request's step is held rather
/// than taken past it.
#[test]
fn a_failed_link_and_close_counts_against_the_cap() {
    let six = "6666666666666666666666666666666666666666";
    let fork = Fork::new();
    fork.edit(|r| {
        answer(r, "pulls?state=all&head=cgwalters-bot:dispatch/1030")["body"] =
            json!([upstream_pull(2600, "open", false, HEAD)]);
        answer(r, "pulls/6/reviews")["body"] = json!([review(
            2,
            "cgwalters",
            "APPROVED",
            six,
            "",
            "2026-10-24T11:00:00Z"
        )]);
        let pull = json!({"owner": "cgwalters-forge", "name": "bootc", "number": 6});
        r.requests.push(graphql(
            CLOSING_QUERY,
            pull.clone(),
            json!({"repository": {"pullRequest": {"closingIssuesReferences": {"nodes": [], "totalCount": 0}}}}),
        ));
        r.requests.push(graphql(
            TEXT_QUERY,
            pull,
            json!({"repository": {"pullRequest": {"lastEditedAt": null, "editor": null, "timelineItems": {"nodes": []}}}}),
        ));
        r.requests.push(get(
            &format!("repos/{UPSTREAM}/pulls?state=all&head=cgwalters-bot:dispatch/1041-unused-results&per_page=10"),
            json!([upstream_pull(2601, "open", false, six)]),
        ));
    });
    *fork.fail.borrow_mut() = Some(format!("PATCH {API_ROOT}repos/{FORK}/pulls/5"));
    let report = apply(&fork, &config(json!({})), Some(3));
    assert_eq!(report.failures, 1, "{:#?}", report.lines);
    assert!(
        report.lines.contains(&"held by --max-writes: promote cgwalters-forge/bootc#6 -> bootc-dev/bootc: https://github.com/bootc-dev/bootc/pull/2601 is open already; would link and close".into()),
        "{:#?}",
        report.lines
    );
    assert_eq!(
        report.summary,
        "summary: dispatched 0/0, writes 2/3, held 1"
    );
    assert_eq!(
        fork.writes(),
        [(
            "POST".to_owned(),
            "repos/cgwalters-forge/bootc/issues/5/comments".to_owned()
        )]
    );
}

/// The text that goes upstream is the text checked: an edit after the
/// listing (here the operator's, approved afterwards) stops the step before
/// anything is written, and the next pass checks and sends the new text.
#[test]
fn apply_sends_only_the_text_it_checked() {
    let corrected = "The corrected description.\n\nCloses bootc-dev/bootc#1030.";
    let mut fork = Fork::new();
    let url = format!("{API_ROOT}repos/{FORK}/pulls/5");
    fork.late = Some(Box::new(move |read, _, body| {
        if read == url {
            body["body"] = corrected.into();
        }
    }));
    let config = config(json!({}));
    let first = apply(&fork, &config, Some(5));
    assert!(
        first.lines.contains(&"promote error: cgwalters-forge/bootc#5: its title or description changed while promoting; nothing is opened, and the next pass checks the text as it is".into()),
        "{:#?}",
        first.lines
    );
    assert!(fork.writes().is_empty(), "{:#?}", fork.writes());
    fork.late = None;
    fork.edit(|r| answer(r, "pulls?state=open")["body"][0]["body"] = corrected.into());
    let second = apply(&fork, &config, Some(5));
    assert_eq!(second.failures, 0, "{:#?}", second.lines);
    let body = text(&fork.bodies("bootc-dev/bootc/pulls")[0], "/body").to_owned();
    assert!(body.starts_with(corrected), "{body}");
}

/// A `human-text` promotion cut short after its sign-off push: GitHub shows
/// the bot pushed the signed head, and the next pass recovers it by the
/// operator's push of the approved head and the bot's of the signed one.
#[test]
fn apply_recovers_a_human_text_signoff() {
    let fork = Fork::new();
    fork.edit(|r| {
        human_text(r, "cgwalters", "cgwalters", "cgwalters");
        human_written(r);
    });
    *fork.fail.borrow_mut() = Some(format!("POST {API_ROOT}repos/{UPSTREAM}/pulls"));
    let config = config(json!({}));
    let first = apply(&fork, &config, Some(5));
    assert_eq!(first.failures, 1, "{:#?}", first.lines);
    let signed = format!("{:0>40}", 1);
    assert_eq!(
        fork.sha("cgwalters-bot/bootc", BRANCH).as_deref(),
        Some(signed.as_str())
    );
    let push = |pusher: &str| {
        let signed = signed.clone();
        let pusher = pusher.to_owned();
        move |r: &mut Replay| {
            answer(r, "/activity?")["body"] = json!([
                {"before": FIRST, "after": HEAD, "activity_type": "force_push", "actor": {"login": "cgwalters"}},
                {"before": HEAD, "after": signed, "activity_type": "force_push", "actor": {"login": pusher}},
            ]);
        }
    };
    // Someone else's push of the signed head is not the bot's sign-off.
    fork.edit(push("someone"));
    let held = apply(&fork, &config, Some(0));
    assert!(
        held.lines.iter().any(|l| l.ends_with(&format!(
            "GitHub shows someone pushed the signed head {}, not cgwalters-bot",
            short(&signed)
        ))),
        "{:#?}",
        held.lines
    );
    fork.edit(push("cgwalters-bot"));
    let second = apply(&fork, &config, Some(5));
    assert_eq!(second.failures, 0, "{:#?}", second.lines);
    assert!(
        second.lines.contains(&"promote cgwalters-forge/bootc#5 -> bootc-dev/bootc: opened https://github.com/bootc-dev/bootc/pull/2604 (sign-off recovered from its record)".into()),
        "{:#?}",
        second.lines
    );
    assert_eq!(
        fork.bodies("bootc-dev/bootc/pulls")[0]["body"],
        "The operator's own words.\n\nCloses bootc-dev/bootc#1030."
    );
}

/// Without DCO upstream the three writes are the pull request, the receipt
/// and the close; the body gets no sign-off paragraph. Where the operator
/// wrote the text, the sign-off goes on the commits but the body is theirs.
#[test]
fn apply_adds_no_text_where_none_is_due() {
    let fork = Fork::new();
    fork.edit(no_dco);
    let report = apply(&fork, &config(json!({})), Some(3));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    let writes: Vec<String> = fork
        .writes()
        .iter()
        .map(|(m, u)| format!("{m} {u}"))
        .collect();
    assert_eq!(
        writes,
        [
            "POST repos/bootc-dev/bootc/pulls",
            "POST repos/cgwalters-forge/bootc/issues/5/comments",
            "PATCH repos/cgwalters-forge/bootc/pulls/5",
        ]
    );
    let body = text(&fork.bodies("bootc-dev/bootc/pulls")[0], "/body").to_owned();
    assert!(
        body.ends_with(&format!("Closes bootc-dev/bootc#1030.\n\n{FOOTER}")),
        "{body}"
    );
    let fork = Fork::new();
    fork.edit(|r| {
        human_text(r, "cgwalters", "cgwalters", "cgwalters");
        human_written(r);
    });
    let report = apply(&fork, &config(json!({})), Some(5));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    assert_eq!(fork.bodies("git/commits").len(), 1);
    assert_eq!(
        fork.bodies("bootc-dev/bootc/pulls")[0]["body"],
        "The operator's own words.\n\nCloses bootc-dev/bootc#1030."
    );
}

/// `--max-writes` holds the promotion whole, and a notice is its own step.
#[test]
fn caps_hold_the_promotion_whole() {
    for writes in [0, 4] {
        let fork = Fork::new();
        let report = apply(&fork, &config(json!({})), Some(writes));
        assert_eq!(
            stage(&report),
            [COUNT, &format!("held by --max-writes: {WOULD}")]
        );
        assert_eq!(
            report.summary,
            format!("summary: dispatched 0/0, writes 0/{writes}, held 1")
        );
        assert!(fork.writes().is_empty(), "{:#?}", fork.writes());
    }
    let fork = Fork::new();
    fork.edit(|r| answer(r, "compare/main")["body"]["behind_by"] = 3.into());
    let report = apply(&fork, &config(json!({})), Some(0));
    assert!(
        report
            .lines
            .contains(&"held by --max-writes: notice cgwalters-forge/bootc#5: refused".into()),
        "{:#?}",
        report.lines
    );
    assert!(fork.writes().is_empty());
}

/// A push by someone else refuses with an error, and no pull request is
/// opened: one just before the ref update, which the lease keeps, and one
/// between the ref update and the pull request.
#[test]
fn apply_refuses_when_the_head_moves_while_promoting() {
    let ref_url = format!("{API_ROOT}repos/cgwalters-bot/bootc/git/ref/heads/{BRANCH}");
    for (when, nth, error) in [
        (
            "between the read of the ref and its update",
            0,
            "promote error: cgwalters-forge/bootc#5: the branch is at 4444444, not the approved 3333333: it moved while the commits were signed; nothing is pushed",
        ),
        (
            "between the ref update and the pull request",
            3,
            "promote error: cgwalters-forge/bootc#5: the branch is at 4444444, not 0000000: it moved while promoting; nothing is opened",
        ),
    ] {
        let mut fork = Fork::new();
        let url = ref_url.clone();
        match nth {
            0 => *fork.push_first.borrow_mut() = Some(OLDER.into()),
            _ => {
                fork.late = Some(Box::new(move |read, n, body| {
                    if read == url && n == nth {
                        body["object"]["sha"] = OLDER.into();
                    }
                }))
            }
        }
        let report = apply(&fork, &config(json!({})), Some(5));
        assert!(
            report.lines.iter().any(|l| l == error),
            "{when}: {:#?}",
            report.lines
        );
        assert_eq!(report.failures, 1, "{when}");
        assert!(
            fork.bodies("bootc-dev/bootc/pulls").is_empty(),
            "{when}: a pull request was opened"
        );
        if nth == 0 {
            // The other push is kept, not overwritten by the signed head.
            assert_eq!(
                fork.sha("cgwalters-bot/bootc", BRANCH).as_deref(),
                Some(OLDER)
            );
        }
    }
}

/// A push that lands after the last ref read, as GitHub opens the pull
/// request from the branch: the opened head is not the checked one, so the
/// pull request is closed again, refused with a notice, and neither the
/// receipt nor the fork pull request's close follows.
#[test]
fn apply_closes_a_pull_request_opened_on_another_head() {
    let fork = Fork::new();
    *fork.push_at_pull.borrow_mut() = Some(OLDER.into());
    let report = apply(&fork, &config(json!({})), Some(6));
    assert_eq!(report.failures, 1, "{:#?}", report.lines);
    let line = "hold promote cgwalters-forge/bootc#5: https://github.com/bootc-dev/bootc/pull/2604 opened with head 4444444, not the approved 0000000: the branch moved as it was opened; it was closed again";
    assert!(
        report.lines.iter().any(|l| l == line),
        "{:#?}",
        report.lines
    );
    let writes: Vec<String> = fork.writes()[3..]
        .iter()
        .map(|(m, u)| format!("{m} {u}"))
        .collect();
    assert_eq!(
        writes,
        [
            "POST repos/bootc-dev/bootc/pulls",
            "PATCH repos/bootc-dev/bootc/pulls/2604",
            "POST repos/cgwalters-forge/bootc/issues/5/comments",
            "POST repos/cgwalters-forge/bootc/issues/5/labels",
        ]
    );
    assert_eq!(
        fork.bodies("bootc-dev/bootc/pulls/2604")[0],
        json!({"state": "closed"})
    );
    let notice = text(&fork.bodies("issues/5/comments")[1], "/body").to_owned();
    assert!(notice.starts_with("Not promoted: "), "{notice}");
    assert!(!notice.contains(RECEIPT_SCHEMA), "{notice}");
    assert!(
        fork.pulls().iter().any(|p| p["number"] == 5),
        "the fork pull request was closed"
    );
}

/// A pass cut short after the ref update: the next pass proves the head
/// against the record and finishes without signing again. A record that
/// does not prove holds the pull request for a person, with nothing written
/// but the notice.
#[test]
fn apply_cut_short_after_the_ref_update_recovers_by_the_record() {
    let fork = Fork::new();
    *fork.fail.borrow_mut() =
        Some("POST https://api.github.com/repos/bootc-dev/bootc/pulls".into());
    let config = config(json!({}));
    let first = apply(&fork, &config, Some(5));
    assert_eq!(first.failures, 1, "{:#?}", first.lines);
    assert!(
        first.lines.iter().any(|l| l.starts_with("promote error: cgwalters-forge/bootc#5: opening the upstream pull request: connection reset")),
        "{:#?}",
        first.lines
    );
    assert_eq!(fork.writes().len(), 3);
    let second = apply(&fork, &config, Some(5));
    assert_eq!(second.failures, 0, "{:#?}", second.lines);
    assert_eq!(
        stage(&second),
        [
            COUNT,
            "promote cgwalters-forge/bootc#5 -> bootc-dev/bootc: opened https://github.com/bootc-dev/bootc/pull/2604 (sign-off recovered from its record)"
        ]
    );
    assert_eq!(
        second.summary,
        "summary: dispatched 0/0, writes 3/5, held 0"
    );
    let writes: Vec<String> = fork.writes()[3..]
        .iter()
        .map(|(m, u)| format!("{m} {u}"))
        .collect();
    assert_eq!(
        writes,
        [
            "POST repos/bootc-dev/bootc/pulls",
            "POST repos/cgwalters-forge/bootc/issues/5/comments",
            "PATCH repos/cgwalters-forge/bootc/pulls/5",
        ]
    );
    let body = text(&fork.bodies("bootc-dev/bootc/pulls")[0], "/body").to_owned();
    assert!(
        body.contains("was added to each commit on cgwalters's approval"),
        "{body}"
    );
    // A record whose series does not prove: the approved series has
    // another tree.
    let fork = Fork::new();
    fork.edit(|r| {
        reviews(r, vec![review(1, "cgwalters", "APPROVED", OLDER, "", "2026-10-24T10:12:00Z")]);
        let record = json!({"schema": RECEIPT_SCHEMA, "kind": "signoff", "approval": 1, "approved": OLDER, "signed": HEAD});
        answer(r, "issues/5/comments")["body"] = json!([{"id": 1, "html_url": "https://github.com/cgwalters-forge/bootc/pull/5#issuecomment-1",
            "user": {"login": "cgwalters-bot"}, "body": format!("Signing off.\n\n```{PROMOTE_MARKER}\n{record}\n```\n")}]);
    });
    {
        let mut commits = fork.commits.borrow_mut();
        let mut other = commits[HEAD].clone();
        other["sha"] = OLDER.into();
        other["commit"]["tree"]["sha"] = "9999999999999999999999999999999999999999".into();
        other["commit"]["message"] = "install: Keep xfs as the default for a blank disk\n\nGenerated-by: https://github.com/cgwalters/#llms".into();
        commits.insert(OLDER.into(), other);
    }
    let report = apply(&fork, &config, Some(5));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    let line = "hold promote cgwalters-forge/bootc#5: its head 3333333 is the bot's sign-off push by the record https://github.com/cgwalters-forge/bootc/pull/5#issuecomment-1, but it does not prove as the approved 4444444 plus the sign-off: 4444444 as 3333333: the tree differs";
    assert!(
        report.lines.iter().any(|l| l == line),
        "{:#?}",
        report.lines
    );
    let writes: Vec<String> = fork
        .writes()
        .iter()
        .map(|(m, u)| format!("{m} {u}"))
        .collect();
    assert_eq!(
        writes,
        [
            "POST repos/cgwalters-forge/bootc/issues/5/comments",
            "POST repos/cgwalters-forge/bootc/issues/5/labels",
        ]
    );
    assert_eq!(fork.bodies("labels")[0], json!({"labels": ["needs-human"]}));
}

/// The same refusal on two passes posts one notice; a new approval posts a
/// new one; `head-moved` is said once per approval though the head moves
/// again. The notice reads as the operator's next step.
#[test]
fn notices_are_said_once_per_approval_and_head() {
    let fork = Fork::new();
    fork.edit(|r| answer(r, "compare/main")["body"]["behind_by"] = 3.into());
    let config = config(json!({}));
    let first = apply(&fork, &config, Some(5));
    assert_eq!(first.failures, 0, "{:#?}", first.lines);
    assert_eq!(fork.bodies("comments").len(), 1);
    let notice = text(&fork.bodies("comments")[0], "/body").to_owned();
    assert_eq!(
        notice,
        format!(
            "Not promoted: 3 commit(s) behind bootc-dev/bootc main; this stage does not rebase.\n\nNext: rebase the branch onto the upstream base and approve the new head.\n\n{FOOTER}\n{}\n",
            marker(&notice_key(REFUSED, 3400000001, HEAD))
        )
    );
    let second = apply(&fork, &config, Some(5));
    assert!(
        second.lines.iter().any(|l| l.starts_with(
            "hold promote cgwalters-forge/bootc#5: refused already for this approval and head, "
        )),
        "{:#?}",
        second.lines
    );
    assert_eq!(fork.bodies("comments").len(), 1);
    fork.edit(|r| {
        reviews(
            r,
            vec![review(
                2,
                "cgwalters",
                "APPROVED",
                HEAD,
                "",
                "2026-10-24T11:00:00Z",
            )],
        )
    });
    apply(&fork, &config, Some(5));
    assert_eq!(fork.bodies("comments").len(), 2);
    // head-moved: one notice for the approval, however the head moves.
    let fork = Fork::new();
    fork.edit(|r| {
        reviews(
            r,
            vec![review(
                1,
                "cgwalters",
                "APPROVED",
                OLDER,
                "",
                "2026-10-24T10:12:00Z",
            )],
        )
    });
    apply(&fork, &config, Some(5));
    fork.edit(|r| {
        answer(r, "pulls?state=open")["body"][0]["head"]["sha"] =
            "5555555555555555555555555555555555555555".into()
    });
    let report = apply(&fork, &config, Some(5));
    assert!(
        report.lines.contains(&"hold promote cgwalters-forge/bootc#5: cgwalters approved 4444444, but the head is now 5555555".into()),
        "{:#?}",
        report.lines
    );
    assert_eq!(fork.bodies("comments").len(), 1);
    let notice = text(&fork.bodies("comments")[0], "/body").to_owned();
    assert!(notice.starts_with("Not promoted: cgwalters approved 4444444, but the head is now 3333333.\n\nNext: approve the current head to promote it."), "{notice}");
    // A blocking policy quotes the record, once.
    let fork = Fork::new();
    fork.edit(|r| set_record(r, &record_text("no-go")));
    apply(&fork, &config, Some(5));
    let notice = text(&fork.bodies("comments")[0], "/body").to_owned();
    assert!(
        notice.contains("\n\n> The project's rules, quoted.\n\n"),
        "{notice}"
    );
}

/// An upstream that refuses the pull request: the response in the line and
/// one notice, nothing closed.
#[test]
fn apply_reports_an_upstream_that_refuses() {
    // The failed step's writes count against the cap, so with five the
    // notice is held; with six it is posted.
    for (cap, notice) in [(5, false), (6, true)] {
        let mut fork = Fork::new();
        fork.refuse_pull = Some((403, "Resource not accessible by integration"));
        let report = apply(&fork, &config(json!({})), Some(cap));
        assert!(
            report.lines.contains(&"hold promote cgwalters-forge/bootc#5: bootc-dev/bootc refused the pull request: HTTP status 403, Resource not accessible by integration".into()),
            "{:#?}",
            report.lines
        );
        assert_eq!(report.failures, 1);
        let writes: Vec<String> = fork
            .writes()
            .iter()
            .map(|(m, u)| format!("{m} {u}"))
            .collect();
        assert_eq!(writes[3], "POST repos/bootc-dev/bootc/pulls");
        match notice {
            true => {
                assert_eq!(
                    writes[4],
                    "POST repos/cgwalters-forge/bootc/issues/5/comments"
                );
                assert_eq!(writes.len(), 5);
                assert_eq!(
                    report.summary,
                    "summary: dispatched 0/0, writes 6/6, held 0"
                );
            }
            false => {
                assert_eq!(writes.len(), 4);
                assert!(
                    report.lines.contains(
                        &"held by --max-writes: notice cgwalters-forge/bootc#5: refused".into()
                    ),
                    "{:#?}",
                    report.lines
                );
                assert_eq!(
                    report.summary,
                    "summary: dispatched 0/0, writes 5/5, held 1"
                );
            }
        }
        assert!(
            fork.pulls().iter().any(|p| p["number"] == 5),
            "the fork pull request was closed"
        );
    }
}

/// The configuration's refusals.
#[test]
fn config_table() {
    let base: Value = serde_json::from_str(CONFIG).unwrap();
    let with = |edit: fn(&mut Value)| {
        let mut value = base.clone();
        edit(&mut value);
        Config::parse(&value.to_string())
            .map(|_| ())
            .map_err(|e| format!("{e:#}"))
    };
    for (case, edit, error) in [
        (
            "unknown key",
            (|v| v["promote"]["draft"] = true.into()) as fn(&mut Value),
            "unknown field",
        ),
        (
            "null",
            |v| v["promote"]["signoff"] = Value::Null,
            "config.promote.signoff is null",
        ),
        (
            "a fork not OWNER/REPO",
            |v| {
                v["promote"]["forks"] =
                    json!({"bootc": {"upstream": "bootc-dev/bootc", "base": "main"}})
            },
            "promote forks: invalid repository",
        ),
        (
            "an upstream not OWNER/REPO",
            |v| v["promote"]["forks"]["cgwalters-forge/bootc"]["upstream"] = "bootc".into(),
            "upstream: invalid repository",
        ),
        (
            "a fork also in follow",
            |v| v["follow"] = json!(["cgwalters-forge/bootc"]),
            "is also in follow",
        ),
        (
            "forks without signoff",
            |v| {
                v["promote"].as_object_mut().unwrap().remove("signoff");
            },
            "promote signoff is required with forks",
        ),
        (
            "forks without a policy repository",
            |v| {
                v["promote"]
                    .as_object_mut()
                    .unwrap()
                    .remove("policyRepository");
            },
            "promote policyRepository is required with forks",
        ),
        (
            "a signoff with angle brackets",
            |v| v["promote"]["signoff"]["name"] = "Op <x>".into(),
            "promote signoff name and email",
        ),
        (
            "a base with ..",
            |v| v["promote"]["forks"]["cgwalters-forge/bootc"]["base"] = "../main".into(),
            "is not a plain branch or path name",
        ),
        (
            "a fork that is its own upstream",
            |v| {
                v["promote"]["forks"]["cgwalters-forge/bootc"]["upstream"] =
                    "cgwalters-forge/bootc".into()
            },
            "names itself as upstream",
        ),
        (
            "a fork named twice, differing in case",
            |v| {
                v["promote"]["forks"]["CGWalters-forge/bootc"] =
                    json!({"upstream": "bootc-dev/bootc", "base": "main"})
            },
            "is named twice",
        ),
    ] {
        let got = with(edit).unwrap_err();
        assert!(got.contains(error), "{case}: {got}");
    }
    // Empty forks is off, and needs nothing else.
    with(|v| v["promote"] = json!({"forks": {}})).unwrap();
    let off = preview(&config(json!({"promote": {"forks": {}}})), &replay());
    assert_eq!(stage(&off), ["promote: off"]);
}

/// The pure rules a recording does not reach every corner of.
#[test]
fn rules_table() {
    let trailer = "Signed-off-by: A <a@example.com>";
    for (message, expected) in [
        ("Subject", "Subject\n\nSigned-off-by: A <a@example.com>"),
        (
            "Subject\n\nBody.\n",
            "Subject\n\nBody.\n\nSigned-off-by: A <a@example.com>",
        ),
        (
            "Subject\n\nBody.\n\nGenerated-by: x",
            "Subject\n\nBody.\n\nGenerated-by: x\nSigned-off-by: A <a@example.com>",
        ),
        (
            "Subject\n\nFixes: #1\nSee: also",
            "Subject\n\nFixes: #1\nSee: also\nSigned-off-by: A <a@example.com>",
        ),
        (
            "Subject\n\nNot a trailer here",
            "Subject\n\nNot a trailer here\n\nSigned-off-by: A <a@example.com>",
        ),
    ] {
        assert_eq!(with_trailer(message, trailer), expected, "{message:?}");
    }
    for (text, expected) in [
        ("Closes #12.", Some("#12")),
        ("Fixes GH-12", Some("GH-12")),
        ("See (#7)", Some("#7")),
        ("Closes bootc-dev/bootc#12", None),
        ("https://github.com/o/r/issues/12", None),
        ("Item #", None),
        ("C# is a language", None),
    ] {
        assert_eq!(bare_reference(text).as_deref(), expected, "{text:?}");
    }
    assert_eq!(base64("aGVsbG8=\n").unwrap(), b"hello");
    assert_eq!(base64("aGk").unwrap(), b"hi");
    assert!(base64("a!b").is_err());
    let policy = Policy::parse(&record_text("human-text")).unwrap();
    assert_eq!(policy.verdict, "human-text");
    assert_eq!(
        policy.sources,
        [(
            UPSTREAM.to_owned(),
            "CONTRIBUTING.md".to_owned(),
            "cccccccccccccccccccccccccccccccccccccccc".to_owned()
        )]
    );
    assert_eq!(policy.quote(), "> The project's rules, quoted.\n");
    for (text, error) in [
        ("no front matter", "no front matter"),
        ("---\nverdict: bot-ok\n---\n", "the record has no body"),
        ("---\nsources: []\n---\nBody", "no verdict"),
        (
            "---\nverdict: bot-ok\nsources:\n  - repo: o/r\n    path: x\n    sha: short\n---\nBody",
            "a source lacks repo, path or sha",
        ),
    ] {
        let got = format!("{:#}", Policy::parse(text).unwrap_err());
        assert!(got.contains(error), "{text:?}: {got}");
    }
    for (name, expected) in [
        ("CONTRIBUTING.md", true),
        ("AGENTS.md", true),
        ("copilot-instructions.md", true),
        ("PULL_REQUEST_TEMPLATE.md", true),
        ("DCO", true),
        ("llm-policy.rst", true),
        ("AI_USE.md", true),
        ("README.md", false),
        ("OPENAI.md", false),
        ("docs", false),
    ] {
        assert_eq!(policy_file(name), expected, "{name}");
    }
    // A signed merge rewritten on one parent proves nothing: the series
    // it came from is not linear.
    let person = Person {
        name: "A".into(),
        email: "a@example.com".into(),
        date: "2026-10-22T18:01:00Z".into(),
    };
    let commit = |sha: &str, parents: &[&str]| Commit {
        sha: sha.into(),
        tree: "a".repeat(40),
        parents: parents.iter().map(|p| p.to_string()).collect(),
        author: person.clone(),
        committer: person.clone(),
        message: format!("Merge\n\n{trailer}"),
        login: None,
    };
    let before = Series {
        merge_base: BASE.into(),
        commits: vec![commit(HEAD, &[BASE, OLDER])],
    };
    let after = Series {
        merge_base: BASE.into(),
        commits: vec![commit(FIRST, &[BASE])],
    };
    let got = format!("{:#}", prove(&before, &after, trailer).unwrap_err());
    assert!(got.contains("a merge commit is not rewritten"), "{got}");
}
