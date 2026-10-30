use super::*;
use crate::epic::{EPIC_PLAN_SCHEMA, SUB_ISSUES_QUERY};
use crate::pass::tests::{
    answer, block, config, preview, replay as board, set_usage, subs_mut, subs_request, unfinished,
    usage,
};
use std::cell::{Cell, RefCell};

/// The plan tracker#890 applied, and its receipt, as recorded.
const PLAN_890: &str = "6366132099@2026-10-23T13:34:49Z";
const RECEIPT_890: &str = "#issuecomment-6366266811";
const COMMENTS_596: &str = r#""after":null,"name":"tracker","number":596,"#;
const COMMENTS_890: &str = r#""after":null,"name":"tracker","number":890,"#;

/// `value` with every time in it moved by `by`.
fn moved(value: &mut Value, by: time::Duration) {
    match value {
        Value::String(text) => {
            if let Ok(at) = OffsetDateTime::parse(text, &Rfc3339) {
                *text = (at + by).format(&Rfc3339).unwrap();
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| moved(v, by)),
        Value::Object(map) => map.values_mut().for_each(|v| moved(v, by)),
        _ => {}
    }
}

/// When stage 3's reads of the recording were made, the evening of the
/// same day: they were recorded by `pass --record` seeded with the board's
/// recording, and appended to it.
const EPICS_READ: &str = "2026-10-23T21:04:10.085106014Z";

/// The recorded board at the time its epics were read, its synthetic usage
/// snapshot moved with it.
fn replay() -> Replay {
    let mut replay = board();
    let now = OffsetDateTime::parse(EPICS_READ, &Rfc3339).unwrap();
    let by = now - replay.now;
    replay.now = now;
    let mut snapshot = usage(&mut replay);
    moved(&mut snapshot, by);
    set_usage(&mut replay, &snapshot);
    replay
}

/// Stage 3's lines of a pass: from its first to stage 4's candidates.
fn stage(report: &Report) -> Vec<&str> {
    let lines = report.lines.iter().map(String::as_str);
    lines
        .skip_while(|l| !l.starts_with("epics:"))
        .take_while(|l| !l.starts_with("candidates:"))
        .collect()
}

/// The recorded comments of an epic, by its comments request.
fn comments<'a>(replay: &'a mut Replay, epic: &str) -> &'a mut Vec<Value> {
    answer(replay, epic)["data"]["repository"]["issue"]["comments"]["nodes"]
        .as_array_mut()
        .unwrap()
}

/// The recorded comment whose URL ends `url`.
fn comment<'a>(replay: &'a mut Replay, epic: &str, url: &str) -> &'a mut Value {
    (comments(replay, epic).iter_mut())
        .find(|c| text(c, "/url").ends_with(url))
        .unwrap()
}

/// A comment on tracker#`n` by `author`, `minutes` before the recording.
fn posted(replay: &Replay, n: u64, id: u64, author: &str, minutes: i64, body: &str) -> Value {
    let at = replay.now - time::Duration::minutes(minutes);
    json!({"url": format!("https://github.com/cgwalters-forge/tracker/issues/{n}#issuecomment-{id}"),
        "body": body, "createdAt": at.format(&Rfc3339).unwrap(), "lastEditedAt": null,
        "author": {"login": author}, "editor": null})
}

/// A planning request on tracker#596 `minutes` before the recording, after
/// its plan: the record of a dispatch whose run is 77.
fn requested(replay: &mut Replay, minutes: i64) {
    let record = posted(
        replay,
        596,
        7_000_000_001,
        "cgwalters-bot",
        minutes,
        "<!-- agent-board-dispatch: 7000000000@2026-10-23T20:03:00Z -->\nDispatched the plan in https://github.com/cgwalters-forge/tracker/issues/596#issuecomment-7000000000: [run](https://github.com/cgwalters-forge/tracker/actions/runs/77).",
    );
    comments(replay, COMMENTS_596).push(record);
}

/// [`requested`] eight hours before the recording as the retry of an earlier
/// request, its run 77 ended in failure seven hours before.
fn retried(replay: &mut Replay) {
    let intro = format!(
        "{RETRY_INTRO}https://github.com/cgwalters-forge/tracker/issues/596#issuecomment-6999999999 got no plan."
    );
    let plan = posted(
        replay,
        596,
        7_000_000_000,
        "cgwalters-bot",
        8 * 60 + 1,
        &intro,
    );
    comments(replay, COMMENTS_596).push(plan);
    requested(replay, 8 * 60);
    run_77(replay, Some(7 * 60));
}

/// Run 77 as GitHub answers it: finished in failure `minutes` before the
/// recording, or in progress.
fn run_77(replay: &mut Replay, minutes: Option<i64>) {
    let body = match minutes {
        Some(m) => {
            let at = replay.now - time::Duration::minutes(m);
            json!({"id": 77, "status": "completed", "conclusion": "failure",
                "updated_at": at.format(&Rfc3339).unwrap()})
        }
        None => json!({"id": 77, "status": "in_progress"}),
    };
    replay.requests.push(json!({
        "request": {"get": format!("{API_ROOT}repos/cgwalters-forge/tracker/actions/runs/77")},
        "answer": {"status": 200, "link": null, "body": body},
    }));
}

/// The recorded listing of the epic tracker#`n`, without its sub-issues.
fn listed(replay: &Replay, n: u64) -> &Value {
    (replay.requests.iter())
        .filter_map(|e| e["answer"].pointer("/data/repository/issues/nodes"))
        .flat_map(|nodes| nodes.as_array().unwrap())
        .find(|node| node["number"] == n)
        .unwrap()
}

/// The recorded census of tracker#`n`: its listing with its sub-issues, as
/// stage 3 puts them together for [`EpicFacts::from_node`].
fn node(replay: &Replay, n: u64) -> Value {
    let mut node = listed(replay, n).clone();
    let request = subs_request(n, Value::Null);
    let subs = (replay.requests.iter())
        .find(|e| e["request"] == request)
        .and_then(|e| e["answer"].pointer("/data/repository/issue/subIssues"))
        .unwrap();
    node["subIssues"] = json!({"nodes": subs["nodes"], "totalCount": subs["totalCount"]});
    node
}

/// The epic as `epic apply` reads it, from the recorded census.
fn epic_answer(replay: &Replay, n: u64) -> Value {
    let node = node(replay, n);
    let subs: Vec<Value> = (node["subIssues"]["nodes"].as_array().unwrap().iter())
        .map(|s| json!({"number": s["number"], "state": s["state"], "repository": s["repository"]}))
        .collect();
    json!({"repository": {"issue": {"id": format!("I_{n}"), "state": "OPEN",
        "labels": {"nodes": [{"name": "epic"}]},
        "subIssues": {"nodes": subs, "pageInfo": {"hasNextPage": false}}}}})
}

/// Answers `epic apply`'s read of tracker#`n` from the recorded census.
fn census(replay: &mut Replay, n: u64) {
    let variables = json!({"owner": "cgwalters-forge", "name": "tracker", "number": n});
    let answer = epic_answer(replay, n);
    replay.requests.push(json!({
        "request": graphql_key(SUB_ISSUES_QUERY, &variables),
        "answer": {"data": answer},
    }));
}

/// A plan on tracker#596 five minutes before the recording, of `schema`
/// and with `actions`, with the epic as `epic apply` reads it.
fn plan_596(replay: &mut Replay, schema: &str, actions: Value) {
    let body = format!(
        "```agent-board-epic-plan\n{}\n```",
        json!({"schema": schema, "epic": "cgwalters-forge/tracker#596", "actions": actions})
    );
    let plan = posted(replay, 596, 7_000_000_003, "cgwalters-bot", 5, &body);
    comments(replay, COMMENTS_596).push(plan);
    census(replay, 596);
}

/// A plan on tracker#596 that apply refuses.
fn invalid_596(replay: &mut Replay) {
    plan_596(replay, "nope", json!([]));
}

/// A plan on tracker#596 with `action` on agentic-job#630, a sub-issue
/// that closed after it was written.
fn stale_596(replay: &mut Replay, action: Value) {
    let mut action = action;
    action["issue"] = "cgwalters-forge/agentic-job#630".into();
    plan_596(replay, EPIC_PLAN_SCHEMA, json!([action]));
}

/// [`stale_596`] rescoping agentic-job#630, which apply refuses.
fn rescoped_596(replay: &mut Replay) {
    let rescope = json!({"type": "rescope_sub_issue", "body": "Smaller.", "reason": "Split."});
    stale_596(replay, rescope);
}

/// tracker#890 without its receipt: its plan as if never applied, which
/// `epic apply` may then read.
fn unapplied(replay: &mut Replay) {
    comments(replay, COMMENTS_890).retain(|c| !text(c, "/url").ends_with(RECEIPT_890));
    census(replay, 890);
}

/// `author`'s comment on tracker#596, a minute before the recording, that
/// its recorded sub-issues are all closed.
fn said_done(replay: &mut Replay, author: &str) {
    let board = config(json!({})).board.unwrap();
    let facts = EpicFacts::from_node(&node(replay, 596), Some(&board)).unwrap();
    let body = format!("{}\nEvery sub-issue is closed.", done_marker(&facts.census));
    let said = posted(replay, 596, 7_000_000_009, author, 1, &body);
    comments(replay, COMMENTS_596).push(said);
}

/// Labels tracker#596 with each of `labels`.
fn label_596(replay: &mut Replay, labels: &[&str]) {
    label(replay, 596, labels);
}

/// Labels the epic tracker#`n` with each of `labels`.
fn label(replay: &mut Replay, n: u64, labels: &[&str]) {
    let nodes = (replay.requests.iter_mut())
        .filter_map(|e| e["answer"].pointer_mut("/data/repository/issues/nodes"))
        .filter_map(Value::as_array_mut)
        .flatten();
    for node in nodes.filter(|n2| n2["number"] == n) {
        let held = &mut node["labels"];
        for label in labels {
            held["nodes"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name": label}));
        }
        held["totalCount"] = (held["totalCount"].as_u64().unwrap() + labels.len() as u64).into();
    }
}

/// [`said_done`] by the bot and labelled `labels`, then the operator's
/// comment, at the recording.
fn answered_done(replay: &mut Replay, labels: &[&str]) {
    said_done(replay, "cgwalters-bot");
    label_596(replay, labels);
    let answer = posted(replay, 596, 7_000_000_010, "cgwalters", 0, "Docs next.");
    comments(replay, COMMENTS_596).push(answer);
}

/// The live board of the evening of 23 October with a fresh usage
/// snapshot, tracker#596 given an open sub-issue: #596, frozen since
/// tracker#851 by a coordinator that matched its sub-issues by title, is
/// planned; tracker#890's plan is recognised as applied from its receipt,
/// though most of the sub-issues it created have closed since and one was
/// reprioritised, so nothing is created again; it waits a day after its
/// last plan; #766 is held by its `needs-human` label.
#[test]
fn replay_plans_one_epic_and_recreates_nothing() {
    let mut replay = replay();
    unfinished(&mut replay);
    let report = preview(&config(json!({})), &replay);
    assert_eq!(
        stage(&report),
        [
            "epics: 3 live of 3 open in cgwalters-forge/tracker",
            "start planning cgwalters-forge/tracker#596 pool=claude: its sub-issues changed (cgwalters-forge/agentic-job#630 closed, cgwalters-forge/agentic-job#715 closed, cgwalters-forge/agentic-job#716 closed and 4 more)",
            "hold epic cgwalters-forge/tracker#890: its sub-issues changed (cgwalters-forge/agent-board#285 closed, cgwalters-forge/agent-board#286 closed, cgwalters-forge/agent-board#287 closed and 5 more); planned again from 2026-10-24T13:34:49Z, 24 hours after its last request",
            "hold epic cgwalters-forge/tracker#766: labelled needs-human; otherwise due to be planned (its sub-issues changed (cgwalters-forge/actions#83 closed, cgwalters-forge/agent-board#167 closed, cgwalters-forge/agent-board#168 closed and 2 more))",
        ]
    );
    assert!(!report.lines.iter().any(|l| l.contains("apply epic")));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    // The planning run counts against --max-dispatch.
    let capped = Limits {
        dispatch: Some(0),
        writes: Some(0),
        ..Limits::default()
    };
    let report = block(run(
        &replay,
        &replay,
        &config(json!({})),
        State::default(),
        (Mode::Preview, capped),
        replay.now,
    ))
    .unwrap();
    let lines = stage(&report);
    assert!(
        lines[1].starts_with(
            "held by --max-dispatch: start planning cgwalters-forge/tracker#596 pool=claude: "
        ),
        "{lines:#?}"
    );
    assert!(
        lines[2].ends_with(", 24 hours after its last request"),
        "{lines:#?}"
    );
}

/// tracker#596 with 60 sub-issues, more than a page of them: 9 recorded and
/// closed, and 51 open ones, read in two pages after the cursor of the
/// first. All are planned for; were the second page lost, or read twice,
/// the epic could not be read.
#[test]
fn replay_pages_an_epics_sub_issues() {
    const SUBS: usize = 60;
    const CURSOR: &str = "Y3Vyc29yOjUw";
    let mut replay = replay();
    let subs = subs_mut(&mut replay, 596);
    let recorded = subs["nodes"].as_array().unwrap().clone();
    let mut nodes = recorded.clone();
    nodes.extend((recorded.len()..SUBS).map(|i| {
        let mut open = recorded[0].clone();
        open["number"] = (10_000 + i).into();
        open["state"] = "OPEN".into();
        open["repository"] = json!({"nameWithOwner": "cgwalters-forge/tracker"});
        open
    }));
    let (first, second) = nodes.split_at(SUBS_PER_PAGE);
    *subs = json!({"nodes": first, "totalCount": SUBS,
        "pageInfo": {"hasNextPage": true, "endCursor": CURSOR}});
    let page = |nodes: &[Value]| {
        json!({"data": {"repository": {"issue": {"subIssues": {"nodes": nodes, "totalCount": SUBS,
            "pageInfo": {"hasNextPage": false, "endCursor": null}}}}}})
    };
    (replay.requests)
        .push(json!({"request": subs_request(596, CURSOR.into()), "answer": page(second)}));
    let report = preview(&config(json!({})), &replay);
    let lines = stage(&report);
    let planned = (lines.iter())
        .find(|l| l.starts_with("start planning cgwalters-forge/tracker#596 "))
        .unwrap_or_else(|| panic!("{lines:#?}"));
    // Each of the 51 added, and the 6 closed since its plan, counted.
    let changed = SUBS - recorded.len() + 6;
    assert!(
        planned.ends_with(&format!(" and {} more)", changed - 3)),
        "{planned}"
    );
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    // Its second page changed between the reads, one fewer or one read
    // again: the epic is not read whole, and is held for it.
    let changed = "the sub-issues of cgwalters-forge/tracker#596 changed while they were read";
    for edit in [&second[1..], &[first[0].clone()]] {
        let mut moved = Replay {
            now: replay.now,
            requests: replay.requests.clone(),
        };
        moved.requests.last_mut().unwrap()["answer"] = page(edit);
        let report = preview(&config(json!({})), &moved);
        assert!(
            (report.lines.iter()).any(|l| l.starts_with("hold epic cgwalters-forge/tracker#596: ")
                && l.ends_with(changed)),
            "{:#?}",
            report.lines
        );
    }
    // A read that fails, here the second page's, fails the stage rather than
    // hold the one epic, as a failed listing does.
    let mut failed = Replay {
        now: replay.now,
        requests: replay.requests.clone(),
    };
    failed.requests.last_mut().unwrap()["answer"] = json!({"error": "rate limited"});
    let report = preview(&config(json!({})), &failed);
    assert!(
        report
            .lines
            .contains(&"epics error: rate limited".to_owned()),
        "{:#?}",
        report.lines
    );
    assert_eq!(report.failures, 1, "{:#?}", report.lines);
}

type Edit = Box<dyn Fn(&mut Replay)>;
/// A hold's kind, and whether it waits on the operator.
type Held = Option<(HoldKind, bool)>;

/// Each edit of the recording, the line stage 3 must then print for an
/// epic (the start of it), how many operations fail, and the kind of hold
/// that line is and whether it waits on the operator, where it is one.
#[test]
fn replay_epic_scenarios() {
    let receipt_890 = |replay: &mut Replay, body: &dyn Fn(&str) -> String| {
        let receipt = comment(replay, COMMENTS_890, RECEIPT_890);
        receipt["body"] = body(text(receipt, "/body")).into();
    };
    let cases: Vec<(&str, Edit, &str, usize, Held)> = vec![
        (
            "a receipt that stopped, its writes all listed, is recovered and not replayed",
            Box::new(move |r| {
                receipt_890(r, &|body| {
                    body.replacen(": Applied.", ": Stopped after a failed request; its outcome may be ambiguous. Inspect before replacing the plan.", 1)
                });
            }),
            "recover epic cgwalters-forge/tracker#890 plan=6366132099@2026-10-23T13:34:49Z: its apply was stopped after a failed request, and its receipt and the epic confirm every action's effect; would record that on https://github.com/cgwalters-forge/tracker/issues/890#issuecomment-6366266811",
            0,
            None,
        ),
        (
            "a claim that confirms nothing is left for a person, never replayed",
            Box::new(move |r| {
                receipt_890(r, &|body| {
                    let first = body.lines().next().unwrap();
                    format!(
                        "{first}\nApplying https://github.com/cgwalters-forge/tracker/issues/890#issuecomment-6366132099. Do not replay this plan after a failure."
                    )
                });
            }),
            "hold epic cgwalters-forge/tracker#890: the apply of plan 6366132099@2026-10-23T13:34:49Z was claimed and never finished, and is not proved: action 1 (create_sub_issue): no confirmed issue for its sub-issue in cgwalters-forge/tracker; ",
            0,
            Some((HoldKind::NeedsOperator, true)),
        ),
        (
            "without its receipt, the plan would be applied: the receipt is what stops it",
            Box::new(unapplied),
            "apply epic cgwalters-forge/tracker#890 plan=6366132099@2026-10-23T13:34:49Z: Create a sub-issue in cgwalters-forge/tracker (P0); Create a sub-issue in cgwalters-forge/agent-board (P0); Create a sub-issue in cgwalters-forge/tracker (P0); Create a sub-issue in cgwalters-forge/tracker (P0); Create a sub-issue in cgwalters-forge/agent-board (P1); Create a sub-issue in cgwalters-forge/agent-board (P2); ",
            0,
            None,
        ),
        (
            "the operator's comment is direction; the bot's own are not",
            Box::new(|r| {
                let direction = posted(
                    r,
                    890,
                    7_000_000_002,
                    "cgwalters",
                    10,
                    "Fold the release train into the pass.",
                );
                comments(r, COMMENTS_890).push(direction);
            }),
            "hold epic cgwalters-forge/tracker#890: due (operator direction since it was last planned); one epic is planned a pass",
            0,
            Some((HoldKind::Other, false)),
        ),
        (
            "a planning request whose run finished a minute ago waits for its plan",
            Box::new(|r| {
                requested(r, 60);
                run_77(r, Some(1));
            }),
            "hold epic cgwalters-forge/tracker#596: its planning request https://github.com/cgwalters-forge/tracker/issues/596#issuecomment-7000000001 waits for its plan",
            0,
            Some((HoldKind::Dispatched, false)),
        ),
        (
            "a planning request whose run finished without a plan is retried after a cooldown",
            Box::new(|r| {
                requested(r, 60);
                run_77(r, Some(10));
            }),
            "hold epic cgwalters-forge/tracker#596: its planning request https://github.com/cgwalters-forge/tracker/issues/596#issuecomment-7000000001 got no plan; planned once more from 2026-10-24T02:54:10Z, six hours after its run ended",
            0,
            Some((HoldKind::EpicUnplanned, false)),
        ),
        (
            "and once the cooldown has passed, retried once",
            Box::new(|r| {
                requested(r, 8 * 60);
                run_77(r, Some(7 * 60));
            }),
            "start planning cgwalters-forge/tracker#596 pool=claude: its planning request https://github.com/cgwalters-forge/tracker/issues/596#issuecomment-7000000001 got no plan, retried once",
            0,
            None,
        ),
        (
            "a retry that got no plan either is held and asks the operator",
            Box::new(retried),
            "hold epic cgwalters-forge/tracker#596: its planning request https://github.com/cgwalters-forge/tracker/issues/596#issuecomment-7000000001 got no plan, nor did https://github.com/cgwalters-forge/tracker/issues/596#issuecomment-6999999999 before it; a person comments on the epic to plan it again; label needs-human, Why Q: Planning this epic got no plan twice (its run 77 ended failure). What should change?",
            0,
            Some((HoldKind::EpicUnplanned, true)),
        ),
        (
            "with what the planner said, where it said something",
            Box::new(|r| {
                retried(r);
                let said = posted(
                    r,
                    596,
                    7_000_000_005,
                    "cgwalters-bot",
                    7 * 60,
                    "\nThe brief is too large to plan in one run.\n\nMore detail.",
                );
                comments(r, COMMENTS_596).push(said);
            }),
            "hold epic cgwalters-forge/tracker#596: its planning request https://github.com/cgwalters-forge/tracker/issues/596#issuecomment-7000000001 got no plan, nor did https://github.com/cgwalters-forge/tracker/issues/596#issuecomment-6999999999 before it; a person comments on the epic to plan it again; label needs-human, Why Q: Planning this epic got no plan twice (The brief is too large to plan in one run.). What should change?",
            0,
            Some((HoldKind::EpicUnplanned, true)),
        ),
        (
            "a planning run in flight holds its epic",
            Box::new(|r| {
                requested(r, 60);
                let body = &mut answer(r, "status=queued")["body"];
                body["total_count"] = 1.into();
                body["workflow_runs"] = json!([{"id": 77, "display_title": "triage cgwalters-forge/tracker#596 (claude)",
                    "html_url": "https://github.com/cgwalters-forge/tracker/actions/runs/77"}]);
            }),
            "hold epic cgwalters-forge/tracker#596: its planning run is in flight, https://github.com/cgwalters-forge/tracker/actions/runs/77",
            0,
            Some((HoldKind::InFlight, false)),
        ),
        (
            "a newest plan that is not JSON holds, and is never read as no plan",
            Box::new(|r| {
                let plan = posted(
                    r,
                    596,
                    7_000_000_003,
                    "cgwalters-bot",
                    5,
                    "```agent-board-epic-plan\n{\"schema\":\n```",
                );
                comments(r, COMMENTS_596).push(plan);
            }),
            "hold epic cgwalters-forge/tracker#596: its newest epic plan is invalid: newest epic plan https://github.com/cgwalters-forge/tracker/issues/596#issuecomment-7000000003: plan is not valid JSON",
            0,
            Some((HoldKind::NeedsOperator, true)),
        ),
        (
            "a newest plan apply refuses holds for a person",
            Box::new(|r| {
                invalid_596(r);
            }),
            "hold epic cgwalters-forge/tracker#596: plan 7000000003@2026-10-23T20:59:10.085106014Z is not applied: the plan cannot be applied: unsupported epic plan schema",
            0,
            Some((HoldKind::NeedsOperator, true)),
        ),
        (
            "a refused plan does not take the one apply a pass (agent-board#453)",
            Box::new(|r| {
                invalid_596(r);
                unapplied(r);
            }),
            "apply epic cgwalters-forge/tracker#890 plan=6366132099@2026-10-23T13:34:49Z: Create a sub-issue",
            0,
            None,
        ),
        (
            "a Priority for a sub-issue closed since the plan is satisfied, and the plan applies",
            Box::new(|r| {
                stale_596(r, json!({"type": "set_priority", "value": "P1"}));
                unapplied(r);
            }),
            "apply epic cgwalters-forge/tracker#596 plan=7000000003@2026-10-23T20:59:10.085106014Z: cgwalters-forge/agentic-job#630 is already closed",
            0,
            None,
        ),
        (
            "by default one accepted plan is applied a pass, and the next waits",
            Box::new(|r| {
                stale_596(r, json!({"type": "set_priority", "value": "P1"}));
                unapplied(r);
            }),
            "hold epic cgwalters-forge/tracker#890: plan 6366132099@2026-10-23T13:34:49Z waits; one epic plan is applied a pass",
            0,
            Some((HoldKind::Other, false)),
        ),
        (
            "with epics.maxAppliesPerPass 2, both apply in one pass (agent-board#455)",
            Box::new(|r| {
                stale_596(r, json!({"type": "set_priority", "value": "P1"}));
                unapplied(r);
            }),
            "apply epic cgwalters-forge/tracker#890 plan=6366132099@2026-10-23T13:34:49Z: Create a sub-issue",
            0,
            None,
        ),
        (
            "a plan refused for a sub-issue closed since is planned again, not held for a person",
            Box::new(rescoped_596),
            "start planning cgwalters-forge/tracker#596 pool=claude: plan 7000000003@2026-10-23T20:59:10.085106014Z names a sub-issue that has closed since",
            0,
            None,
        ),
        (
            "and while that planning is asked for, it waits without the operator",
            Box::new(|r| {
                rescoped_596(r);
                requested(r, 1);
            }),
            "hold epic cgwalters-forge/tracker#596: plan 7000000003@2026-10-23T20:59:10.085106014Z is not applied: the plan cannot be applied: epic plan refused:\n  action 1 (rescope_sub_issue): cgwalters-forge/agentic-job#630 is a closed sub-issue; a plan may not change it; it was planned again, which it waits for",
            0,
            Some((HoldKind::Other, false)),
        ),
        (
            "a stale plan that answers an ordinary request is planned again, as tracker#939's is",
            Box::new(|r| {
                requested(r, 60);
                rescoped_596(r);
            }),
            "start planning cgwalters-forge/tracker#596 pool=claude: plan 7000000003@2026-10-23T20:59:10.085106014Z names a sub-issue that has closed since",
            0,
            None,
        ),
        (
            "but one that answers a request to plan again because its plan went stale is the operator's",
            Box::new(|r| {
                let intro = posted(
                    r,
                    596,
                    7_000_000_000,
                    "cgwalters-bot",
                    61,
                    &format!("{STALE_INTRO}."),
                );
                comments(r, COMMENTS_596).push(intro);
                requested(r, 60);
                rescoped_596(r);
            }),
            "hold epic cgwalters-forge/tracker#596: plan 7000000003@2026-10-23T20:59:10.085106014Z is not applied: the plan cannot be applied: epic plan refused:\n  action 1 (rescope_sub_issue): cgwalters-forge/agentic-job#630 is a closed sub-issue; a plan may not change it; it answers a request to plan again because the plan before it went stale, so a person replaces it",
            0,
            Some((HoldKind::NeedsOperator, true)),
        ),
        (
            "and a request to plan again that got no plan in a day is the operator's",
            Box::new(|r| {
                rescoped_596(r);
                let plan = comments(r, COMMENTS_596).last_mut().unwrap();
                moved(plan, -time::Duration::days(2));
                let intro = posted(
                    r,
                    596,
                    7_000_000_000,
                    "cgwalters-bot",
                    24 * 60 + 1,
                    &format!("{STALE_INTRO}."),
                );
                comments(r, COMMENTS_596).push(intro);
                requested(r, 24 * 60);
            }),
            "hold epic cgwalters-forge/tracker#596: plan 7000000003@2026-10-21T20:59:10.085106014Z is not applied: the plan cannot be applied: epic plan refused:\n  action 1 (rescope_sub_issue): cgwalters-forge/agentic-job#630 is a closed sub-issue; a plan may not change it; the request to plan it again got no plan in a day, so a person replaces it",
            0,
            Some((HoldKind::NeedsOperator, true)),
        ),
        (
            "and a day after that request, it is planned again",
            Box::new(|r| {
                rescoped_596(r);
                // The plan two days old, and its request after it a day old.
                let plan = comments(r, COMMENTS_596).last_mut().unwrap();
                moved(plan, -time::Duration::days(2));
                requested(r, 24 * 60);
            }),
            "start planning cgwalters-forge/tracker#596 pool=claude: plan 7000000003@2026-10-21T20:59:10.085106014Z names a sub-issue that has closed since",
            0,
            None,
        ),
        (
            "and the operator's direction since plans it again",
            Box::new(|r| {
                invalid_596(r);
                let direction = posted(r, 596, 7_000_000_004, "cgwalters", 1, "Plan it again.");
                comments(r, COMMENTS_596).push(direction);
            }),
            "start planning cgwalters-forge/tracker#596 pool=claude: operator direction since its plan, which cannot be applied",
            0,
            None,
        ),
        (
            "done: an epic whose sub-issues are all closed is asked about, not planned",
            Box::new(|_| {}),
            "hold epic cgwalters-forge/tracker#596: every sub-issue is closed; its sub-issues changed (cgwalters-forge/agentic-job#630 closed, cgwalters-forge/agentic-job#715 closed, cgwalters-forge/agentic-job#716 closed and 3 more); label needs-human, Why Q: Every sub-issue of this epic is closed. Close it, or comment with what is left?",
            0,
            Some((HoldKind::NeedsHuman, true)),
        ),
        (
            "done: once, for those sub-issues",
            Box::new(|r| said_done(r, "cgwalters-bot")),
            "epic cgwalters-forge/tracker#596: every sub-issue is closed, as https://github.com/cgwalters-forge/tracker/issues/596#issuecomment-7000000009 says",
            0,
            None,
        ),
        (
            "done: while its label holds it, the question stays the operator's",
            Box::new(|r| {
                said_done(r, "cgwalters-bot");
                label_596(r, &["needs-human"]);
            }),
            "hold epic cgwalters-forge/tracker#596: labelled needs-human; every sub-issue is closed, as https://github.com/cgwalters-forge/tracker/issues/596#issuecomment-7000000009 says",
            0,
            Some((HoldKind::NeedsHuman, true)),
        ),
        (
            "done: the operator's comment answers the ask, releasing its label",
            Box::new(|r| answered_done(r, &["needs-human"])),
            "release epic cgwalters-forge/tracker#596: the operator answered https://github.com/cgwalters-forge/tracker/issues/596#issuecomment-7000000009; unlabel needs-human",
            0,
            None,
        ),
        (
            "done: the answer, released, plans it",
            Box::new(|r| answered_done(r, &["needs-human"])),
            "start planning cgwalters-forge/tracker#596 pool=claude: operator direction since it was last planned",
            0,
            None,
        ),
        (
            "done: the answer releases no other hold",
            Box::new(|r| answered_done(r, &["needs-human", "needs-design"])),
            "hold epic cgwalters-forge/tracker#596: labelled needs-human; otherwise due to be planned (operator direction since it was last planned)",
            0,
            Some((HoldKind::NeedsHuman, true)),
        ),
        (
            "done: a visitor's marker is not the bot's",
            Box::new(|r| said_done(r, "visitor")),
            "hold epic cgwalters-forge/tracker#596: every sub-issue is closed; ",
            0,
            Some((HoldKind::NeedsHuman, true)),
        ),
        (
            "done: with epics.close, it is closed instead",
            Box::new(|_| {}),
            "close epic cgwalters-forge/tracker#596: every sub-issue is closed; its sub-issues changed (",
            0,
            None,
        ),
        (
            "done: the operator's direction still plans it",
            Box::new(|r| {
                let direction = posted(r, 596, 7_000_000_004, "cgwalters", 1, "Next: the docs.");
                comments(r, COMMENTS_596).push(direction);
            }),
            "start planning cgwalters-forge/tracker#596 pool=claude: operator direction since it was last planned",
            0,
            None,
        ),
        (
            "no planning while new work is off",
            Box::new(|_| {}),
            "hold epic cgwalters-forge/tracker#596: due (its sub-issues changed",
            0,
            Some((HoldKind::Other, false)),
        ),
    ];
    for (name, edit, expected, failures, kind) in cases {
        let mut replay = replay();
        // tracker#596's sub-issues are all closed, which only the cases
        // about that leave so.
        if !name.starts_with("done: ") {
            unfinished(&mut replay);
        }
        edit(&mut replay);
        let settings = if name.starts_with("no planning") {
            json!({"wip": 0})
        } else if name.contains("maxAppliesPerPass") {
            json!({"epics": {"repository": "cgwalters-forge/tracker", "maxAppliesPerPass": 2}})
        } else if name.contains("epics.close") {
            json!({"epics": {"repository": "cgwalters-forge/tracker", "close": true}})
        } else {
            json!({})
        };
        let report = preview(&config(settings), &replay);
        let lines = stage(&report);
        assert!(
            lines.iter().any(|l| l.starts_with(expected)),
            "{name}: no line {expected:?} in {lines:#?}"
        );
        assert_eq!(report.failures, failures, "{name}: {:#?}", report.lines);
        let held = (report.holds.iter()).find(|h| h.line().starts_with(expected));
        let held = held.map(|h| (h.hold.kind, h.hold.operator));
        assert_eq!(held, kind, "{name}");
    }
}

/// The recording as a forge that takes writes: comments posted and edited
/// show in later reads of the epic, issues get numbers from 1000, and every
/// write is kept.
struct Forge {
    replay: Replay,
    added: RefCell<Vec<Value>>,
    writes: RefCell<Vec<(String, Value)>>,
    ids: Cell<u64>,
    /// Refuses a board Why update.
    why_fails: Cell<bool>,
    /// Edits each listing of the epics after the first, and each read of
    /// sub-issues after it: what changed while the pass went through them.
    relisted: Cell<Option<fn(&mut Value)>>,
    listings: Cell<usize>,
    comment_reads: RefCell<BTreeMap<u64, usize>>,
    changed_plan: Cell<Option<(u64, usize)>>,
    link_fails: Cell<bool>,
}

impl Forge {
    fn new(replay: Replay) -> Self {
        Self {
            replay,
            added: RefCell::new(vec![]),
            writes: RefCell::new(vec![]),
            ids: Cell::new(1000),
            why_fails: Cell::new(false),
            relisted: Cell::new(None),
            listings: Cell::new(0),
            comment_reads: RefCell::new(BTreeMap::new()),
            changed_plan: Cell::new(None),
            link_fails: Cell::new(false),
        }
    }

    fn next(&self) -> u64 {
        self.ids.set(self.ids.get() + 1);
        self.ids.get()
    }

    fn kept(&self, what: String, body: Value) {
        self.writes.borrow_mut().push((what, body));
    }

    /// The writes, as `METHOD path` or `graphql mutation`.
    fn written(&self) -> Vec<String> {
        self.writes
            .borrow()
            .iter()
            .map(|(w, _)| w.clone())
            .collect()
    }
}

impl Broker for Forge {
    fn usage(&self, url: &str) -> Result<Value> {
        self.replay.usage(url)
    }

    fn grant(&self, url: &str) -> Result<Value> {
        self.replay.grant(url)
    }
}

impl Rest for Forge {
    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
        self.replay.get(url, validator)
    }

    fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        let path = url.trim_start_matches(API_ROOT);
        self.kept(format!("{method} {path}"), body.clone());
        let id = self.next();
        if method == "POST" && path.ends_with("/issues") {
            return Ok((201, json!({"number": id})));
        }
        let commented = (path.strip_prefix("repos/cgwalters-forge/tracker/issues/"))
            .and_then(|rest| rest.strip_suffix("/comments")?.parse::<u64>().ok());
        if method == "POST"
            && let Some(n) = commented
        {
            let html_url =
                format!("https://github.com/cgwalters-forge/tracker/issues/{n}#issuecomment-{id}");
            let mut node = posted(&self.replay, n, id, "cgwalters-bot", 0, text(body, "/body"));
            node["url"] = html_url.clone().into();
            self.added.borrow_mut().push(node);
            return Ok((201, json!({"id": id, "html_url": html_url})));
        }
        if method == "PATCH" && path.starts_with("repos/cgwalters-forge/tracker/issues/comments/") {
            let edited = path.rsplit('/').next().unwrap();
            for node in self.added.borrow_mut().iter_mut() {
                if text(node, "/url").ends_with(&format!("-{edited}")) {
                    node["body"] = body["body"].clone();
                }
            }
        }
        Ok((200, json!({"id": id})))
    }
}

impl crate::github::GraphQl for Forge {
    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        Transport::graphql(self, query, variables)
    }
}

impl Transport for Forge {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        if query.contains("viewer {") {
            return Ok(json!({"viewer": {"login": "cgwalters-bot"}}));
        }
        if let Some(name) = [
            "addSubIssue(",
            "addProjectV2ItemById(",
            "updateProjectV2ItemFieldValue(",
        ]
        .iter()
        .find(|m| query.contains(*m))
        {
            if self.link_fails.get() && *name == "addSubIssue(" {
                bail!("link refused after create");
            }
            if self.why_fails.get() && variables["field"] == "F_why" {
                bail!("{WHY_FIELD} update refused");
            }
            let item = variables["item"].clone();
            self.kept(format!("graphql {}", name.trim_end_matches('(')), variables);
            return Ok(json!({"addProjectV2ItemById": {"item": {"id": "PVTI_new"}},
                "updateProjectV2ItemFieldValue": {"projectV2Item": {"id": item}}}));
        }
        if query.contains("issue(number: $number) { id } }") {
            return Ok(
                json!({"repository": {"issue": {"id": format!("I_{}", variables["number"])}}}),
            );
        }
        if query == SUB_ISSUES_QUERY {
            return Ok(epic_answer(
                &self.replay,
                variables["number"].as_u64().unwrap(),
            ));
        }
        let mut data = self.replay.graphql(query, variables.clone())?;
        if query == EPICS_QUERY {
            self.listings.set(self.listings.get() + 1);
        }
        if (query == EPICS_QUERY || query == EPIC_SUBS_QUERY)
            && let Some(edit) = self.relisted.get().filter(|_| self.listings.get() > 1)
        {
            edit(&mut data);
        }
        if let Some(nodes) = data
            .pointer_mut("/repository/issue/comments/nodes")
            .and_then(Value::as_array_mut)
        {
            let number = variables["number"].as_u64().unwrap();
            let mut reads = self.comment_reads.borrow_mut();
            let count = reads.entry(number).or_default();
            *count += 1;
            if self
                .changed_plan
                .get()
                .is_some_and(|(n, at)| n == number && *count >= at)
            {
                for node in nodes
                    .iter_mut()
                    .filter(|n| has_fence(text(n, "/body"), EPIC_PLAN_FENCE))
                {
                    node["lastEditedAt"] = self.replay.now.format(&Rfc3339).unwrap().into();
                    node["editor"] = json!({"login": "cgwalters-bot"});
                }
            }
            let on = format!("/issues/{}#", variables["number"]);
            let added = self.added.borrow();
            let here = added.iter().filter(|c| text(c, "/url").contains(&on));
            nodes.extend(here.cloned());
        }
        Ok(data)
    }
}

/// Three pending plans in scheduling order. Only the first is invalid.
fn queued_plans() -> Replay {
    let mut replay = replay();
    invalid_596(&mut replay);
    let third = {
        let mut node = listed(&replay, 890).clone();
        node["number"] = 891.into();
        node
    };
    let subs = subs_mut(&mut replay, 890).clone();
    (replay.requests).push(json!({"request": subs_request(891, Value::Null),
        "answer": {"data": {"repository": {"issue": {"subIssues": subs}}}}}));
    for entry in &mut replay.requests {
        if let Some(nodes) = entry["answer"].pointer_mut("/data/repository/issues/nodes") {
            let nodes = nodes.as_array_mut().unwrap();
            nodes.retain(|n| n["number"] != 766);
            nodes.push(third.clone());
        }
    }
    let mut third_comments = replay
        .requests
        .iter()
        .find(|e| e["request"].to_string().contains(COMMENTS_890))
        .unwrap()
        .clone();
    third_comments["request"]["variables"]["number"] = 891.into();
    third_comments["answer"]["data"]["repository"]["issue"]["comments"]["nodes"] = json!([]);
    replay.requests.push(third_comments);
    let third_reads: Vec<_> = replay
        .requests
        .iter()
        .filter(|e| {
            e["request"]["variables"]["number"] == 890
                && e["answer"]
                    .pointer("/data/repository/issue/comments")
                    .is_none()
        })
        .map(|e| {
            let mut copy = e.clone();
            copy["request"]["variables"]["number"] = 891.into();
            copy
        })
        .collect();
    replay.requests.extend(third_reads);
    for (number, needle) in [
        (890, COMMENTS_890),
        (891, "\"after\":null,\"name\":\"tracker\",\"number\":891,"),
    ] {
        let body = format!(
            "```{EPIC_PLAN_FENCE}\n{}\n```",
            json!({
                "schema": crate::epic::EPIC_PLAN_SCHEMA,
                "epic": format!("cgwalters-forge/tracker#{number}"), "actions": []
            })
        );
        let plan = posted(
            &replay,
            number,
            number + 7_000_000_000,
            "cgwalters-bot",
            5,
            &body,
        );
        *comments(&mut replay, needle) = vec![plan];
        census(&mut replay, number);
    }
    replay
}

#[test]
fn refused_first_valid_second_and_third_waits() {
    for apply in [false, true] {
        let forge = Forge::new(queued_plans());
        let report = if apply {
            local(&forge, 10)
        } else {
            preview(&config(json!({})), &forge.replay)
        };
        let lines = stage(&report);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("#596: plan") && l.contains("is not applied")),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("apply epic cgwalters-forge/tracker#890")),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("#891:") && l.contains("one epic plan is applied a pass")),
            "{lines:#?}"
        );
        assert_eq!(report.failures, 0, "{lines:#?}");
        assert_eq!(forge.written().len(), if apply { 2 } else { 0 });
    }
}

fn closed_target_596(replay: &mut Replay, minutes: i64, extra: Value) {
    invalid_596(replay);
    let target = &epic_answer(replay, 596)["repository"]["issue"]["subIssues"]["nodes"][0];
    let mut action = json!({"type": "rescope_sub_issue", "issue": format!("{}#{}",
        target["repository"]["nameWithOwner"].as_str().unwrap(), target["number"]),
        "body": "Smaller.", "reason": "Split."});
    action
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let body = format!(
        "```{EPIC_PLAN_FENCE}\n{}\n```",
        json!({
            "schema": crate::epic::EPIC_PLAN_SCHEMA, "epic": "cgwalters-forge/tracker#596", "actions": [action]
        })
    );
    let plan = posted(replay, 596, 7_000_000_003, "cgwalters-bot", minutes, &body);
    *comment(replay, COMMENTS_596, "-7000000003") = plan;
}

#[test]
fn closed_targets_request_a_whole_new_plan_then_wait() {
    let mut snapshot = replay();
    closed_target_596(&mut snapshot, 2 * 24 * 60, json!({}));
    // Preview requests a new whole plan, not a subset or a write on a child.
    {
        let report = preview(&config(json!({})), &snapshot);
        assert_eq!(report.failures, 0, "{:#?}", report.lines);
        assert!(
            stage(&report).iter().any(|l| l
                .starts_with("start planning cgwalters-forge/tracker#596")
                && l.contains("has closed since")),
            "{:#?}",
            report.lines
        );
        assert!(
            !stage(&report)
                .iter()
                .any(|l| l.starts_with("apply epic cgwalters-forge/tracker#596"))
        );
        // The resulting dispatch record is the state the next pass sees.
        requested(&mut snapshot, 1);
        run_77(&mut snapshot, None);
        for _ in 0..2 {
            let waiting = preview(&config(json!({})), &snapshot);
            assert!(
                stage(&waiting)
                    .iter()
                    .any(|l| l.contains("#596:") && l.contains("waits for")),
                "{:#?}",
                waiting.lines
            );
            assert!(
                !stage(&waiting)
                    .iter()
                    .any(|l| l.starts_with("start planning") && l.contains("#596"))
            );
        }
    }
    // An unanswered stale-plan request is held for the operator after a day,
    // rather than repeatedly asking a planner that keeps naming closed issues.
    let mut retry = replay();
    closed_target_596(&mut retry, 3 * 24 * 60, json!({}));
    requested(&mut retry, 2 * 24 * 60);
    let ask = posted(
        &retry,
        596,
        7_000_000_000,
        "cgwalters-bot",
        2 * 24 * 60 + 1,
        &format!("{STALE_INTRO}."),
    );
    comments(&mut retry, COMMENTS_596).push(ask);
    run_77(&mut retry, Some(2 * 24 * 60 - 60));
    let report = preview(&config(json!({})), &retry);
    assert!(
        stage(&report)
            .iter()
            .any(|l| l.contains("#596:") && l.contains("a person replaces it")),
        "{:#?}",
        report.lines
    );
}

#[test]
fn closed_target_mixed_invalidity_and_human_holds() {
    for (minutes, extra, held, expected) in [
        (
            2 * 24 * 60,
            json!({"value": "not-a-priority"}),
            false,
            "a person replaces it",
        ),
        (
            2 * 24 * 60,
            json!({"unknown": true}),
            false,
            "a person replaces it",
        ),
        (2 * 24 * 60, json!({}), true, "labelled needs-design"),
    ] {
        let mut replay = replay();
        closed_target_596(&mut replay, minutes, extra);
        if held {
            label_596(&mut replay, &["needs-design"]);
        }
        for apply in [false, true] {
            let forge = Forge::new(Replay {
                now: replay.now,
                requests: replay.requests.clone(),
            });
            let report = if apply {
                local(&forge, 0)
            } else {
                preview(&config(json!({})), &forge.replay)
            };
            assert!(
                stage(&report)
                    .iter()
                    .any(|l| l.contains("#596:") && l.contains(expected)),
                "{:#?}",
                report.lines
            );
            assert!(
                !stage(&report)
                    .iter()
                    .any(|l| l.starts_with("start planning") && l.contains("#596"))
            );
            assert!(forge.written().is_empty());
            assert_eq!(report.failures, 0, "{:#?}", report.lines);
        }
    }
}

#[test]
fn stale_plans_preserve_in_flight_requests_and_configuration_refusals() {
    for busy in [false, true] {
        let mut replay = replay();
        closed_target_596(&mut replay, 2 * 24 * 60, json!({}));
        if busy {
            requested(&mut replay, 60);
            let body = &mut answer(&mut replay, "status=queued")["body"];
            body["total_count"] = 1.into();
            body["workflow_runs"] = json!([{"id": 77, "display_title": "triage cgwalters-forge/tracker#596 (claude)",
                "html_url": "https://github.com/cgwalters-forge/tracker/actions/runs/77"}]);
        } else {
            // Also structurally valid with closed targets relaxed, but not
            // permitted by pass's apply configuration: not stale-only.
            let plan = comment(&mut replay, COMMENTS_596, "-7000000003");
            let mut value = fenced_json(text(plan, "/body"), EPIC_PLAN_FENCE)
                .unwrap()
                .unwrap();
            value["actions"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type": "close_epic", "reason": "Done"}));
            plan["body"] = format!("```{EPIC_PLAN_FENCE}\n{value}\n```").into();
        }
        let report = preview(&config(json!({})), &replay);
        let expected = if busy {
            "it was planned again, which it waits for"
        } else {
            "configuration does not allow"
        };
        assert!(
            stage(&report)
                .iter()
                .any(|l| l.contains("#596:") && l.contains(expected)),
            "{:#?}",
            report.lines
        );
        assert!(
            !stage(&report)
                .iter()
                .any(|l| l.starts_with("start planning") && l.contains("#596"))
        );
        assert_eq!(report.failures, 0, "{:#?}", report.lines);
    }
}

#[test]
fn changed_plan_before_validation_does_not_take_slot() {
    let forge = Forge::new(queued_plans());
    forge.changed_plan.set(Some((596, 2)));
    let report = local(&forge, 10);
    let lines = stage(&report);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("#596:") && l.contains("changed since it was read")),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("apply epic cgwalters-forge/tracker#890")),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("#891:") && l.contains("one epic plan")),
        "{lines:#?}"
    );
    assert_eq!(report.failures, 0, "{lines:#?}");
}

#[test]
fn failure_after_admission_never_admits_another_apply() {
    for race in [false, true] {
        let mut replay = queued_plans();
        if !race {
            let plan = &mut comments(&mut replay, COMMENTS_890)[0];
            let body = text(plan, "/body").replace("\"actions\":[]", "\"actions\":[{\"type\":\"create_sub_issue\",\"repo\":\"cgwalters-forge/agent-board\",\"title\":\"Slice\",\"body\":\"Work\"}]");
            plan["body"] = body.into();
        }
        let forge = Forge::new(replay);
        if race {
            // Selection, preview validation, apply selection, pre-claim, post-claim.
            forge.changed_plan.set(Some((890, 5)));
        } else {
            forge.link_fails.set(true);
        }
        let report = local(&forge, 10);
        let lines = stage(&report);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("#891:") && l.contains("one epic plan")),
            "{lines:#?}"
        );
        assert!(
            forge
                .written()
                .iter()
                .any(|w| w == "POST repos/cgwalters-forge/tracker/issues/890/comments"),
            "{lines:#?}"
        );
        assert!(!forge.written().iter().any(|w| w.contains("/891/")));
        assert_eq!(report.failures, usize::from(!race), "{lines:#?}");
        let before = forge.written().len();
        let again = local(&forge, 0);
        assert_eq!(forge.written().len(), before, "{:#?}", again.lines);
        assert!(
            !stage(&again)
                .iter()
                .any(|l| l.starts_with("start planning") && l.contains("#890"))
        );
    }
}

/// An applying pass in `forge`, starting no run, within `writes` writes.
fn local(forge: &Forge, writes: usize) -> Report {
    local_with(forge, writes, json!({}))
}

/// [`local`] with `settings`.
fn local_with(forge: &Forge, writes: usize, settings: Value) -> Report {
    let limits = Limits {
        dispatch: Some(0),
        writes: Some(writes),
        ..Limits::default()
    };
    let config = config(settings);
    let mode = (Mode::Local, limits);
    block(run(
        forge,
        forge,
        &config,
        State::default(),
        mode,
        forge.replay.now,
    ))
    .unwrap()
}

/// An applying pass applies tracker#890's plan, had it no receipt, once:
/// each new sub-issue created, on the receipt before it is linked, and given
/// its final Priority in one write; the receipt says what was done, and the
/// next pass recognises it and creates nothing. Past --max-writes it is held
/// whole, with nothing written.
#[test]
fn apply_creates_once_with_final_priorities() {
    let mut replay = replay();
    unfinished(&mut replay);
    unapplied(&mut replay);
    let held = Forge::new(Replay {
        now: replay.now,
        requests: replay.requests.clone(),
    });
    let report = local(&held, 9);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l
                .starts_with("held by --max-writes: apply epic cgwalters-forge/tracker#890 plan=")),
        "{:#?}",
        report.lines
    );
    assert!(held.written().is_empty(), "{:?}", held.written());

    let forge = Forge::new(replay);
    let report = local(&forge, 10);
    let line = (report.lines.iter())
        .find(|l| l.starts_with("apply epic cgwalters-forge/tracker#890"))
        .unwrap_or_else(|| panic!("{:#?}", report.lines));
    assert!(
        line.starts_with(&format!(
            "apply epic cgwalters-forge/tracker#890 plan={PLAN_890}: Created cgwalters-forge/tracker#1002 (P0); Created cgwalters-forge/agent-board#"
        )),
        "{line}"
    );
    assert!(
        line.ends_with(
            " receipt=https://github.com/cgwalters-forge/tracker/issues/890#issuecomment-1001"
        ),
        "{line}"
    );
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    let written = forge.written();
    // The claim first, then each create: its issue, the receipt naming it,
    // the link and its one Priority write; then the receipt's last edit.
    assert_eq!(
        written[0],
        "POST repos/cgwalters-forge/tracker/issues/890/comments"
    );
    assert_eq!(
        written[1..6],
        [
            "POST repos/cgwalters-forge/tracker/issues",
            "PATCH repos/cgwalters-forge/tracker/issues/comments/1001",
            "graphql addSubIssue",
            "graphql addProjectV2ItemById",
            "graphql updateProjectV2ItemFieldValue",
        ]
    );
    let priorities = (forge.writes.borrow().iter())
        .filter(|(w, _)| w == "graphql updateProjectV2ItemFieldValue")
        .count();
    assert_eq!(priorities, 9, "one Priority write per new sub-issue");
    assert_eq!(
        written.last().unwrap(),
        "PATCH repos/cgwalters-forge/tracker/issues/comments/1001"
    );
    let receipt = text(&forge.added.borrow()[0], "/body").to_owned();
    assert!(
        receipt.starts_with(
            "<!-- agent-board-epic-apply: 6366132099@2026-10-23T13:34:49Z -->\nApplied [this epic plan](https://github.com/cgwalters-forge/tracker/issues/890#issuecomment-6366132099) at "
        ),
        "{receipt}"
    );
    assert!(
        receipt.contains("\n- Created #1002 (P0)\n- Created [agent-board#"),
        "{receipt}"
    );
    assert!(receipt.contains("```agent-board-epic-apply\n"), "{receipt}");

    // The next pass reads the receipt: nothing is written again.
    let before = written.len();
    let report = local(&forge, 10);
    assert_eq!(forge.written().len(), before, "{:#?}", report.lines);
    assert!(
        (report.lines.iter()).any(|l| l.starts_with(&format!(
            "epic cgwalters-forge/tracker#890: plan {PLAN_890} applied; nothing changed since"
        ))),
        "{:#?}",
        report.lines
    );
}

/// An applying pass holds tracker#596, whose retried planning request got
/// no plan either, for the operator: it labels it needs-human and sets its
/// Why to the question, both or, past --max-writes, neither.
#[test]
fn second_empty_plan_labels_and_asks() {
    let mut replay = replay();
    retried(&mut replay);
    let variables =
        json!({"owner": "cgwalters-forge", "name": "tracker", "number": 596, "field": WHY_FIELD});
    replay.requests.push(json!({
        "request": graphql_key(WHY_QUERY, &variables),
        "answer": {"data": {"repository": {"issue": {"projectItems": {"nodes": [{"id": "PVTI_596",
            "project": {"id": "PVT_1", "number": 1, "owner": {"login": "cgwalters-forge"},
                "field": {"id": "F_why", "dataType": "TEXT"}},
            "why": null}], "totalCount": 1}}}}},
    }));
    let ours = |forge: &Forge| -> Vec<(String, Value)> {
        (forge.writes.borrow().iter())
            .filter(|(w, body)| w.ends_with("/596/labels") || body["field"] == "F_why")
            .cloned()
            .collect()
    };
    let question =
        "Q: Planning this epic got no plan twice (its run 77 ended failure). What should change?";
    for (writes, expected) in [
        (1, vec![]),
        (
            2,
            vec![
                (
                    "POST repos/cgwalters-forge/tracker/issues/596/labels".to_owned(),
                    json!({"labels": ["needs-human"]}),
                ),
                (
                    "graphql updateProjectV2ItemFieldValue".to_owned(),
                    json!({"project": "PVT_1", "item": "PVTI_596", "field": "F_why", "text": question}),
                ),
            ],
        ),
    ] {
        let forge = Forge::new(Replay {
            now: replay.now,
            requests: replay.requests.clone(),
        });
        let report = local(&forge, writes);
        let line = (report.lines.iter())
            .find(|l| l.contains("hold epic cgwalters-forge/tracker#596: "))
            .unwrap_or_else(|| panic!("{:#?}", report.lines));
        let held = line.starts_with("held by --max-writes: ");
        assert_eq!(held, expected.is_empty(), "{line}");
        assert!(
            line.ends_with(&format!("; label needs-human, Why {question}")),
            "{line}"
        );
        assert_eq!(ours(&forge), expected, "{:#?}", report.lines);
        assert_eq!(report.failures, 0, "{:#?}", report.lines);
    }
}

/// An applying pass on tracker#596, whose sub-issues are all closed: asks
/// the operator (the Why, the comment that says so once, the label), all
/// or, past --max-writes, none of it; or with `epics.close` closes it as
/// completed after that comment. The comment's marker makes the next pass
/// leave it be; a Why that failed before it is asked again.
#[test]
fn done_epic_is_asked_or_closed_once() {
    let mut replay = replay();
    let variables =
        json!({"owner": "cgwalters-forge", "name": "tracker", "number": 596, "field": WHY_FIELD});
    replay.requests.push(json!({
        "request": graphql_key(WHY_QUERY, &variables),
        "answer": {"data": {"repository": {"issue": {"projectItems": {"nodes": [{"id": "PVTI_596",
            "project": {"id": "PVT_1", "number": 1, "owner": {"login": "cgwalters-forge"},
                "field": {"id": "F_why", "dataType": "TEXT"}},
            "why": null}], "totalCount": 1}}}}},
    }));
    let board = config(json!({})).board.unwrap();
    let marker = done_marker(
        &EpicFacts::from_node(&node(&replay, 596), Some(&board))
            .unwrap()
            .census,
    );
    let close = json!({"epics": {"repository": "cgwalters-forge/tracker", "close": true}});
    let asked = [
        "graphql updateProjectV2ItemFieldValue",
        "POST repos/cgwalters-forge/tracker/issues/596/comments",
        "POST repos/cgwalters-forge/tracker/issues/596/labels",
    ];
    let closed = [
        "POST repos/cgwalters-forge/tracker/issues/596/comments",
        "PATCH repos/cgwalters-forge/tracker/issues/596",
    ];
    let ours = |forge: &Forge| -> Vec<(String, Value)> {
        (forge.writes.borrow().iter())
            .filter(|(w, body)| w.contains("/596") || body["field"] == "F_why")
            .cloned()
            .collect()
    };
    let names = |writes: &[(String, Value)]| -> Vec<String> {
        writes.iter().map(|(w, _)| w.clone()).collect()
    };
    for (settings, writes, expected) in [
        (json!({}), 2, &[][..]),
        (json!({}), 3, &asked[..]),
        (close.clone(), 1, &[][..]),
        (close, 2, &closed[..]),
    ] {
        let forge = Forge::new(Replay {
            now: replay.now,
            requests: replay.requests.clone(),
        });
        let report = local_with(&forge, writes, settings.clone());
        assert_eq!(
            report.failures, 0,
            "{settings} {writes}: {:#?}",
            report.lines
        );
        let made = ours(&forge);
        assert_eq!(
            names(&made),
            expected,
            "{settings} {writes}: {:#?}",
            report.lines
        );
        // The next pass reads the comment, and leaves the epic be.
        let again = local_with(&forge, writes, settings.clone());
        assert_eq!(ours(&forge).len(), made.len(), "{:#?}", again.lines);
        for (what, body) in &made {
            if what.ends_with("/comments") {
                assert!(text(body, "/body").starts_with(&marker), "{body}");
            }
            if what.starts_with("PATCH") {
                assert_eq!(
                    *body,
                    json!({"state": "closed", "state_reason": "completed"})
                );
            }
        }
    }
    // The Why refused: no comment marks the ask made, so the next pass
    // asks whole, once.
    let forge = Forge::new(Replay {
        now: replay.now,
        requests: replay.requests.clone(),
    });
    forge.why_fails.set(true);
    let failed = local_with(&forge, 3, json!({}));
    assert_eq!(failed.failures, 1, "{:#?}", failed.lines);
    assert!(ours(&forge).is_empty(), "{:#?}", failed.lines);
    forge.why_fails.set(false);
    for _ in 0..2 {
        let report = local_with(&forge, 3, json!({}));
        assert_eq!(report.failures, 0, "{:#?}", report.lines);
    }
    assert_eq!(names(&ours(&forge)), asked);
    // The operator's answer takes the ask's label off first.
    let mut answered = Replay {
        now: replay.now,
        requests: replay.requests.clone(),
    };
    answered_done(&mut answered, &["needs-human"]);
    let forge = Forge::new(answered);
    let report = local_with(&forge, 3, json!({}));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    assert_eq!(
        names(&ours(&forge)).first().map(String::as_str),
        Some("DELETE repos/cgwalters-forge/tracker/issues/596/labels/needs-human"),
        "{:#?}",
        report.lines
    );
    // A sub-issue reopened, or a hold put on, after the epics were listed:
    // read again before the writes, it waits, asked or closed.
    fn epic_596(data: &mut Value) -> Option<&mut Value> {
        let nodes = data.pointer_mut("/repository/issues/nodes")?.as_array_mut();
        (nodes.unwrap().iter_mut()).find(|n| n["number"] == 596)
    }
    // Only tracker#596's sub-issues are read again.
    let reopened: fn(&mut Value) = |data| {
        if let Some(sub) = data.pointer_mut("/repository/issue/subIssues/nodes/0") {
            sub["state"] = "OPEN".into();
        }
    };
    let held: fn(&mut Value) = |data| {
        let Some(epic) = epic_596(data) else {
            return;
        };
        let labels = &mut epic["labels"];
        labels["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name": "needs-design"}));
        labels["totalCount"] = (labels["totalCount"].as_u64().unwrap() + 1).into();
    };
    let close = json!({"epics": {"repository": "cgwalters-forge/tracker", "close": true}});
    for (change, why) in [
        (reopened, "its sub-issues changed since they were read"),
        (held, "labelled needs-design"),
    ] {
        for settings in [json!({}), close.clone()] {
            let forge = Forge::new(Replay {
                now: replay.now,
                requests: replay.requests.clone(),
            });
            forge.relisted.set(Some(change));
            let report = local_with(&forge, 3, settings.clone());
            assert_eq!(report.failures, 0, "{why}: {:#?}", report.lines);
            assert!(ours(&forge).is_empty(), "{why}: {:#?}", report.lines);
            let waits = format!("; not yet: {why}; the next pass reads it again");
            assert!(
                (report.lines.iter())
                    .any(|l| l.starts_with("hold epic cgwalters-forge/tracker#596: ")
                        && l.ends_with(&waits)),
                "{settings} {why}: {:#?}",
                report.lines
            );
        }
    }
}

/// [`decide`] on tracker#890's recorded facts at other times: changes to
/// its sub-issues wait a day after the last plan unless the operator
/// speaks, twice as long for each apply in a row that had nothing to do;
/// an epic whose sub-issues changed only in Priority or text, or not at
/// all, is left alone however long after, the bot's comments and an empty
/// plan notwithstanding; one whose sub-issues are all closed is done; and
/// the operator's word plans an epic whose plan is invalid once, not again
/// after the request it led to.
#[test]
fn decide_coalesces_changes_but_not_direction() {
    let replay = replay();
    let board = config(json!({})).board.unwrap();
    let facts = EpicFacts::from_node(&node(&replay, 890), Some(&board)).unwrap();
    let recorded: Vec<Comment> = {
        let mut replay = Replay {
            now: replay.now,
            requests: replay.requests.clone(),
        };
        (comments(&mut replay, COMMENTS_890).iter())
            .map(|c| crate::plan::comment(c).unwrap())
            .collect()
    };
    let at = |stamp: &str| OffsetDateTime::parse(stamp, &Rfc3339).unwrap();
    let note = |id: u64, author: &str, when: &str, body: &str| Comment {
        url: format!("https://github.com/cgwalters-forge/tracker/issues/890#issuecomment-{id}"),
        body: body.into(),
        author: Some(author.into()),
        edited: false,
        editor: None,
        changed: at(when),
    };
    let say = |author: &str, when: &str| note(9, author, when, "Direction.");
    // A newest plan that is not JSON, the operator's word after it, and a
    // planning request after that, which the word does not plan again.
    let invalid = note(
        10,
        "cgwalters-bot",
        "2026-10-23T15:00:00Z",
        "```agent-board-epic-plan\n{\n```",
    );
    let asked = note(
        12,
        "cgwalters-bot",
        "2026-10-23T15:20:00Z",
        "<!-- agent-board-dispatch: 11@2026-10-23T15:19:00Z -->\nDispatched the plan.",
    );
    // The receipt's fingerprint with every sub-issue closed, as is the epic.
    let all_closed: Vec<Comment> = (recorded.iter())
        .map(
            |c| match c.body.starts_with("<!-- agent-board-epic-fingerprint: ") {
                true => Comment {
                    body: c.body.replace("\"OPEN\"", "\"CLOSED\""),
                    ..c.clone()
                },
                false => c.clone(),
            },
        )
        .collect();
    // Settled: every sub-issue as the receipt left it.
    let mut settled = facts.clone();
    let (receipt, _) = epic_apply::receipts(&recorded, &["cgwalters", "cgwalters-bot"]);
    let baseline = receipt[0].detail.fingerprint.clone().unwrap();
    settled.census.sub_issues.clear();
    settled.census.closed.clear();
    for (issue, state) in &baseline {
        let issue = IssueRef::parse(issue).unwrap();
        match state.as_str() {
            "OPEN" => settled.census.sub_issues.push(issue),
            _ => settled.census.closed.push(issue),
        }
    }
    settled.priorities = receipt[0].detail.priorities();
    // Its sub-issues' Priority moved since: churn, not a change.
    let mut drifted = settled.clone();
    for priority in drifted.priorities.values_mut() {
        *priority = "P3".into();
    }
    let mut empty = settled.clone();
    empty.census.closed.append(&mut empty.census.sub_issues);
    // An apply after it that had nothing to do, the `n`th.
    let nothing = |n: u64| {
        let detail = json!({"schema": epic_apply::RECEIPT_SCHEMA, "plan": receipt[0].detail.plan,
            "outcome": "applied", "actions": [], "fingerprint": baseline});
        let body = format!("```{}\n{detail}\n```", epic_apply::APPLY_MARKER);
        note(20 + n, "cgwalters-bot", "2026-10-23T14:00:00Z", &body)
    };
    // Never planned, and the bot's comment that its sub-issues are all
    // closed.
    let unplanned = vec![];
    let said = note(
        40,
        "cgwalters-bot",
        "2026-10-25T14:00:00Z",
        &format!("{}\nDone.", done_marker(&empty.census)),
    );
    // The facts, the recorded comments and more, the time, and the start
    // of what comes of it; a time after `3 ` sets `triageFollowUpDays` to 3.
    type Case<'a> = (
        &'a EpicFacts,
        &'a Vec<Comment>,
        Vec<Comment>,
        &'a str,
        &'a str,
    );
    let cases: Vec<Case> = vec![
        (
            &facts,
            &recorded,
            vec![],
            "2026-10-23T15:00:00Z",
            "Wait { why: \"its sub-issues changed",
        ),
        (
            &facts,
            &recorded,
            vec![say("cgwalters-bot", "2026-10-23T14:50:00Z")],
            "2026-10-23T15:00:00Z",
            "Wait { why: \"its sub-issues changed",
        ),
        (
            &facts,
            &recorded,
            vec![say("cgwalters", "2026-10-23T14:50:00Z")],
            "2026-10-23T15:00:00Z",
            "Plan(\"operator direction since it was last planned\")",
        ),
        (
            &facts,
            &recorded,
            vec![],
            "2026-10-23T16:35:00Z",
            "Wait { why: \"its sub-issues changed",
        ),
        (
            &facts,
            &recorded,
            vec![],
            "2026-10-24T13:35:00Z",
            "Plan(\"its sub-issues changed",
        ),
        (
            &drifted,
            &recorded,
            vec![],
            "2026-10-30T14:00:00Z",
            "Idle(\"plan 6366132099@2026-10-23T13:34:49Z applied; nothing changed since",
        ),
        // A plan that had nothing to do ends planning until they change.
        (
            &settled,
            &recorded,
            vec![nothing(1)],
            "2026-10-30T14:00:00Z",
            "Idle(\"plan 6366132099@2026-10-23T13:34:49Z applied; nothing changed since 2026-10-23T14:00:00Z\")",
        ),
        (
            &settled,
            &recorded,
            vec![],
            "2026-10-23T20:00:00Z",
            "Idle(\"plan 6366132099@2026-10-23T13:34:49Z applied; nothing changed since 2026-10-23T13:42:20Z\")",
        ),
        // Unchanged, it is not planned again a day or a week later.
        (
            &settled,
            &recorded,
            vec![],
            "2026-10-24T14:00:00Z",
            "Idle(\"plan 6366132099@2026-10-23T13:34:49Z applied; nothing changed since 2026-10-23T13:42:20Z\")",
        ),
        (
            &settled,
            &recorded,
            vec![say("cgwalters-bot", "2026-10-25T14:00:00Z")],
            "2026-10-30T14:00:00Z",
            "Idle(\"plan 6366132099@2026-10-23T13:34:49Z applied; nothing changed since",
        ),
        (
            &settled,
            &recorded,
            vec![say("cgwalters", "2026-10-25T14:00:00Z")],
            "2026-10-30T14:00:00Z",
            "Plan(\"operator direction since it was last planned\")",
        ),
        (
            &empty,
            &recorded,
            vec![],
            "2026-10-23T15:00:00Z",
            "Done(\"every sub-issue is closed; its sub-issues changed",
        ),
        (
            &empty,
            &all_closed,
            vec![],
            "2026-10-24T14:00:00Z",
            "Idle(\"plan 6366132099@2026-10-23T13:34:49Z applied; nothing changed since",
        ),
        // Never planned: done, said once, and planned on the operator's
        // word after that.
        (
            &empty,
            &unplanned,
            vec![],
            "2026-10-25T13:00:00Z",
            "Done(\"every sub-issue is closed; it has never been planned\")",
        ),
        (
            &empty,
            &unplanned,
            vec![say("cgwalters", "2026-10-25T13:00:00Z"), said.clone()],
            "2026-10-30T14:00:00Z",
            "Said(\"every sub-issue is closed, as",
        ),
        (
            &empty,
            &unplanned,
            vec![said.clone(), say("cgwalters-bot", "2026-10-25T15:00:00Z")],
            "2026-10-30T14:00:00Z",
            "Said(\"every sub-issue is closed, as",
        ),
        (
            &empty,
            &unplanned,
            vec![said, say("cgwalters", "2026-10-25T15:00:00Z")],
            "2026-10-30T14:00:00Z",
            "Plan(\"operator direction since https://github.com/cgwalters-forge/tracker/issues/890#issuecomment-40\")",
        ),
        // Each apply in a row that had nothing to do doubles the day, up
        // to `triageFollowUpDays`; without it, a day.
        (
            &facts,
            &recorded,
            vec![nothing(1)],
            "3 2026-10-25T13:00:00Z",
            "Wait { why: \"its sub-issues changed",
        ),
        (
            &facts,
            &recorded,
            vec![nothing(1)],
            "3 2026-10-25T14:00:00Z",
            "Plan(\"its sub-issues changed",
        ),
        (
            &facts,
            &recorded,
            vec![nothing(1), nothing(2)],
            "3 2026-10-26T13:00:00Z",
            "Wait { why: \"its sub-issues changed",
        ),
        (
            &facts,
            &recorded,
            vec![nothing(1), nothing(2)],
            "3 2026-10-26T14:00:00Z",
            "Plan(\"its sub-issues changed",
        ),
        (
            &facts,
            &recorded,
            vec![nothing(1), nothing(2)],
            "2026-10-24T14:00:00Z",
            "Plan(\"its sub-issues changed",
        ),
        (
            &facts,
            &recorded,
            vec![invalid.clone(), say("cgwalters", "2026-10-23T15:10:00Z")],
            "2026-10-23T15:30:00Z",
            "Plan(\"operator direction since its invalid epic plan\")",
        ),
        (
            &facts,
            &recorded,
            vec![invalid, say("cgwalters", "2026-10-23T15:10:00Z"), asked],
            "2026-10-23T15:30:00Z",
            "Hold(Hold { kind: NeedsOperator, reason: \"its newest epic plan is invalid",
        ),
    ];
    for (facts, base, extra, now, expected) in cases {
        let mut all = base.clone();
        all.extend(extra);
        let (follow_up_days, now) = now.split_once(' ').unwrap_or(("0", now));
        let next = decide(&Inputs {
            epic: facts,
            comments: &all,
            operator: "cgwalters",
            bot: "cgwalters-bot",
            busy: None,
            now: at(now),
            follow_up_days: follow_up_days.parse().unwrap(),
            direction: None,
        });
        let got = format!("{next:?}");
        assert!(got.starts_with(expected), "{now}: {got}");
    }
    // The operator's direction: its hash in the record of the request the
    // applied plan answered, the hash this pass read, and the request that
    // a change led to, not yet answered.
    let record = |id: u64, when: &str, hash: Option<&str>| {
        let line = hash.map(|h| format!("\n<!-- agent-board-direction: {h} -->"));
        let body = format!(
            "<!-- agent-board-dispatch: {id}@{when} -->{}\nDispatched the plan.",
            line.unwrap_or_default()
        );
        note(id, "cgwalters-bot", when, &body)
    };
    let idle = "Idle(\"plan 6366132099@2026-10-23T13:34:49Z applied";
    let changed = "Plan(\"the operator's direction changed since it was last planned\")";
    for (recorded_hash, now_hash, again, expected) in [
        (Some("aaaa"), Some("aaaa"), false, idle),
        (Some("aaaa"), Some("bbbb"), false, changed),
        // Once: the planning request the change led to carries its hash.
        (Some("aaaa"), Some("bbbb"), true, "Unanswered"),
        // Set where it was not, or unreadable this pass: nothing is due.
        (None, Some("bbbb"), false, idle),
        (Some("aaaa"), None, false, idle),
    ] {
        let mut all = recorded.clone();
        all.push(record(30, "2026-10-23T13:30:00Z", recorded_hash));
        if again {
            all.push(record(31, "2026-10-25T14:00:00Z", now_hash));
        }
        let next = decide(&Inputs {
            epic: &settled,
            comments: &all,
            operator: "cgwalters",
            bot: "cgwalters-bot",
            busy: None,
            now: at("2026-10-30T14:00:00Z"),
            follow_up_days: 0,
            direction: now_hash,
        });
        let got = format!("{next:?}");
        assert!(
            got.starts_with(expected),
            "{recorded_hash:?} {now_hash:?}: {got}"
        );
    }
}

/// An epic whose boards were not all read must not lose its board Status,
/// and with it the operator's hold: eleven boards, ten read, ours not among
/// them.
#[test]
fn board_fields_refuse_a_partial_membership_read() {
    let board = Board {
        owner: "example".into(),
        project: 1,
        priority_field: "Priority".into(),
    };
    let item = |owner: &str, number: u64, status: &str| {
        json!({"project": {"number": number, "owner": {"login": owner}},
               "priority": {"name": "P1"}, "status": {"name": status}})
    };
    let others: Vec<Value> = (2..12).map(|n| item("other", n, "Todo")).collect();
    let ours = item("example", 1, "Blocked");
    for (nodes, total, want) in [
        (vec![ours.clone()], 1, Some("Blocked")),
        (others.clone(), 10, None),
        (others.clone(), 11, Some("error")),
    ] {
        let node = json!({"projectItems": {"nodes": nodes, "totalCount": total}});
        let got = match board_fields(&node, Some(&board)) {
            Ok(fields) => fields.status,
            Err(_) => Some("error".into()),
        };
        assert_eq!(got.as_deref(), want, "{total} boards");
    }
}

/// tracker#766 without its labels that hold it, and planned four days later than recorded, so
/// that like tracker#890 it waits a day after its last request.
fn waiting_766(replay: &mut Replay) {
    let before = OffsetDateTime::parse("2026-10-20T00:00:00Z", &Rfc3339).unwrap();
    let comments = comments(replay, r#""after":null,"name":"tracker","number":766,"#);
    for comment in comments.iter_mut() {
        let created = OffsetDateTime::parse(text(comment, "/createdAt"), &Rfc3339).unwrap();
        if created < before {
            moved(comment, time::Duration::days(4));
            // The plan its receipt names, by its time.
            let body = text(comment, "/body").replace("2026-10-19T", "2026-10-23T");
            comment["body"] = body.into();
        }
    }
    let nodes = (replay.requests.iter_mut())
        .filter_map(|e| e["answer"].pointer_mut("/data/repository/issues/nodes"))
        .filter_map(Value::as_array_mut)
        .flatten();
    for node in nodes.filter(|n| n["number"] == 766) {
        let held = &mut node["labels"];
        let labels = held["nodes"].as_array_mut().unwrap();
        labels.retain(|l| l["name"] != NEEDS_HUMAN && l["name"] != "needs-design");
        held["totalCount"] = labels.len().into();
    }
}

/// An epic held only by the day's wait after its last planning request is
/// planned at once by a pass that would otherwise be idle: one a pass, the
/// highest in Priority, never one a person holds, and never while slots
/// are taken.
#[test]
fn idle_pass_plans_a_waiting_epic_early() {
    const WAIT_890: &str = "hold epic cgwalters-forge/tracker#890: its sub-issues changed";
    const EARLY_890: &str =
        "start planning cgwalters-forge/tracker#890 pool=claude: its sub-issues changed";
    const IDLE: &str = "idle: 6 slot(s) free and nothing started";
    // Stage 4 with one candidate, which waits: an idle pass.
    let idle = |r: &mut Replay, c: &mut Value| {
        crate::pass::tests::only(r, &["cgwalters-forge/tracker#446"]);
        c["triage"] = false.into();
    };
    // tracker#596 held by a person, so that stage 3 plans nothing.
    let quiet = |r: &mut Replay| label_596(r, &["needs-human"]);
    type Case = (
        &'static str,
        Box<dyn Fn(&mut Replay, &mut Value)>,
        Vec<&'static str>,
    );
    let cases: Vec<Case> = vec![
        (
            "idle: tracker#890 is planned now",
            Box::new(move |r, c| {
                quiet(r);
                idle(r, c);
            }),
            vec![WAIT_890, EARLY_890],
        ),
        (
            "busy: stage 4 takes every slot and tracker#890 waits its day",
            Box::new(move |r, _| quiet(r)),
            vec![WAIT_890],
        ),
        (
            "needs-design: a person holds tracker#890",
            Box::new(move |r, c| {
                quiet(r);
                label(r, 890, &["needs-design"]);
                idle(r, c);
            }),
            vec![
                "hold epic cgwalters-forge/tracker#890: labelled needs-design; otherwise its sub-issues changed",
                IDLE,
            ],
        ),
        (
            "one a pass: tracker#766 waits too, behind tracker#890 in Priority",
            Box::new(move |r, c| {
                quiet(r);
                waiting_766(r);
                idle(r, c);
            }),
            vec![
                WAIT_890,
                "hold epic cgwalters-forge/tracker#766: its sub-issues changed",
                EARLY_890,
            ],
        ),
        (
            "one a pass: stage 3 planned tracker#596",
            Box::new(idle),
            vec![
                "start planning cgwalters-forge/tracker#596 pool=claude: ",
                WAIT_890,
                "idle: 5 slot(s) free and nothing started",
            ],
        ),
    ];
    for (name, edit, expected) in cases {
        let mut replay = replay();
        unfinished(&mut replay);
        let mut settings = json!({});
        edit(&mut replay, &mut settings);
        let report = preview(&config(settings), &replay);
        assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
        let lines: Vec<&str> = (report.lines.iter())
            .filter(|l| !l.contains(": labelled needs-human;"))
            .filter(|l| {
                l.contains("epic cgwalters-forge/tracker#890")
                    || l.contains("epic cgwalters-forge/tracker#766")
                    || l.starts_with("start planning")
                    || l.starts_with("idle:")
            })
            .map(String::as_str)
            .collect();
        assert_eq!(lines.len(), expected.len(), "{name}: {lines:#?}");
        for (line, start) in lines.iter().zip(&expected) {
            assert!(line.starts_with(start), "{name}: {line}");
        }
        // The early start's hold is taken back; a waiting one stays.
        let early = expected.contains(&EARLY_890);
        let held = (report.holds.iter())
            .any(|e| e.line().starts_with(WAIT_890) && e.hold.kind == HoldKind::TriageCooldown);
        assert_eq!(held, expected.contains(&WAIT_890) && !early, "{name}");
        if early {
            let line = lines.iter().find(|l| l.starts_with(EARLY_890)).unwrap();
            assert!(
                line.ends_with("; early, with 6 slot(s) free and nothing started"),
                "{line}"
            );
        }
    }
}
