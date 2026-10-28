use super::*;
use handback::{Entry, HandBack, report};

/// This run is run 1, number 50, of example/control's supervisor.yml.
const OWN_NUMBER: u64 = 50;
const DONE: &str = "`dispatch` example/project #9 (research, large, no-plan): done, u";

/// The agent's safe outputs: a noop and its comment.
fn outputs(body: &str) -> String {
    format!(
        "{}\n{}\n",
        json!({"type": "noop", "message": "x"}),
        json!({"type": "add_comment", "body": body})
    )
}

fn ledger(number: u64, blocked: Value, entries: &[(&str, &str)]) -> Value {
    let entries: Vec<Value> = entries
        .iter()
        .map(|(at, action)| json!({"at": at, "action": action, "run": "https://github.com/example/control/actions/runs/7", "line": DONE}))
        .collect();
    json!({"schema": "agent-board/supervisor-ledger/v1", "runNumber": number,
        "blockedUntil": blocked, "entries": entries})
}

/// A run of the supervisor's workflow, scheduled on main, with `fields`
/// changed.
fn workflow_run(number: u64, created: &str, fields: Value) -> Value {
    let mut run = json!({"id": 100 + number, "run_number": number, "created_at": created,
        "status": "completed", "event": "schedule", "path": ".github/workflows/supervisor.yml",
        "head_branch": "main", "head_repository": {"full_name": "example/control"}});
    for (k, v) in fields.as_object().unwrap() {
        run[k] = v.clone();
    }
    run
}

/// Run 40 of the workflow on main wrote the previous ledger.
fn previous(fake: &mut Fake, value: Value) -> Option<(u64, Value)> {
    fake.objects.insert(
        "/repos/example/control/actions/runs/140".into(),
        workflow_run(40, "2026-10-16T07:00:00Z", json!({})),
    );
    Some((140, value))
}

/// The agent job and an execute job with this status and conclusion.
fn jobs(status: &str, conclusion: &str) -> Value {
    json!({"total_count": 2, "jobs": [
        {"name": "agent / run", "status": "completed", "conclusion": "success"},
        {"name": "execute", "status": status, "conclusion": conclusion}]})
}

/// This run, created now, and others with their jobs.
fn list_runs(fake: &mut Fake, runs: Vec<(Value, Value)>) {
    let mut listed = vec![json!({"id": 1, "run_number": OWN_NUMBER, "created_at": NOW})];
    for (run, jobs) in runs {
        fake.objects.insert(
            format!(
                "/repos/example/control/actions/runs/{}/jobs?filter=all&per_page=100",
                run["id"]
            ),
            jobs,
        );
        listed.push(run);
    }
    fake.objects
        .insert("own runs".into(), json!({"workflow_runs": listed}));
}

/// Earlier runs as `(number, created, execute job's status and conclusion)`.
fn own_runs(fake: &mut Fake, runs: &[(u64, &str, &str, &str)]) {
    let runs = (runs.iter())
        .map(|(number, created, status, conclusion)| {
            let run = if *status == "completed" {
                "completed"
            } else {
                "in_progress"
            };
            (
                workflow_run(*number, created, json!({"status": run})),
                jobs(status, conclusion),
            )
        })
        .collect();
    list_runs(fake, runs);
}

fn hand_back_ctx(outputs: &str, previous: Option<(u64, Value)>, attempt: u64) -> Run {
    Run {
        source: Source::HandBack(HandBack {
            outputs: outputs.into(),
            previous,
            repository: "example/control".into(),
            run_id: 1,
            run_number: OWN_NUMBER,
        }),
        run_url: RUN_URL.into(),
        default_branch: "main".into(),
        attempt,
        now: millis(&json!(NOW)).unwrap(),
    }
}

fn dispatches(n: u64) -> String {
    let requests: Vec<Value> = (3..3 + n).map(|i| request(json!({"item": i}))).collect();
    outputs(&block(&requests))
}

#[test]
fn the_report_is_the_one_comment_in_the_safe_outputs() {
    let comment = |body: Value| json!({"type": "add_comment", "body": body}).to_string();
    let cases: Vec<(String, Result<&str, &str>)> = vec![
        (String::new(), Ok("")),
        (json!({"type": "noop", "message": "x"}).to_string(), Ok("")),
        (outputs("Why."), Ok("Why.")),
        (
            format!("{}\n{}", comment(json!("a")), comment(json!("b"))),
            Err("More than one comment"),
        ),
        (comment(json!(null)), Err("Malformed safe outputs")),
        (comment(json!(["a"])), Err("Malformed safe outputs")),
        ("[1]".into(), Err("Malformed safe outputs")),
        ("{\"secret\": ".into(), Err("Malformed safe outputs")),
        ("x".repeat(1024 * 1024 + 1), Err("exceed 1 MiB")),
    ];
    for (text, expected) in cases {
        match (report(&text), expected) {
            (Ok(body), Ok(want)) => assert_eq!(body, want),
            (Err(error), Err(want)) => assert!(error.to_string().contains(want), "{error}"),
            (got, want) => panic!("{got:?} for {want:?}"),
        }
    }
}

#[test]
fn a_ledger_is_read_strictly_and_written_back_alike() {
    let good = ledger(
        40,
        json!("2026-10-16T13:00:00.000Z"),
        &[("2026-10-16T08:00:00.000Z", "dispatch")],
    );
    let parsed = Ledger::parse(&good).unwrap();
    assert_eq!(parsed.entries.len(), 1);
    assert_eq!(parsed.to_json(), good);
    let edit = |f: &dyn Fn(&mut Value)| {
        let mut v = good.clone();
        f(&mut v);
        v
    };
    for bad in [
        edit(&|v| v["schema"] = json!("agent-board/supervisor-ledger/v2")),
        edit(&|v| v["extra"] = json!(1)),
        edit(&|v| {
            v.as_object_mut().unwrap().remove("blockedUntil");
        }),
        edit(&|v| v["runNumber"] = json!(-1)),
        edit(&|v| v["blockedUntil"] = json!("tomorrow")),
        edit(&|v| v["entries"] = json!({})),
        edit(&|v| v["entries"][0]["extra"] = json!(1)),
        edit(&|v| v["entries"][0]["at"] = json!("yesterday")),
        edit(&|v| v["entries"][0]["action"] = json!(1)),
        edit(&|v| v["entries"][0]["line"] = json!("a\n- forged")),
        edit(&|v| v["entries"][0]["line"] = json!("x".repeat(1001))),
        edit(&|v| v["entries"][0]["run"] = json!("http://example.com/")),
        edit(&|v| v["entries"][0]["run"] = json!("javascript:alert(1)")),
        edit(&|v| v["entries"][0]["run"] = json!("https://github.com/x) [y](z")),
        edit(&|v| v["entries"] = json!(vec![v["entries"][0].clone(); 1001])),
    ] {
        assert!(Ledger::parse(&bad).is_err(), "{bad}");
    }
}

#[test]
fn a_hand_back_counts_from_its_ledger_and_blocks_a_day_after_a_lost_one() {
    let recent = "2026-10-16T08:00:00.000Z";
    let old = "2026-10-15T11:00:00.000Z";
    // Previous ledger, earlier runs, dispatches asked for, and the refusal
    // expected (none when it goes ahead) with the ledger's block.
    type Case<'a> = (
        Option<Value>,
        Vec<(u64, &'a str, &'a str, &'a str)>,
        u64,
        Option<&'a str>,
        Option<&'a str>,
    );
    let cases: Vec<Case> = vec![
        // The first run: nothing counted, nothing lost.
        (None, vec![], 1, None, None),
        // Two of three used: one more fits, two do not; a day-old entry is gone.
        (
            Some(ledger(
                40,
                Value::Null,
                &[(recent, "dispatch"), (recent, "dispatch")],
            )),
            vec![],
            1,
            None,
            None,
        ),
        (
            Some(ledger(
                40,
                Value::Null,
                &[(recent, "dispatch"), (recent, "dispatch")],
            )),
            vec![],
            2,
            Some("daily dispatch cap"),
            None,
        ),
        (
            Some(ledger(
                40,
                Value::Null,
                &[(recent, "dispatch"), (old, "dispatch")],
            )),
            vec![],
            2,
            None,
            None,
        ),
        // Other actions do not count against dispatches.
        (
            Some(ledger(
                40,
                Value::Null,
                &[
                    (recent, "dispatch"),
                    (recent, "comment"),
                    (recent, "comment"),
                ],
            )),
            vec![],
            2,
            None,
            None,
        ),
        // A run since the ledger's that executed left none: a day and the
        // margin after it was created.
        (
            Some(ledger(40, Value::Null, &[])),
            vec![(41, "2026-10-16T09:00:00Z", "completed", "failure")],
            1,
            Some("writes wait until 2026-10-17T21:00:00.000Z"),
            Some("2026-10-17T21:00:00.000Z"),
        ),
        (
            None,
            vec![(41, "2026-10-16T09:00:00Z", "in_progress", "")],
            1,
            Some("writes wait"),
            Some("2026-10-17T21:00:00.000Z"),
        ),
        // A later run's execute job may finish before this one's starts.
        (
            Some(ledger(40, Value::Null, &[])),
            vec![(51, "2026-10-16T11:00:00Z", "completed", "success")],
            1,
            Some("writes wait"),
            Some("2026-10-17T23:00:00.000Z"),
        ),
        // Not without a ledger either, but an empty batch still goes ahead.
        (
            None,
            vec![(41, "2026-10-16T09:00:00Z", "completed", "success")],
            0,
            None,
            Some("2026-10-17T21:00:00.000Z"),
        ),
        // One whose execute job was skipped or never left the queue wrote
        // nothing, nor did one before the ledger's.
        (
            Some(ledger(40, Value::Null, &[])),
            vec![
                (41, "2026-10-16T09:00:00Z", "completed", "skipped"),
                (42, "2026-10-16T10:00:00Z", "queued", ""),
                (39, "2026-10-16T07:00:00Z", "completed", "success"),
                (51, "2026-10-16T11:59:00Z", "waiting", ""),
            ],
            1,
            None,
            None,
        ),
        // The block is carried forward until it ends.
        (
            Some(ledger(40, json!("2026-10-16T13:00:00Z"), &[])),
            vec![],
            1,
            Some("writes wait until 2026-10-16T13:00:00.000Z"),
            Some("2026-10-16T13:00:00.000Z"),
        ),
        (
            Some(ledger(40, json!("2026-10-16T11:00:00Z"), &[])),
            vec![],
            1,
            None,
            None,
        ),
    ];
    for (before, runs, n, refused, blocked) in cases {
        let mut fake = Fake::new("");
        let prior = before.clone().and_then(|v| previous(&mut fake, v));
        own_runs(&mut fake, &runs);
        let s = settings();
        let p = policy(&s);
        let outcome = execute(&fake, &p, &hand_back_ctx(&dispatches(n), prior, 1));
        let label = format!("{before:?} {runs:?} {n}");
        let ledger = outcome.ledger.as_ref().expect(&label);
        assert_eq!(ledger.run_number, OWN_NUMBER, "{label}");
        assert_eq!(ledger.blocked_until.map(iso).as_deref(), blocked, "{label}");
        // The log issue is read for nothing, and written only by `record`.
        assert!(
            !fake
                .calls
                .borrow()
                .iter()
                .any(|(_, p, _)| p.contains("/issues/7/")),
            "{label}"
        );
        assert!(
            !fake
                .calls
                .borrow()
                .iter()
                .any(|(_, p, _)| p.contains("/issues/5")),
            "{label}"
        );
        match refused {
            Some(message) => {
                assert!(!outcome.ok, "{label}");
                assert!(
                    outcome.lines[0].contains(message),
                    "{label}: {:?}",
                    outcome.lines
                );
                assert_eq!(fake.writes(), [], "{label}");
            }
            None => {
                assert!(outcome.ok, "{label}: {:?}", outcome.lines);
                assert_eq!(fake.writes().len() as u64, n, "{label}");
                let added: Vec<&Entry> = (ledger.entries.iter())
                    .filter(|e| e.run == RUN_URL)
                    .collect();
                assert_eq!(added.len() as u64, n, "{label}");
                assert!(
                    added.iter().all(|e| e.action == "dispatch"
                        && e.line
                            .contains(": done, https://github.com/example/project/issues/")),
                    "{label}"
                );
                assert!(
                    ledger
                        .entries
                        .iter()
                        .all(|e| e.at > millis(&json!(old)).unwrap())
                );
            }
        }
    }
}

#[test]
fn a_hand_back_refuses_what_it_cannot_trust_and_writes_no_ledger() {
    let wrong_run = |fields: Value| {
        move |fake: &mut Fake| {
            fake.objects.insert(
                "/repos/example/control/actions/runs/140".into(),
                workflow_run(40, "2026-10-16T07:00:00Z", fields.clone()),
            );
        }
    };
    // A name, the previous ledger, how the fake differs and the refusal.
    type Case<'a> = (&'a str, Value, Box<dyn Fn(&mut Fake)>, &'a str);
    let cases: Vec<Case> = vec![
        (
            "another workflow's artifact",
            ledger(40, Value::Null, &[]),
            Box::new(wrong_run(json!({"path": ".github/workflows/ci.yml"}))),
            "not an earlier run",
        ),
        (
            "another branch's run",
            ledger(40, Value::Null, &[]),
            Box::new(wrong_run(json!({"head_branch": "feature"}))),
            "not an earlier run",
        ),
        // A pull request runs its own copy of the workflow file, from a
        // fork whose branch may be called main.
        (
            "a pull request's run",
            ledger(40, Value::Null, &[]),
            Box::new(wrong_run(json!({"event": "pull_request"}))),
            "not an earlier run",
        ),
        (
            "a fork's run",
            ledger(40, Value::Null, &[]),
            Box::new(wrong_run(
                json!({"head_repository": {"full_name": "visitor/control"}}),
            )),
            "not an earlier run",
        ),
        (
            "another run's number",
            ledger(39, Value::Null, &[]),
            Box::new(wrong_run(json!({}))),
            "not an earlier run",
        ),
        (
            "a later run",
            ledger(OWN_NUMBER, Value::Null, &[]),
            Box::new(wrong_run(json!({"run_number": OWN_NUMBER}))),
            "not an earlier run",
        ),
        (
            "too many jobs to check",
            ledger(40, Value::Null, &[]),
            Box::new(move |fake: &mut Fake| {
                wrong_run(json!({}))(fake);
                let mut many = jobs("completed", "success");
                many["total_count"] = json!(101);
                list_runs(
                    fake,
                    vec![(workflow_run(41, "2026-10-16T09:00:00Z", json!({})), many)],
                );
            }),
            "too many jobs",
        ),
        (
            "malformed",
            json!({"schema": "agent-board/supervisor-ledger/v1"}),
            Box::new(wrong_run(json!({}))),
            "malformed",
        ),
        (
            "runs unreadable",
            ledger(40, Value::Null, &[]),
            Box::new(move |fake: &mut Fake| {
                wrong_run(json!({}))(fake);
                fake.objects.insert("own runs".into(), json!({}));
            }),
            "workflow runs",
        ),
        (
            "too many runs",
            ledger(40, Value::Null, &[]),
            Box::new(move |fake: &mut Fake| {
                wrong_run(json!({}))(fake);
                let runs = vec![json!({"id": 9, "run_number": 1, "created_at": NOW}); 100];
                fake.objects
                    .insert("own runs".into(), json!({"workflow_runs": runs}));
            }),
            "too many",
        ),
    ];
    let s = settings();
    let p = policy(&s);
    for (name, value, setup, message) in cases {
        let mut fake = Fake::new("");
        setup(&mut fake);
        let outcome = execute(
            &fake,
            &p,
            &hand_back_ctx(&dispatches(1), Some((140, value)), 1),
        );
        assert!(!outcome.ok, "{name}");
        assert!(
            outcome.lines[0].contains(message),
            "{name}: {:?}",
            outcome.lines
        );
        assert_eq!(outcome.ledger, None, "{name}");
        assert_eq!(fake.writes(), [], "{name}");
    }
    // A re-run reads nothing and hands on nothing.
    let fake = Fake::new("");
    let outcome = execute(&fake, &p, &hand_back_ctx(&dispatches(1), None, 2));
    assert_eq!(outcome.ledger, None);
    assert!(fake.calls.borrow().is_empty());
    // Bad requests refuse the batch but hand the ledger on.
    let fake = Fake::new("");
    let bad = outputs(&block(&[request(json!({"kind": "implement"}))]));
    let outcome = execute(&fake, &p, &hand_back_ctx(&bad, None, 1));
    assert!(!outcome.ok);
    assert_eq!(outcome.ledger.unwrap().entries, []);
    assert_eq!(fake.writes(), []);
}

#[test]
fn only_the_supervisors_own_runs_that_may_have_written_block() {
    let s = settings();
    let p = policy(&s);
    let created = "2026-10-16T09:00:00Z";
    let only = |name: &str| json!({"total_count": 1, "jobs": [{"name": name, "status": "completed", "conclusion": "success"}]});
    let cases = [
        // A pull request's or a fork's copy of the workflow writes with no
        // token of the bot's, and must not block the supervisor either.
        (
            json!({"event": "pull_request"}),
            jobs("completed", "success"),
            false,
        ),
        (
            json!({"head_repository": {"full_name": "visitor/control"}}),
            jobs("completed", "success"),
            false,
        ),
        (
            json!({"event": "workflow_dispatch"}),
            jobs("completed", "success"),
            true,
        ),
        (
            json!({"event": "issue_comment"}),
            jobs("completed", "success"),
            true,
        ),
        // A trigger whose ledger is not accepted may still have written.
        (
            json!({"event": "workflow_run"}),
            jobs("completed", "success"),
            true,
        ),
        (
            json!({"event": "pull_request_target"}),
            jobs("completed", "success"),
            false,
        ),
        // The job in a reusable workflow, or renamed so that none matches.
        (json!({}), only("execute / run"), true),
        (json!({}), only("executor"), true),
        (json!({"status": "in_progress"}), only("agent / run"), false),
        // A run that started no job wrote nothing.
        (json!({}), json!({"total_count": 0, "jobs": []}), false),
    ];
    for (fields, jobs, blocks) in cases {
        let mut fake = Fake::new("");
        let run = workflow_run(41, created, fields.clone());
        list_runs(&mut fake, vec![(run, jobs.clone())]);
        let outcome = execute(&fake, &p, &hand_back_ctx(&dispatches(1), None, 1));
        assert_eq!(outcome.ok, !blocks, "{fields} {jobs}: {:?}", outcome.lines);
        assert_eq!(
            outcome.ledger.unwrap().blocked_until.is_some(),
            blocks,
            "{fields} {jobs}"
        );
    }
    // A run created longer ago than the margin writes nothing; the ledger
    // still goes on.
    let mut fake = Fake::new("");
    fake.objects.insert(
        "own runs".into(),
        json!({"workflow_runs": [{"id": 1, "run_number": OWN_NUMBER, "created_at": "2026-10-16T05:59:00Z"}]}),
    );
    let outcome = execute(&fake, &p, &hand_back_ctx(&dispatches(1), None, 1));
    assert!(outcome.lines[0].contains("created too long ago"));
    assert!(outcome.ledger.is_some());
    assert_eq!(fake.writes(), []);
}

#[test]
fn a_ledger_written_after_a_long_error_reads_back() {
    let s = settings();
    let p = policy(&s);
    let mut fake = Fake::new("");
    fake.fail = Some("/repos/example/control/actions/workflows/dispatch.yml/dispatches".into());
    let mut outcome = execute(&fake, &p, &hand_back_ctx(&dispatches(1), None, 1));
    outcome.lines[0] = format!("- failed (HTTP 422\r\n{}é)", "x".repeat(2000));
    let mut ledger = outcome.ledger.unwrap();
    ledger.entries.clear();
    ledger.reserve(
        &judgment::parse_batch(&lines(&[request(json!({}))]), &p).unwrap(),
        &hand_back_ctx("", None, 1),
    );
    ledger.settle(&outcome.lines);
    let read = Ledger::parse(&ledger.to_json()).unwrap();
    assert_eq!(read, ledger);
    assert!(read.entries[0].line.starts_with("failed (HTTP 422  xx"));
    assert_eq!(read.entries[0].line.len(), 1000);
}

#[test]
fn a_failed_dispatch_stays_counted_and_one_not_attempted_does_not() {
    let s = settings();
    let p = policy(&s);
    let mut fake = Fake::new("");
    fake.fail = Some("/repos/example/control/actions/workflows/dispatch.yml/dispatches".into());
    let outcome = execute(&fake, &p, &hand_back_ctx(&dispatches(2), None, 1));
    assert!(!outcome.ok);
    let entries = outcome.ledger.unwrap().entries;
    assert_eq!(entries.len(), 1);
    assert!(entries[0].line.starts_with("`dispatch` example/project #3"));
    assert!(entries[0].line.contains(": failed (POST "));
}

#[test]
fn the_status_comment_on_the_log_is_replaced_in_place() {
    let s = settings();
    let p = policy(&s);
    let status = |author: &str, id: u64| {
        json!({"id": id, "user": {"login": author},
            "body": "<!-- agent-board-supervisor-status -->\nold"})
    };
    let entry = json!({"id": 3, "user": {"login": "worker"}, "body": "Supervisor [run](u), requests:\n- no action requested"});
    let cases = [
        // None yet, or only someone else's copy: a new one.
        (
            json!([entry.clone()]),
            "POST /repos/example/operator/issues/7/comments",
        ),
        (
            json!([entry.clone(), status("someone", 4)]),
            "POST /repos/example/operator/issues/7/comments",
        ),
        // The bot's newest is replaced; earlier entries are left as they are.
        (
            json!([status("worker", 5), entry, status("worker", 6)]),
            "PATCH /repos/example/operator/issues/comments/6",
        ),
    ];
    for (log, write) in cases {
        let mut fake = Fake::new("");
        let prior = previous(
            &mut fake,
            ledger(40, Value::Null, &[("2026-10-16T08:00:00.000Z", "dispatch")]),
        );
        fake.objects.insert("log".into(), log.clone());
        let ctx = hand_back_ctx(&dispatches(1), prior, 1);
        let outcome = execute(&fake, &p, &ctx);
        assert!(outcome.ok, "{:?}", outcome.lines);
        let body = record(&fake, &p, &ctx, &outcome).unwrap();
        let writes = fake.writes();
        let (method, path, data) = writes.last().unwrap();
        assert_eq!(format!("{method} {path}"), write, "{log}");
        assert_eq!(data["body"], body);
        assert_eq!(
            body,
            format!(
                "<!-- agent-board-supervisor-status -->\nThe supervisor's executor replaces this comment on each run.\n\nSupervisor [run]({RUN_URL}), requests:\n- `dispatch` example/project #3 (research, large, no-plan): done, https://github.com/example/project/issues/3\n\nEarlier in the last 24 hours:\n- 2026-10-16T08:00:00.000Z [run](https://github.com/example/control/actions/runs/7): {DONE}"
            )
        );
    }
}
