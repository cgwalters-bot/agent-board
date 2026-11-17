use super::*;
use crate::supervise::tests::{board, supervisor};
use std::cell::RefCell;

/// 00:45Z, when the operator restarted the pass by hand in agent-board#715.
const NOW: &str = "2026-11-01T00:45:00Z";

fn now() -> i64 {
    millis(&json!(NOW)).unwrap()
}

/// A poll run `minutes` before [`NOW`], completed with `conclusion` unless
/// that is a status.
fn run(id: u64, minutes: i64, conclusion: &str) -> Value {
    let created = iso(now() - minutes * 60_000);
    match conclusion {
        "queued" | "in_progress" | "waiting" => {
            json!({"id": id, "created_at": created, "status": conclusion, "conclusion": null, "path": ".github/workflows/poll-apply.yml", "head_branch": "main"})
        }
        _ => {
            json!({"id": id, "created_at": created, "status": "completed", "conclusion": conclusion, "path": ".github/workflows/poll-apply.yml", "head_branch": "main"})
        }
    }
}

fn restarted(after: u64) -> Option<Record> {
    Some(Record {
        after,
        escalated: false,
        created_at: now() - 60 * 60_000,
    })
}

fn escalated(after: u64) -> Option<Record> {
    Some(Record {
        after,
        escalated: true,
        created_at: now() - 60 * 60_000,
    })
}

#[test]
fn decisions() {
    let restart = |after, age| Decision::Restart { after, age };
    let wait = |why: &str| Decision::Wait(why.into());
    let escalate = |after, error: &str| Decision::Escalate {
        after,
        error: error.into(),
    };
    let cases = [
        (
            "no runs",
            vec![],
            None,
            wait("no poll run is listed; nothing to restart"),
        ),
        (
            "fresh run",
            vec![run(5, 10, "success")],
            None,
            wait("poll run 5 started 10 minutes ago; not restarting"),
        ),
        // The 23:12Z pass of agent-board#715, 93 minutes before.
        (
            "stalled pass",
            vec![run(5, 93, "failure"), run(4, 103, "success")],
            None,
            restart(5, 93),
        ),
        (
            "exactly stale",
            vec![run(5, 25, "success")],
            None,
            restart(5, 25),
        ),
        (
            "queued pass",
            vec![run(6, 1, "queued"), run(5, 93, "failure")],
            None,
            wait("poll run 6 is queued; not restarting"),
        ),
        (
            "running pass behind a newer completed one",
            vec![run(6, 40, "cancelled"), run(5, 93, "in_progress")],
            None,
            wait("poll run 5 is in_progress; not restarting"),
        ),
        (
            "restart started nothing",
            vec![run(5, 93, "failure")],
            restarted(5),
            escalate(5, "no poll run started after the restart, 60 minutes ago"),
        ),
        (
            "restarted pass failed",
            vec![run(6, 30, "failure"), run(5, 93, "failure")],
            restarted(5),
            escalate(
                5,
                "poll run 6 after the restart ended failure and none followed it",
            ),
        ),
        (
            "restarted pass did not chain",
            vec![run(6, 30, "success"), run(5, 93, "failure")],
            restarted(5),
            escalate(
                5,
                "poll run 6 after the restart ended success and none followed it",
            ),
        ),
        (
            "restarted pass still running",
            vec![run(6, 3, "in_progress"), run(5, 93, "failure")],
            restarted(5),
            wait("poll run 6 is in_progress; not restarting"),
        ),
        (
            "escalated once already",
            vec![run(6, 30, "failure"), run(5, 93, "failure")],
            escalated(5),
            wait("the operator was told the restart after poll run 5 did not take; not restarting"),
        ),
        (
            "chain came back and stalled again",
            vec![
                run(8, 30, "failure"),
                run(7, 40, "success"),
                run(6, 50, "success"),
                run(5, 93, "failure"),
            ],
            restarted(5),
            restart(8, 30),
        ),
        (
            "chain came back after an escalation",
            vec![
                run(8, 30, "failure"),
                run(7, 40, "success"),
                run(6, 50, "success"),
            ],
            escalated(5),
            restart(8, 30),
        ),
        (
            "an older success does not count",
            vec![
                run(6, 30, "failure"),
                run(5, 93, "success"),
                run(4, 103, "success"),
            ],
            restarted(5),
            escalate(
                5,
                "poll run 6 after the restart ended failure and none followed it",
            ),
        ),
    ];
    for (name, runs, record, want) in cases {
        assert_eq!(decide(&runs, record, now(), 25).unwrap(), want, "{name}");
    }
    let undated = [json!({"id": 1, "status": "completed", "created_at": "yesterday"})];
    assert!(decide(&undated, None, now(), 25).is_err());
}

#[test]
fn records_round_trip_and_refuse_other_text() {
    for record in [restarted(5), escalated(12)] {
        let record = record.unwrap();
        let body = format!("{}\nmore text", record.line());
        assert_eq!(Record::parse(&body, record.created_at), Some(record));
    }
    for body in [
        "",
        "<!-- agent-board-pass-restart restarted after=x -->",
        "<!-- agent-board-pass-restart paused after=5 -->",
        "<!-- agent-board-pass-restart restarted after=5",
        "text\n<!-- agent-board-pass-restart restarted after=5 -->",
    ] {
        assert_eq!(Record::parse(body, now()), None, "{body:?}");
    }
}

/// The poll runs and the log issue's comments, recording each write.
struct Fake {
    runs: Vec<Value>,
    comments: Vec<Value>,
    dispatch_fails: bool,
    writes: RefCell<Vec<(String, Value)>>,
}

impl Fake {
    fn new(runs: Vec<Value>, comments: Vec<Value>) -> Self {
        Self {
            runs,
            comments,
            dispatch_fails: false,
            writes: RefCell::default(),
        }
    }

    fn bodies(&self) -> Vec<String> {
        let writes = self.writes.borrow();
        writes
            .iter()
            .map(|(path, body)| match body["body"].as_str() {
                Some(text) => format!("{path}: {text}"),
                None => format!("{path}: {body}"),
            })
            .collect()
    }
}

const RUNS: &str = "/repos/example/control/actions/workflows/poll-apply.yml/runs?branch=main&exclude_pull_requests=true&per_page=100";
const LOG: &str = "/repos/example/operator/issues/7/comments";
const DISPATCH: &str = "/repos/example/control/actions/workflows/poll-apply.yml/dispatches";

impl Api for Fake {
    fn get(&self, path: &str) -> Result<Value> {
        if path == RUNS {
            return Ok(json!({ "workflow_runs": self.runs }));
        }
        if path.starts_with(&format!("{LOG}?since=")) && path.ends_with("&page=1") {
            return Ok(Value::Array(self.comments.clone()));
        }
        panic!("unexpected GET {path}")
    }

    fn graphql(&self, _: &str, _: Value) -> Result<Value> {
        panic!("unexpected GraphQL")
    }

    fn download(&self, path: &str, _: u64) -> Result<Vec<u8>> {
        panic!("unexpected download {path}")
    }

    fn send(&self, method: &str, path: &str, body: &Value) -> Result<Value> {
        assert_eq!(method, "POST");
        self.writes.borrow_mut().push((path.into(), body.clone()));
        if path == DISPATCH && self.dispatch_fails {
            bail!("POST {path}: HTTP 503");
        }
        Ok(json!({}))
    }
}

fn configured() -> Settings {
    let mut cfg = supervisor();
    cfg[OPTION] = json!({"staleMinutes": 25, "inputs": {"apply": "true"}, "applyingWorkflow": "poll-apply.yml"});
    Settings::from_json(&board(), &cfg, None).unwrap()
}

fn comment(login: &str, body: String) -> Value {
    json!({"user": {"login": login}, "created_at": iso(now() - 60 * 60_000), "body": body})
}

const RUN_URL: &str = "https://github.com/example/control/actions/runs/77";

#[test]
fn two_ticks_with_delayed_listing_and_dispatch_errors() {
    for (name, dispatch_fails, elapsed, visible, expected_writes, expected) in [
        (
            "delayed listing",
            false,
            5,
            false,
            0,
            "waiting for visibility",
        ),
        (
            "visibility grace expires",
            false,
            25,
            false,
            1,
            "no poll run started after the restart, 25 minutes ago",
        ),
        (
            "run becomes visible",
            false,
            5,
            true,
            0,
            "poll run 6 is queued",
        ),
        (
            "definite dispatch failure",
            true,
            5,
            false,
            0,
            "the operator was told",
        ),
    ] {
        let mut first = Fake::new(vec![run(5, 93, "failure")], vec![]);
        first.dispatch_fails = dispatch_fails;
        let line = super::run(&first, &configured(), "main", RUN_URL, now()).unwrap();
        let writes = first.writes.borrow();
        assert_eq!(writes.len(), if dispatch_fails { 3 } else { 2 }, "{name}");
        assert_eq!(writes[0].0, LOG, "record before dispatch: {name}");
        assert_eq!(writes[1].0, DISPATCH, "{name}");
        if dispatch_fails {
            assert!(
                line.starts_with("escalated: starting it failed"),
                "{name}: {line}"
            );
        }
        let comments = writes.iter().filter(|(path, _)| path == LOG).map(|(_, body)| {
            json!({"user": {"login": "worker"}, "created_at": NOW, "body": body["body"]})
        }).collect();
        let runs = if visible {
            vec![run(6, 0, "queued"), run(5, 93, "failure")]
        } else {
            vec![run(5, 93, "failure")]
        };
        let second = Fake::new(runs, comments);
        let line = super::run(
            &second,
            &configured(),
            "main",
            RUN_URL,
            now() + elapsed * 60_000,
        )
        .unwrap();
        assert!(line.contains(expected), "{name}: {line}");
        assert_eq!(second.writes.borrow().len(), expected_writes, "{name}");
        assert!(
            second
                .writes
                .borrow()
                .iter()
                .all(|(path, _)| path != DISPATCH),
            "never retry: {name}"
        );
    }
}

#[test]
fn mixed_preview_and_apply_use_workflow_contract_not_titles() {
    for (name, applying, record, expected) in [
        (
            "preview activity cannot hide stall",
            vec![run(5, 93, "failure")],
            None,
            "started poll-apply.yml after poll run 5",
        ),
        (
            "preview success cannot recover",
            vec![run(5, 93, "failure")],
            restarted(5),
            "escalated: no poll run started",
        ),
        (
            "applying activity counts",
            vec![run(6, 1, "queued"), run(5, 93, "failure")],
            restarted(5),
            "poll run 6 is queued",
        ),
        (
            "only applying chain recovers",
            vec![
                run(7, 30, "failure"),
                run(6, 40, "success"),
                run(5, 93, "failure"),
            ],
            restarted(5),
            "started poll-apply.yml after poll run 7",
        ),
    ] {
        let mut runs = vec![
            run(10, 0, "in_progress"),
            run(9, 30, "success"),
            run(8, 40, "success"),
        ];
        for run in &mut runs {
            run["path"] = json!(".github/workflows/poll.yml");
            // A mutable apply-looking title is not trusted mode evidence.
            run["display_title"] = json!("pass (apply)");
        }
        runs.extend(applying.into_iter().map(|mut run| {
            run["display_title"] = json!("preview");
            run
        }));
        let comments = record
            .into_iter()
            .map(|r| comment("worker", r.line()))
            .collect();
        let fake = Fake::new(runs, comments);
        let line = super::run(&fake, &configured(), "main", RUN_URL, now()).unwrap();
        assert!(line.starts_with(expected), "{name}: {line}");
    }
}

#[test]
fn legacy_records_keep_authenticated_creation_time() {
    for (name, login, created, body, expected) in [
        (
            "legacy restarted",
            "worker",
            now() - 5 * 60_000,
            "<!-- agent-board-pass-restart restarted after=5 -->",
            "waiting for visibility",
        ),
        (
            "legacy escalated",
            "worker",
            now() - 5 * 60_000,
            "<!-- agent-board-pass-restart escalated after=5 -->",
            "the operator was told",
        ),
        (
            "legacy expired",
            "worker",
            now() - RECORD_LOOKBACK_MS - 1,
            "<!-- agent-board-pass-restart restarted after=5 -->",
            "started poll-apply.yml",
        ),
        (
            "foreign recent record",
            "someone",
            now(),
            "<!-- agent-board-pass-restart restarted after=5 -->",
            "started poll-apply.yml",
        ),
    ] {
        let fake = Fake::new(
            vec![run(5, 93, "failure")],
            vec![json!({
                "user": {"login": login}, "created_at": iso(created), "updated_at": NOW, "body": body
            })],
        );
        let line = super::run(&fake, &configured(), "main", RUN_URL, now()).unwrap();
        assert!(line.contains(expected), "{name}: {line}");
        if name == "legacy restarted" {
            assert_eq!(
                newest_record(&fake, &configured(), now())
                    .unwrap()
                    .unwrap()
                    .created_at,
                created
            );
        }
    }
}

#[test]
fn ambiguous_mode_never_writes_and_full_history_can_restart() {
    for value in [
        json!({"staleMinutes": 25}),
        json!({"staleMinutes": 25, "applyingWorkflow": null}),
    ] {
        let mut cfg = supervisor();
        cfg[OPTION] = value;
        let settings = Settings::from_json(&board(), &cfg, None).unwrap();
        assert!(
            settings
                .unset
                .contains(&"restartPass.applyingWorkflow".into())
        );
        let fake = Fake::new(vec![run(5, 93, "success")], vec![]);
        let line = super::run(&fake, &settings, "main", RUN_URL, now()).unwrap();
        assert!(line.contains("applying recovery is unknown"), "{line}");
        assert!(fake.bodies().is_empty());
    }
    let runs = (1..=RUN_PAGE as u64)
        .rev()
        .map(|id| run(id, 93 + (RUN_PAGE as i64 - id as i64) * 10, "failure"))
        .collect();
    let fake = Fake::new(runs, vec![]);
    let line = super::run(&fake, &configured(), "main", RUN_URL, now()).unwrap();
    assert_eq!(line, "started poll-apply.yml after poll run 100");
    assert_eq!(fake.writes.borrow().len(), 2);
}

#[test]
fn restarts_a_stalled_pass_once_and_records_it_first() {
    let fake = Fake::new(vec![run(5, 93, "failure")], vec![]);
    let line = super::run(&fake, &configured(), "main", RUN_URL, now()).unwrap();
    assert_eq!(line, "started poll-apply.yml after poll run 5");
    let bodies = fake.bodies();
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert!(
        bodies[0].starts_with(&format!(
            "{LOG}: <!-- agent-board-pass-restart restarted after=5 -->\n"
        )),
        "{bodies:?}"
    );
    assert!(bodies[0].contains(RUN_URL), "{bodies:?}");
    assert!(!bodies[0].contains("@owner"), "{bodies:?}");
    assert_eq!(
        fake.writes.borrow()[1],
        (
            DISPATCH.to_owned(),
            json!({"ref": "main", "inputs": {"apply": "true"}})
        )
    );
}

#[test]
fn starts_nothing_while_a_pass_is_queued_or_running() {
    for status in ["queued", "in_progress", "waiting"] {
        let fake = Fake::new(vec![run(6, 1, status), run(5, 93, "failure")], vec![]);
        let line = super::run(&fake, &configured(), "main", RUN_URL, now()).unwrap();
        assert_eq!(line, format!("poll run 6 is {status}; not restarting"));
        assert!(fake.bodies().is_empty(), "{status}");
    }
}

#[test]
fn escalates_a_failed_restart_once() {
    let record = |kind: &str, after: u64| {
        comment(
            "worker",
            format!("<!-- agent-board-pass-restart {kind} after={after} -->\ntext"),
        )
    };
    // The dispatch fails: the record, then one escalation with its error.
    let mut fake = Fake::new(vec![run(5, 93, "failure")], vec![]);
    fake.dispatch_fails = true;
    let line = super::run(&fake, &configured(), "main", RUN_URL, now()).unwrap();
    assert!(line.starts_with("escalated: starting it failed"), "{line}");
    let bodies = fake.bodies();
    assert_eq!(bodies.len(), 3, "{bodies:?}");
    assert!(
        bodies[2].starts_with(&format!(
            "{LOG}: <!-- agent-board-pass-restart escalated after=5 -->\n@owner "
        )),
        "{bodies:?}"
    );
    assert!(bodies[2].contains("HTTP 503"), "{bodies:?}");
    // The restarted pass failed: one escalation, and nothing started.
    let runs = vec![run(6, 30, "failure"), run(5, 93, "failure")];
    let fake = Fake::new(runs.clone(), vec![record("restarted", 5)]);
    super::run(&fake, &configured(), "main", RUN_URL, now()).unwrap();
    let bodies = fake.bodies();
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    assert!(
        bodies[0].contains("@owner")
            && bodies[0].contains("poll run 6 after the restart ended failure"),
        "{bodies:?}"
    );
    // Told once: the next run writes nothing.
    let fake = Fake::new(
        runs.clone(),
        vec![record("restarted", 5), record("escalated", 5)],
    );
    super::run(&fake, &configured(), "main", RUN_URL, now()).unwrap();
    assert!(fake.bodies().is_empty());
    // Only the bot's own records count.
    let fake = Fake::new(
        runs,
        vec![comment(
            "someone",
            "<!-- agent-board-pass-restart escalated after=6 -->".into(),
        )],
    );
    super::run(&fake, &configured(), "main", RUN_URL, now()).unwrap();
    assert!(fake.bodies()[0].contains("restarted after=6"));
}

#[test]
fn off_unless_configured() {
    let s = Settings::from_json(&board(), &supervisor(), None).unwrap();
    assert!(s.restart_pass.is_none());
    assert!(s.unset.contains(&OPTION.to_owned()));
    let fake = Fake::new(vec![run(5, 93, "failure")], vec![]);
    let line = super::run(&fake, &s, "main", RUN_URL, now()).unwrap();
    assert_eq!(line, "restartPass is not configured; nothing to do");
    assert!(fake.bodies().is_empty());
}

#[test]
fn settings() {
    let s = configured();
    assert_eq!(
        s.restart_pass,
        Some(RestartPass {
            stale_minutes: 25,
            inputs: [("apply".into(), "true".into())].into_iter().collect(),
            applying_workflow: Some("poll-apply.yml".into()),
        })
    );
    assert!(!s.unset.contains(&OPTION.to_owned()));
    let parse = |value: Value| {
        let mut cfg = supervisor();
        cfg[OPTION] = value;
        Settings::from_json(&board(), &cfg, None).map(|s| s.restart_pass)
    };
    assert_eq!(parse(Value::Null).unwrap(), None);
    assert!(parse(json!({"staleMinutes": 20})).unwrap().is_some());
    for refused in [
        json!(25),
        json!({}),
        json!({"staleMinutes": 19}),
        json!({"staleMinutes": 1441}),
        json!({"staleMinutes": "25"}),
        json!({"staleMinutes": 25, "branch": "other"}),
        json!({"staleMinutes": 25, "applyingWorkflow": true}),
        json!({"staleMinutes": 25, "applyingWorkflow": "../poll.yml"}),
        json!({"staleMinutes": 25, "applyingWorkflow": ""}),
        json!({"staleMinutes": 25, "inputs": ["apply"]}),
        json!({"staleMinutes": 25, "inputs": {"apply": true}}),
        json!({"staleMinutes": 25, "inputs": {"a b": "x"}}),
        json!({"staleMinutes": 25, "inputs": {"x": "y".repeat(MAX_INPUT_BYTES + 1)}}),
        json!({"staleMinutes": 25, "inputs": (0..=MAX_INPUTS).map(|i| (format!("i{i}"), json!("x"))).collect::<Map<_, _>>()}),
    ] {
        assert!(parse(refused.clone()).is_err(), "{refused}");
    }
    // It records and escalates on the agent's log issue.
    let mut cfg = supervisor();
    cfg[OPTION] = json!({"staleMinutes": 25});
    cfg.as_object_mut().unwrap().remove("agent");
    let error = Settings::from_json(&board(), &cfg, None).unwrap_err();
    assert!(
        format!("{error:#}").contains("needs the agent's log issue"),
        "{error:#}"
    );
}
