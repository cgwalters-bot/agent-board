use super::*;
use crate::epic::SUB_ISSUES_QUERY;
use crate::pass::tests::{answer, block, config, preview, replay as board, set_usage, usage};
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

/// The recorded census of tracker#`n`.
fn node(replay: &Replay, n: u64) -> &Value {
    (replay.requests.iter())
        .filter_map(|e| e["answer"].pointer("/data/repository/issues/nodes"))
        .flat_map(|nodes| nodes.as_array().unwrap())
        .find(|node| node["number"] == n)
        .unwrap()
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

/// A plan on tracker#596 five minutes before the recording that apply
/// refuses, with the epic as `epic apply` reads it.
fn invalid_596(replay: &mut Replay) {
    let body = format!(
        "```agent-board-epic-plan\n{}\n```",
        json!({"schema": "nope", "epic": "cgwalters-forge/tracker#596", "actions": []})
    );
    let plan = posted(replay, 596, 7_000_000_003, "cgwalters-bot", 5, &body);
    comments(replay, COMMENTS_596).push(plan);
    census(replay, 596);
}

/// tracker#890 without its receipt: its plan as if never applied, which
/// `epic apply` may then read.
fn unapplied(replay: &mut Replay) {
    comments(replay, COMMENTS_890).retain(|c| !text(c, "/url").ends_with(RECEIPT_890));
    census(replay, 890);
}

/// The live board of the evening of 23 October with a fresh usage
/// snapshot: tracker#596, frozen since tracker#851 by a coordinator that
/// matched its sub-issues by title, is planned; tracker#890's plan is
/// recognised as applied from its receipt, though most of the sub-issues
/// it created have closed since and one was reprioritised, so nothing is
/// created again; it is due too, but one epic is planned a pass; #766 is
/// held by its `needs-human` label.
#[test]
fn replay_plans_one_epic_and_recreates_nothing() {
    let report = preview(&config(json!({})), &replay());
    assert_eq!(
        stage(&report),
        [
            "epics: 3 live of 3 open in cgwalters-forge/tracker",
            "start planning cgwalters-forge/tracker#596 pool=claude: its sub-issues changed (cgwalters-forge/agentic-job#630 closed, cgwalters-forge/agentic-job#715 closed, cgwalters-forge/agentic-job#716 closed and 3 more)",
            "hold epic cgwalters-forge/tracker#890: due (its sub-issues changed (cgwalters-forge/agent-board#285 closed, cgwalters-forge/agent-board#286 closed, cgwalters-forge/agent-board#287 closed and 5 more); cgwalters-forge/agent-board#284 edited; cgwalters-forge/agent-board#289's Priority is P0, not P2); one epic is planned a pass",
            "hold epic cgwalters-forge/tracker#766: labelled needs-human; otherwise due to be planned (its sub-issues changed (cgwalters-forge/actions#83 closed, cgwalters-forge/agent-board#167 closed, cgwalters-forge/agent-board#168 closed and 2 more))",
        ]
    );
    assert!(!report.lines.iter().any(|l| l.contains("apply epic")));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    // The planning run counts against --max-dispatch, and takes its turn.
    let replay = replay();
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
        lines[2].ends_with("; one epic is planned a pass"),
        "{lines:#?}"
    );
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
            "no planning while new work is off",
            Box::new(|_| {}),
            "hold epic cgwalters-forge/tracker#596: due (its sub-issues changed",
            0,
            Some((HoldKind::Other, false)),
        ),
    ];
    for (name, edit, expected, failures, kind) in cases {
        let mut replay = replay();
        edit(&mut replay);
        let settings = match name.starts_with("no planning") {
            true => json!({"wip": 0}),
            false => json!({}),
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
}

impl Forge {
    fn new(replay: Replay) -> Self {
        Self {
            replay,
            added: RefCell::new(vec![]),
            writes: RefCell::new(vec![]),
            ids: Cell::new(1000),
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
        if method == "POST" && path == "repos/cgwalters-forge/tracker/issues/890/comments" {
            let html_url =
                format!("https://github.com/cgwalters-forge/tracker/issues/890#issuecomment-{id}");
            let mut node = posted(
                &self.replay,
                890,
                id,
                "cgwalters-bot",
                0,
                text(body, "/body"),
            );
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
        if variables["number"] == 890
            && let Some(nodes) = data
                .pointer_mut("/repository/issue/comments/nodes")
                .and_then(Value::as_array_mut)
        {
            nodes.extend(self.added.borrow().iter().cloned());
        }
        Ok(data)
    }
}

/// An applying pass in `forge`, starting no run, within `writes` writes.
fn local(forge: &Forge, writes: usize) -> Report {
    let limits = Limits {
        dispatch: Some(0),
        writes: Some(writes),
        ..Limits::default()
    };
    let config = config(json!({}));
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

/// [`decide`] on tracker#890's recorded facts at other times: changes wait
/// three hours after the last plan unless the operator speaks, twice as
/// long for each apply in a row that had nothing to do; an unchanged epic
/// is left alone however long after, the bot's comments notwithstanding;
/// and the operator's word plans an epic whose plan is invalid once, not
/// again after the request it led to.
#[test]
fn decide_coalesces_changes_but_not_direction() {
    let replay = replay();
    let board = config(json!({})).board.unwrap();
    let facts = EpicFacts::from_node(node(&replay, 890), Some(&board)).unwrap();
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
    settled.edits.clear();
    settled.priorities = receipt[0].detail.priorities();
    let mut empty = settled.clone();
    empty.census.closed.append(&mut empty.census.sub_issues);
    // An apply after it that had nothing to do, the `n`th.
    let nothing = |n: u64| {
        let detail = json!({"schema": epic_apply::RECEIPT_SCHEMA, "plan": receipt[0].detail.plan,
            "outcome": "applied", "actions": [], "fingerprint": baseline});
        let body = format!("```{}\n{detail}\n```", epic_apply::APPLY_MARKER);
        note(20 + n, "cgwalters-bot", "2026-10-23T14:00:00Z", &body)
    };
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
            "Hold(Hold { kind: TriageCooldown, reason: \"its sub-issues changed",
        ),
        (
            &facts,
            &recorded,
            vec![say("cgwalters-bot", "2026-10-23T14:50:00Z")],
            "2026-10-23T15:00:00Z",
            "Hold(Hold { kind: TriageCooldown, reason: \"its sub-issues changed",
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
            "Plan(\"its sub-issues changed",
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
            "2026-10-24T14:00:00Z",
            "Plan(\"its sub-issues changed",
        ),
        (
            &empty,
            &all_closed,
            vec![],
            "2026-10-24T14:00:00Z",
            "Idle(\"plan 6366132099@2026-10-23T13:34:49Z applied; nothing changed since",
        ),
        // Each apply in a row that had nothing to do doubles the three
        // hours, up to `triageFollowUpDays`.
        (
            &facts,
            &recorded,
            vec![nothing(1)],
            "3 2026-10-23T18:00:00Z",
            "Hold(Hold { kind: TriageCooldown, reason: \"its sub-issues changed",
        ),
        (
            &facts,
            &recorded,
            vec![nothing(1)],
            "3 2026-10-23T20:00:00Z",
            "Plan(\"its sub-issues changed",
        ),
        (
            &facts,
            &recorded,
            vec![nothing(1), nothing(2)],
            "3 2026-10-23T20:00:00Z",
            "Hold(Hold { kind: TriageCooldown, reason: \"its sub-issues changed",
        ),
        (
            &facts,
            &recorded,
            vec![nothing(1), nothing(2)],
            "3 2026-10-24T02:00:00Z",
            "Plan(\"its sub-issues changed",
        ),
        (
            &facts,
            &recorded,
            vec![nothing(1), nothing(2)],
            "2026-10-23T18:00:00Z",
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
        });
        let got = format!("{next:?}");
        assert!(got.starts_with(expected), "{now}: {got}");
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
