use super::*;
use crate::supervise::{
    Bounded,
    tests::{base64, settings},
};
use std::cell::{Cell, RefCell};

const SNAPSHOTS: &str = include_str!("../../../../../fixtures/supervisor-snapshots.json");

fn recorded() -> (Value, i64) {
    let snapshots: Value = serde_json::from_str(SNAPSHOTS).unwrap();
    let now = millis(&snapshots["now"]).unwrap();
    (snapshots, now)
}

fn with(id: &str, items: Option<Vec<Value>>) -> Snapshot {
    Snapshot {
        observations: BTreeMap::from([(id.to_owned(), items)]),
        previous: None,
    }
}

/// The capacity margin the tests configure; it has no default.
const CAPACITY_MARGIN: f64 = 10.0;

fn limits() -> Limits {
    Limits {
        capacity_margin_percent: Some(CAPACITY_MARGIN),
        ..Limits::default()
    }
}

fn check(snapshot: &Snapshot, id: &str, now: i64) -> Value {
    evaluate(snapshot, now, &limits())
        .into_iter()
        .find(|c| c["id"] == id)
        .unwrap()
}

fn status(id: &str, items: Value, now: i64) -> String {
    let items = items.as_array().unwrap().clone();
    check(&with(id, Some(items)), id, now)["status"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn recorded_reduced_snapshots_healthy_and_unavailable_observations() {
    let (snapshots, now) = recorded();
    let evidence = "https://github.com/example/project/issues/1";
    for (id, items) in snapshots.as_object().unwrap() {
        if id == "now" {
            continue;
        }
        let items: Vec<Value> = items
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                let mut i = i.clone();
                i["html_url"] = evidence.into();
                i
            })
            .collect();
        let result = check(&with(id, Some(items.clone())), id, now);
        let expected = if ["infrastructure", "unset", "capacity"].contains(&id.as_str()) {
            "warn"
        } else {
            "fail"
        };
        assert_eq!(result["status"], expected, "{id}");
        // An unset option's evidence is its name.
        let mut want = vec![json!(evidence)];
        want.extend(items.iter().filter_map(|i| i.get("option").cloned()));
        assert_eq!(result["evidence"], json!(want), "{id}");
        assert_eq!(check(&with(id, None), id, now)["status"], "warn", "{id}");
        let healthy = if id == "broker" {
            vec![json!({
                "status": "completed",
                "conclusion": "success",
                "updated_at": snapshots["now"],
            })]
        } else {
            vec![]
        };
        assert_eq!(
            check(&with(id, Some(healthy)), id, now)["status"],
            "ok",
            "{id}"
        );
    }
    // Every check has a recorded snapshot.
    for (id, _) in CHECKS {
        assert!(snapshots.get(*id).is_some(), "{id}");
    }
}

#[test]
fn threshold_boundaries_and_stale_probe() {
    let (s, now) = recorded();
    let limits = Limits::default();
    let poll = s["poll"].as_array().unwrap();
    assert_eq!(status("poll", json!(poll[1..]), now), "ok");
    let mut newer = vec![json!({"status": "completed", "conclusion": "success"})];
    newer.extend(poll.iter().cloned());
    assert_eq!(status("poll", json!(newer), now), "ok");
    // A cancelled or skipped run between failures is passed over, not a pass.
    for between in ["cancelled", "skipped", "success"] {
        let mut runs = poll.clone();
        runs.insert(1, json!({"status": "completed", "conclusion": between}));
        let want = if between == "success" { "ok" } else { "fail" };
        assert_eq!(status("poll", json!(runs), now), want, "{between}");
        assert_eq!(status("poll", json!(runs[..3]), now), "ok", "{between}");
    }
    // Past the timeout fails until a week, when GitHub may no longer cancel it.
    let hours = |h: f64| iso(now - (h * HOUR_MS as f64) as i64);
    let timeout = limits.run_timeout_minutes / 60.0;
    for (started, status_now, want) in [
        (timeout, "in_progress", "ok"),
        (timeout + 0.1, "in_progress", "fail"),
        (timeout + 0.1, "completed", "ok"),
        (ABANDONED_RUN_HOURS, "queued", "fail"),
        (ABANDONED_RUN_HOURS + 0.1, "queued", "ok"),
        (ABANDONED_RUN_HOURS + 0.1, "in_progress", "ok"),
    ] {
        assert_eq!(
            status(
                "runs",
                json!([{"status": status_now, "run_started_at": hours(started)}]),
                now
            ),
            want,
            "{started} {status_now}"
        );
    }
    // Without a start time, the creation time counts.
    assert_eq!(
        status(
            "runs",
            json!([{"status": "queued", "run_started_at": null, "created_at": hours(24.0 * 30.0)}]),
            now
        ),
        "ok"
    );
    assert_eq!(
        status(
            "broker",
            json!([{
                "status": "completed",
                "conclusion": "success",
                "updated_at": "2026-10-12T00:00:00Z",
            }]),
            now
        ),
        "fail"
    );
    // A probe still running, cancelled waiting on its concurrency group or
    // skipped has observed nothing: the newest completed probe decides, and
    // one that hangs is caught by that probe going stale.
    let success = json!({"status": "completed", "conclusion": "success", "updated_at": s["now"]});
    let stale = json!({
        "status": "completed",
        "conclusion": "success",
        "updated_at": "2026-10-12T00:00:00Z",
    });
    for unfinished in [
        json!({"status": "in_progress", "conclusion": null, "updated_at": s["now"]}),
        json!({"status": "queued", "conclusion": null, "updated_at": s["now"]}),
        json!({"status": "completed", "conclusion": "cancelled", "updated_at": s["now"]}),
        json!({"status": "completed", "conclusion": "skipped", "updated_at": s["now"]}),
    ] {
        let pair = json!([unfinished, success]);
        assert_eq!(status("broker", pair, now), "ok", "{unfinished}");
        let pair = json!([unfinished, stale]);
        assert_eq!(status("broker", pair, now), "fail", "{unfinished}");
        let mut failed = vec![unfinished.clone()];
        failed.extend(s["broker"].as_array().unwrap().iter().cloned());
        assert_eq!(status("broker", json!(failed), now), "fail", "{unfinished}");
        // Alone, only a skipped run says the probe is off.
        let alone = check(
            &with("broker", Some(vec![unfinished.clone()])),
            "broker",
            now,
        );
        let off = unfinished["conclusion"] == "skipped";
        assert_eq!(
            alone["status"],
            if off { "ok" } else { "warn" },
            "{unfinished}"
        );
        assert_eq!(alone["suggested_action"] == PROBE_OFF, off, "{unfinished}");
    }
    // Nothing listed is no observation, not a probe switched off.
    assert_eq!(status("broker", json!([]), now), "warn");
    assert_eq!(
        status(
            "queue",
            json!([{"event": "removed_from_merge_queue", "created_at": "2026-10-12T00:00:00Z"}]),
            now
        ),
        "ok"
    );
    assert_eq!(status("epics", json!([{"missing": true}]), now), "warn");
    assert_eq!(
        status(
            "epics",
            json!([{"missing": true}, {"failures": limits.epic_failures}]),
            now
        ),
        "fail"
    );
    assert_eq!(
        status(
            "epics",
            json!([{"at": "2026-01-01T00:00:00Z", "applied": "unchanged"}]),
            now
        ),
        "ok"
    );
    assert_eq!(
        status(
            "epics",
            json!([{"pending": {"at": "2026-01-01T00:00:00Z"}}]),
            now
        ),
        "fail"
    );
}

#[test]
fn newer_checks_and_their_boundaries() {
    let (_, now) = recorded();
    let limits = Limits::default();
    let hours = |h: f64| iso(now - (h * HOUR_MS as f64) as i64);
    assert_eq!(
        status("idle", json!([{"idle": true}, {"idle": false}]), now),
        "ok"
    );
    assert_eq!(status("idle", json!([{"idle": true}]), now), "ok");
    assert_eq!(
        status("stuck", json!([{"ended": true, "pull": "none"}]), now),
        "fail"
    );
    assert_eq!(
        status(
            "stuck",
            json!([{"ended": true, "pull": "open"}, {"ended": true, "pull": "closed"}, {"ended": false, "pull": null}]),
            now
        ),
        "ok"
    );
    // Two days of a week elapsed is 28.6%: 50% used is within 25 points, 54% not.
    let week = |used: u64| json!([{"used_percent": used, "window_minutes": 10080, "resets_at": "2026-10-20T12:00:00Z"}]);
    assert_eq!(status("pace", week(50), now), "ok");
    assert_eq!(status("pace", week(54), now), "fail");
    assert_eq!(status("pace", week(0), now), "fail");
    // A pool under a pace override shows, on or off its pace, and only warns
    // unless another pool is off its pace.
    let spend = json!("pace override: large override spend until 2026-10-18T00:00Z");
    for used in [0, 50] {
        let mut items = week(used);
        items[0]["override"] = spend.clone();
        assert_eq!(status("pace", items, now), "warn", "{used}");
    }
    let mut items = week(54);
    items[0]["override"] = spend.clone();
    items
        .as_array_mut()
        .unwrap()
        .extend(week(0).as_array().unwrap().clone());
    assert_eq!(status("pace", items, now), "fail");
    assert_eq!(
        status(
            "pace",
            json!([{"used_percent": 99, "window_minutes": 10080, "resets_at": hours(1.0)}]),
            now
        ),
        "ok"
    );
    assert_eq!(
        status(
            "pace",
            json!([{"used_percent": 10, "window_minutes": 60, "resets_at": hours(-2.0)}]),
            now
        ),
        "ok"
    );
    assert_eq!(status("failures", json!([{"reason": null}]), now), "ok");
    assert_eq!(
        status(
            "held",
            json!([{"labeledAt": hours(limits.hold_wait_hours - 1.0)}]),
            now
        ),
        "ok"
    );
    assert_eq!(status("held", json!([{}]), now), "fail");
    // A draft in the promote queue is the operator's, however long it waits.
    assert_eq!(status("held", json!([{"promote": true}]), now), "ok");
    assert_eq!(
        status("held", json!([{"promote": true}, {"promote": false}]), now),
        "fail"
    );
    // Under capacity by more than the margin, with a pass gone idle, warns;
    // the blockers lead the evidence.
    let pool = |allowed: f64, used: f64| {
        json!({"kind": "pool", "allowed_percent": allowed, "used_percent": used,
            "html_url": "https://github.com/o/ops/issues/1#issuecomment-1"})
    };
    let pass = |idle: bool| json!({"kind": "pass", "idle": idle, "html_url": "https://github.com/o/c/actions/runs/1"});
    let blocker = json!({"kind": "blocker", "html_url": "https://github.com/o/r/pull/3"});
    let under = 50.0 - CAPACITY_MARGIN - 0.1;
    for (items, want) in [
        (vec![pool(50.0, under), pass(true)], "warn"),
        (vec![pool(50.0, 50.0 - CAPACITY_MARGIN), pass(true)], "ok"),
        (vec![pool(50.0, under), pass(false)], "ok"),
        (vec![pool(50.0, under)], "ok"),
        (vec![pass(true), blocker.clone()], "ok"),
        (
            vec![pool(50.0, 49.0), pool(50.0, under), pass(false), pass(true)],
            "warn",
        ),
        (vec![json!({"kind": "pool"}), pass(true)], "ok"),
    ] {
        assert_eq!(status("capacity", json!(items), now), want, "{items:?}");
    }
    let items = vec![pool(50.0, 0.0), pass(true), blocker];
    let result = check(&with("capacity", Some(items.clone())), "capacity", now);
    assert_eq!(
        result["evidence"],
        json!([
            "https://github.com/o/r/pull/3",
            "https://github.com/o/ops/issues/1#issuecomment-1",
            "https://github.com/o/c/actions/runs/1"
        ])
    );
    // Without a margin the check reads nothing; given items, it stays ok.
    let off = evaluate(&with("capacity", Some(items)), now, &Limits::default());
    let off = off.iter().find(|c| c["id"] == "capacity").unwrap();
    assert_eq!(off["status"], "ok");
    assert_eq!(
        status(
            "unqueued",
            json!([{"approvedAt": hours(2.0), "clean": true, "queued": true}, {"approvedAt": hours(2.0), "clean": false, "queued": false}]),
            now
        ),
        "ok"
    );
    assert_eq!(
        status(
            "unqueued",
            json!([{"approvedAt": hours(0.5), "clean": true, "queued": false}]),
            now
        ),
        "ok"
    );
    assert_eq!(
        status(
            "unclosed",
            json!([{"mergedAt": hours(2.0), "issueState": "closed"}, {"mergedAt": hours(0.5), "issueState": "open"}]),
            now
        ),
        "ok"
    );
    let unclosed = vec![json!({"mergedAt": hours(2.0), "issueState": "open",
        "html_url": "https://github.com/o/r/pull/2", "issue_url": "https://github.com/o/r/issues/1"})];
    assert_eq!(
        check(&with("unclosed", Some(unclosed)), "unclosed", now)["evidence"],
        json!([
            "https://github.com/o/r/pull/2",
            "https://github.com/o/r/issues/1"
        ])
    );
    // One failed scheduled run may be a flake; a success among them is a recovery.
    assert_eq!(
        status(
            "scheduled",
            json!([{"runs": 1, "failed": 1}, {"runs": 3, "failed": 2}, {"runs": 0, "failed": 0}]),
            now
        ),
        "ok"
    );
    let many: Vec<Value> = (0..30)
        .map(|i| json!({"reason": "secret", "html_url": format!("https://github.com/o/r/actions/runs/{i}")}))
        .collect();
    let evidence = check(&with("failures", Some(many)), "failures", now)["evidence"].clone();
    assert_eq!(evidence.as_array().unwrap().len(), limits.max_evidence);
}

#[test]
fn unset_options_warn_by_name_and_none_is_ok() {
    let (_, now) = recorded();
    let fake = Fake::new(|_: &str, _: &str, _: &Value| -> Result<Value> { bail!("HTTP 403") });
    let mut s = settings();
    let snapshot = collect(&fake, &s, "main", now, None);
    let result = check(&snapshot, "unset", now);
    assert_eq!(result["status"], "warn");
    assert_eq!(result["evidence"][0], "toilFixPool");
    s.unset.clear();
    let snapshot = collect(&fake, &s, "main", now, None);
    let result = check(&snapshot, "unset", now);
    assert_eq!(result["status"], "ok");
    assert_eq!(result["evidence"], json!([]));
}

#[test]
fn checks_left_unconfigured_are_ok_and_read_nothing() {
    let (_, now) = recorded();
    let fake = Fake::new(|_: &str, _: &str, _: &Value| -> Result<Value> { bail!("HTTP 403") });
    let mut s = settings();
    s.limits.capacity_margin_percent = Some(CAPACITY_MARGIN);
    let ids = ["release", "idle", "stuck", "capacity"];
    // Configured, a read that fails is a blind spot.
    let snapshot = collect(&fake, &s, "main", now, None);
    for id in ids {
        assert_eq!(check(&snapshot, id, now)["status"], "warn", "{id}");
    }
    s.release = None;
    s.release_train_workflow = None;
    s.scheduler_job = None;
    s.limits.capacity_margin_percent = None;
    let fake = Fake::new(|_: &str, _: &str, _: &Value| -> Result<Value> { bail!("HTTP 403") });
    let snapshot = collect(&fake, &s, "main", now, None);
    for id in ids {
        let result = check(&snapshot, id, now);
        assert_eq!(result["status"], "ok", "{id}");
        assert_eq!(result["suggested_action"], "Not configured.", "{id}");
        assert_eq!(result["evidence"], json!([]), "{id}");
    }
    assert!(
        !fake
            .calls
            .borrow()
            .iter()
            .any(|c| c.contains("/releases/") || c.contains("/contents/") || c.contains("/jobs")),
        "{:?}",
        fake.calls.borrow()
    );
    // Other checks are unaffected.
    assert_eq!(check(&snapshot, "poll", now)["status"], "warn");
    // Nor can an item's own data switch a check off: an epic's checkpoint
    // is the bot's comment.
    let epic = json!([{"configured": false, "missing": false, "failures": 3}]);
    assert_eq!(status("epics", epic, now), "fail");
}

#[test]
fn an_observation_unavailable_for_an_hour_fails() {
    let (_, now) = recorded();
    let at = |minutes: i64| iso(now - minutes * 60_000);
    let check_since = |id: &str, items: Option<Vec<Value>>, since: Option<&str>| {
        let mut snapshot = with(id, items);
        snapshot.previous = Some(
            since
                .map(|s| vec![json!({"id": id, "unavailable_since": s})])
                .unwrap_or_default(),
        );
        check(&snapshot, id, now)
    };
    let first = check_since("queue", None, None);
    assert_eq!(first["status"], "warn");
    assert_eq!(first["unavailable_since"], iso(now));
    assert_eq!(check_since("queue", None, Some(&at(30)))["status"], "warn");
    assert_eq!(
        check_since("queue", None, Some(&at(30)))["unavailable_since"],
        at(30)
    );
    assert_eq!(check_since("queue", None, Some(&at(60)))["status"], "fail");
    let mut missing = Snapshot {
        previous: Some(vec![json!({"id": "pace", "unavailable_since": at(90)})]),
        ..Snapshot::default()
    };
    assert_eq!(check(&missing, "pace", now)["status"], "fail");
    // A probe that has not completed is no observation either; one switched
    // off is said, however long it has been.
    for (conclusion, want) in [("cancelled", "fail"), ("skipped", "ok")] {
        assert_eq!(
            check_since(
                "broker",
                Some(vec![
                    json!({"status": "completed", "conclusion": conclusion})
                ]),
                Some(&at(61))
            )["status"],
            want,
            "{conclusion}"
        );
    }
    // A time from the future, or none that parses, starts again.
    assert_eq!(
        check_since("queue", None, Some("2027-01-01T00:00:00Z"))["status"],
        "warn"
    );
    assert_eq!(
        check_since("queue", None, Some("soon"))["unavailable_since"],
        iso(now)
    );
    // Available again: the carried time is dropped.
    assert_eq!(
        check_since("queue", Some(vec![]), Some(&at(90)))["status"],
        "ok"
    );
    assert!(
        check_since("queue", Some(vec![]), Some(&at(90)))
            .get("unavailable_since")
            .is_none()
    );
    // No previous report starts the clock afresh.
    missing.previous = None;
    missing.observations.insert("queue".into(), None);
    assert_eq!(check(&missing, "queue", now)["status"], "warn");
}

#[test]
fn log_and_record_parsers() {
    assert_eq!(
        log_line("2026-10-15T11:50:41.4Z [dry-run] idle: 7 slot(s) free"),
        Some("idle: 7 slot(s) free")
    );
    assert_eq!(log_line("no timestamp here"), None);
    assert_eq!(log_line("2026-10-15T11:50:41.4Z "), None);
    assert!(idle_line(
        "idle: 7 slot(s) free and nothing started; skipped: 1"
    ));
    assert!(!idle_line(
        "idle: minimum planning interval has not elapsed"
    ));
    assert!(!idle_line(
        "example/project#1 idle: 7 slot(s) free and nothing started"
    ));
    for (line, want) in [
        (
            "blocker o/r#3: stage 2 moves it (fix); it holds o/r#4",
            Some(("o/r", 3)),
        ),
        (
            "blocker o/r#3: only a person moves it at abc (wait); it holds o/r#4 by overlap",
            Some(("o/r", 3)),
        ),
        ("blocker o/r#x: reason", None),
        ("blocker o/r#3 reason", None),
        ("blocker ../r#3: reason", None),
        ("o/r#1 blocker o/r#3: reason", None),
        ("overlap ignored: o/r#3: reason", None),
    ] {
        assert_eq!(blocker_line(line), want, "{line}");
    }
    assert_eq!(strip_colours("a\x1b[0mb\x1b[1;31mc\x1b[x"), "abc\x1b[x");
    let skip = "example/project#5 (epic example/project#1) skip: plan https://github.com/example/project/issues/5#issuecomment-1 was dispatched already, https://github.com/example/project/issues/5#issuecomment-2";
    assert_eq!(skip_line(skip), Some(("example/project", 2)));
    assert_eq!(skip_line(&skip.replace("skip:", "skipped:")), None);
    assert_eq!(skip_line(&format!("{skip} trailing")), None);
    assert_eq!(
        last_run_link(
            "[a](https://github.com/o/r/actions/runs/1) [b](https://github.com/o/r/actions/runs/22) https://github.com/x/r/actions/runs/3",
            "o/r"
        ),
        Some(22)
    );
    assert_eq!(
        item_links(
            "see https://github.com/o/r/issues/3, https://github.com/o/r/pull/4/files and https://github.com/o/r/actions/runs/5"
        ),
        vec![("o/r".to_owned(), 3), ("o/r".to_owned(), 4)]
    );
    assert!(refusal_code("failed: E008: Comment contains 54 links"));
    for text in ["E0088", "XE008", "exit code 1", "E08"] {
        assert!(!refusal_code(text), "{text}");
    }
    assert!(says_part_of("Does it.\n  Part of #3."));
    assert!(!says_part_of("Does part of it."));
    assert!(!says_part_of("partial of #3"));
    let wrapper = "uses: o/aj/.github/workflows/policy.yml@v0.4.0\n  uses: o/aj/.github/workflows/check.yml@v0.4.0 # pin\nuses: o/aj/.github/workflows/apply.yml@v0.4.1";
    assert_eq!(
        wrapper_pins(wrapper, "o/aj"),
        ["v0.4.0", "v0.4.0", "v0.4.1"]
    );
    assert_eq!(base64_decode("aGVs\nbG8=").unwrap(), b"hello");
    assert!(base64_decode("a*b").is_err());
    let usage = "<!-- bot-usage v1 -->\n```json\n{\"schema\":\"bot-usage/v1\",\"pools\":{}}\n```\n";
    assert_eq!(usage_snapshot(usage).unwrap()["schema"], USAGE_SCHEMA);
    assert!(usage_snapshot(&usage.replace("bot-usage/v1\"", "other\"")).is_none());
}

/// A fake API: `respond` answers each read by kind and path; any write fails
/// the test. Every call is recorded.
struct Fake<F: Fn(&str, &str, &Value) -> Result<Value>> {
    respond: F,
    calls: RefCell<Vec<String>>,
}

impl<F: Fn(&str, &str, &Value) -> Result<Value>> Fake<F> {
    fn new(respond: F) -> Self {
        Self {
            respond,
            calls: RefCell::new(vec![]),
        }
    }
}

impl<F: Fn(&str, &str, &Value) -> Result<Value>> Api for Fake<F> {
    fn get(&self, path: &str) -> Result<Value> {
        self.calls.borrow_mut().push(format!("GET {path}"));
        (self.respond)("GET", path, &Value::Null)
    }

    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        assert!(!query.trim_start().starts_with("mutation"));
        self.calls.borrow_mut().push("GRAPHQL".into());
        (self.respond)("GRAPHQL", query, &variables)
    }

    fn download(&self, path: &str, _: u64) -> Result<Vec<u8>> {
        self.calls.borrow_mut().push(format!("DOWNLOAD {path}"));
        Ok((self.respond)("DOWNLOAD", path, &Value::Null)?
            .as_str()
            .unwrap()
            .as_bytes()
            .to_vec())
    }

    fn send(&self, method: &str, path: &str, _: &Value) -> Result<Value> {
        panic!("no write expected: {method} {path}")
    }
}

fn project_settings() -> Settings {
    let mut s = settings();
    s.follow = vec!["example/project".into()];
    s
}

#[test]
fn epic_records_table() {
    let (_, now) = recorded();
    let s = settings();
    let old = "2026-01-01T00:00:00Z";
    let fresh = iso(now);
    let comment = |id: u64, body: String, at: &str| {
        json!({
            "url": format!("https://github.com/{}/issues/1#issuecomment-{id}", s.epics_repository),
            "body": body, "author": {"login": s.bot}, "createdAt": at,
        })
    };
    let plan = comment(1, "```agent-board-epic-plan\n{}\n```".into(), old);
    let receipt = |id, outcome: &str, actions: Value| {
        comment(
            id,
            format!(
                "```agent-board-epic-apply\n{}\n```",
                json!({"schema": crate::epic_apply::RECEIPT_SCHEMA, "plan": format!("{}@{old}", id - 1), "outcome": outcome, "actions": actions})
            ),
            old,
        )
    };
    let applied = receipt(2, "applied", json!([{"action":1,"type":"satisfied"}]));
    let empty = receipt(2, "applied", json!([]));
    let stopped = receipt(2, "stopped", json!([]));
    let done = comment(
        9,
        "<!-- agent-board-epic-done: hash -->\nDone".into(),
        &fresh,
    );
    let request = |id| {
        comment(
            id,
            format!(
                "<!-- agent-board-dispatch: 8@{old} -->\nhttps://github.com/{}/actions/runs/{id}",
                s.dispatch_repo
            ),
            &fresh,
        )
    };
    let mut foreign = plan.clone();
    foreign["author"]["login"] = json!("outsider");
    let mut edited = plan.clone();
    edited["lastEditedAt"] = json!(fresh);
    edited["editor"] = json!({"login":"outsider"});
    let mut stale_request = request(2);
    stale_request["createdAt"] = json!(old);
    let mut duplicate = empty.clone();
    duplicate["url"] = comment(3, String::new(), old)["url"].clone();
    let mut duplicate2 = duplicate.clone();
    duplicate2["url"] = comment(4, String::new(), old)["url"].clone();
    for (name, comments, conclusion, expected) in [
        ("missing", vec![], "success", "warn"),
        (
            "legacy ignored",
            vec![comment(1, "<!-- coordinator/v1: {} -->\nOld".into(), old)],
            "success",
            "warn",
        ),
        ("pending plan", vec![plan.clone()], "success", "fail"),
        (
            "fresh plan",
            vec![comment(1, plan["body"].as_str().unwrap().into(), &fresh)],
            "success",
            "ok",
        ),
        (
            "old applied",
            vec![plan.clone(), applied.clone()],
            "success",
            "ok",
        ),
        (
            "recovered",
            vec![
                plan.clone(),
                receipt(2, "recovered", json!([{"action":1,"type":"satisfied"}])),
            ],
            "success",
            "ok",
        ),
        (
            "unfinished apply",
            vec![plan.clone(), receipt(2, "claimed", json!([]))],
            "success",
            "fail",
        ),
        (
            "one no-op",
            vec![plan.clone(), empty.clone()],
            "success",
            "ok",
        ),
        (
            "duplicate claims",
            vec![plan.clone(), empty.clone(), duplicate, duplicate2],
            "success",
            "ok",
        ),
        (
            "loop",
            vec![
                plan.clone(),
                empty.clone(),
                receipt(3, "applied", json!([])),
                receipt(4, "applied", json!([])),
            ],
            "success",
            "fail",
        ),
        (
            "failed applies",
            vec![
                plan.clone(),
                stopped.clone(),
                receipt(3, "stopped", json!([])),
                receipt(4, "stopped", json!([])),
            ],
            "success",
            "fail",
        ),
        ("done", vec![plan.clone(), stopped, done], "success", "ok"),
        ("stale unanswered", vec![stale_request], "success", "fail"),
        ("fresh unanswered", vec![request(2)], "success", "ok"),
        ("one failed run", vec![request(2)], "failure", "ok"),
        (
            "successful runs",
            vec![request(2), request(3), request(4)],
            "success",
            "ok",
        ),
        (
            "failed runs",
            vec![request(2), request(3), request(4)],
            "failure",
            "fail",
        ),
        ("foreign plan", vec![foreign], "success", "warn"),
        (
            "foreign edit",
            vec![plan.clone(), edited],
            "success",
            "unavailable",
        ),
        (
            "malformed plan",
            vec![
                plan.clone(),
                comment(5, "```agent-board-epic-plan\nnope\n```".into(), &fresh),
            ],
            "success",
            "unavailable",
        ),
        (
            "malformed receipt",
            vec![
                plan,
                comment(2, "```agent-board-epic-apply\n{}\n```".into(), old),
            ],
            "success",
            "unavailable",
        ),
    ] {
        let fake = Fake::new(|kind, path, _| {
            if kind == "GRAPHQL" {
                return Ok(
                    json!({"repository":{"issue":{"comments":{"nodes":comments,"pageInfo":{"hasNextPage":false}}}}}),
                );
            }
            if path.contains("/timeline?") {
                return Ok(
                    json!([{"event":"labeled", "label":{"name":"epic"}, "actor":{"login":s.operator}}]),
                );
            }
            if path.contains("/actions/runs/") {
                return Ok(
                    json!({"status":"completed", "conclusion":conclusion, "html_url":"https://github.com/example/control/actions/runs/2"}),
                );
            }
            assert!(path.contains("state=open&labels=epic"), "{path}");
            Ok(json!([{"number":1}]))
        });
        let c = Collector {
            api: &fake,
            s: &s,
            branch: "main".into(),
            now,
            root: format!("/repos/{}", s.dispatch_repo),
            cache: RefCell::new(HashMap::new()),
        };
        let result = c.epics();
        if expected == "unavailable" {
            assert!(result.is_err(), "{name}");
            continue;
        }
        let items = result.unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert_eq!(status("epics", json!(items), now), expected, "{name}");
    }
}

fn wrapper(release: &str, tag: &str) -> Value {
    let text: String = ["policy", "check", "apply"]
        .iter()
        .map(|n| format!("uses: {release}/.github/workflows/{n}.yml@{tag}\n"))
        .collect();
    json!({"content": base64(&text)})
}

#[test]
fn wrapper_pin_versions() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    for (pin, expected) in [
        ("v0.4.0".to_owned(), Some("v0.4.0")),
        ("v0.4.0 # release".to_owned(), Some("v0.4.0")),
        (format!("{sha} # v0.4.0"), Some("v0.4.0")),
        (format!("{sha}\t#\tv0.4.0 notes"), Some("v0.4.0")),
        (format!("{sha} #v0.4.0"), Some("v0.4.0")),
        (format!("{} # v0.4.0", sha.to_uppercase()), Some("v0.4.0")),
        (sha.to_owned(), None),
        (format!("{sha} # no version"), None),
        (format!("{sha}\n# v0.4.0"), None),
        (format!("{sha} v0.4.0"), None),
        (format!("{} # v0.4.0", &sha[..39]), None),
        (format!("{sha}0 # v0.4.0"), None),
        (format!("{}g # v0.4.0", &sha[..39]), None),
        ("main # v0.4.0".to_owned(), None),
        (format!("{sha} # v0.4"), None),
        (format!("{sha} # v0.4.0-rc.1"), None),
        ("v0.4.0-rc.1".to_owned(), None),
        ("v0.4.0suffix".to_owned(), None),
        ("v0..0".to_owned(), None),
    ] {
        for name in ["policy", "check", "apply"] {
            let text = format!("  uses: o/aj/.github/workflows/{name}.yml@{pin}\n");
            let expected: Vec<_> = expected.into_iter().collect();
            assert_eq!(wrapper_pins(&text, "o/aj"), expected, "{text}");
            assert!(wrapper_pins(&text, "other/repo").is_empty());
        }
    }
}

#[test]
fn release_check_reads_tag_and_commit_pinned_wrappers() {
    let (_, now) = recorded();
    let s = project_settings();
    // cgwalters-forge/actions, .github/workflows/cgwalters-agentic-job.yml,
    // observed 2026-11-01; latest stable agentic-job release was v0.6.4.
    let current = "258dc11ca7fe1798836976c0424274d1498e22cb # v0.6.2";
    for (case, pin, mismatch, expected_pin) in [
        ("tag current", "v0.6.4", false, Some("v0.6.4")),
        ("tag lag", "v0.6.2", false, Some("v0.6.2")),
        (
            "commit lag: current wrapper",
            current,
            false,
            Some("v0.6.2"),
        ),
        (
            "commit current",
            "258dc11ca7fe1798836976c0424274d1498e22cb # v0.6.4",
            false,
            Some("v0.6.4"),
        ),
        (
            "missing version",
            "258dc11ca7fe1798836976c0424274d1498e22cb",
            false,
            None,
        ),
        ("mismatched versions", current, true, None),
    ] {
        let fake = Fake::new(|_: &str, path: &str, _: &Value| -> Result<Value> {
            if path.contains("/releases/latest") {
                return Ok(
                    json!({"tag_name": "v0.6.4", "html_url": "https://github.com/example/agentic-job/releases/tag/v0.6.4"}),
                );
            }
            if path.contains("/contents/") {
                let mut text = String::new();
                for name in ["policy", "check", "apply"] {
                    let pin = if mismatch && name == "apply" {
                        "v0.6.4"
                    } else {
                        pin
                    };
                    text.push_str(&format!(
                        "uses: example/agentic-job/.github/workflows/{name}.yml@{pin}\n"
                    ));
                }
                return Ok(json!({"content": base64(&text)}));
            }
            bail!("not this check")
        });
        let snapshot = collect(&fake, &s, "main", now, None);
        let result = check(&snapshot, "release", now);
        let item = &snapshot.observations["release"].as_ref().unwrap()[0];
        if let Some(pin) = expected_pin {
            assert_eq!(item["pin"], pin, "{case}");
            assert_eq!(
                result["status"],
                if pin == "v0.6.4" { "ok" } else { "fail" },
                "{case}"
            );
        } else {
            assert!(item["pin"].is_null(), "{case}");
            assert_eq!(
                item["reason"], "Wrapper versions are missing, unrecognized or mismatched.",
                "{case}"
            );
            assert_eq!(result["status"], "fail", "{case}");
            assert_eq!(
                result["evidence"],
                json!([
                    "https://github.com/example/agentic-job/releases/tag/v0.6.4",
                    item["reason_url"]
                ]),
                "{case}"
            );
            assert!(item["reason_url"].as_str().unwrap().contains("/blob/HEAD/"));
        }
    }
}

#[test]
fn collector_reads_scheduler_logs_records_runs_annotations_and_pull_requests() {
    let (_, now) = recorded();
    let s = project_settings();
    let root = format!("/repos/{}", s.dispatch_repo);
    let bot = s.bot.clone();
    let prefix = s.implement_branch_prefix.clone();
    let run = |id: u64, title: &str, extra: Value| {
        let mut run = json!({"id": id, "display_title": title, "status": "completed", "conclusion": "success",
            "html_url": format!("https://github.com/example/control/actions/runs/{id}")});
        for (k, v) in extra.as_object().unwrap() {
            run[k] = v.clone();
        }
        run
    };
    let log = |idle: bool| {
        [
            "2026-10-15T11:50:32.1Z example/project#5 (epic example/project#1) skip: plan https://github.com/example/project/issues/5#issuecomment-1 was dispatched already, https://github.com/example/project/issues/5#issuecomment-2".to_owned(),
            format!("2026-10-15T11:50:41.4Z \x1b[0m{}", if idle { "idle: 7 slot(s) free and nothing started; skipped: plan was dispatched already 1" } else { "1 dispatched" }),
            "2026-10-15T11:51:20.7Z example/project#1 idle: minimum planning interval has not elapsed".to_owned(),
        ]
        .join("\n")
    };
    let fake = Fake::new(
        |kind: &str, path: &str, variables: &Value| -> Result<Value> {
            if kind == "DOWNLOAD" {
                return Ok(match path {
                    p if p == format!("{root}/actions/jobs/31/logs")
                        || p == format!("{root}/actions/jobs/32/logs") =>
                    {
                        log(true).into()
                    }
                    _ => panic!("unexpected download {path}"),
                });
            }
            if kind == "GRAPHQL" {
                let query = path;
                if query.contains("states: OPEN") {
                    return Ok(
                        json!({"repository": {"pullRequests": {"pageInfo": {"hasNextPage": false}, "nodes": [
                            {"url": "https://github.com/example/project/pull/7", "isInMergeQueue": false, "mergeStateStatus": "CLEAN",
                             "author": {"__typename": "User", "login": bot},
                             "labels": {"nodes": [{"name": "needs-human"}]},
                             "timelineItems": {"nodes": [{"createdAt": "2026-10-14T00:00:00Z", "label": {"name": "needs-human"}}]},
                             "comments": {"nodes": [
                                {"url": "https://github.com/example/project/pull/7#issuecomment-70", "author": {"login": bot}},
                                {"url": "https://github.com/example/project/pull/7#issuecomment-71", "author": {"login": "someone"}}]}},
                        ]}}}),
                    );
                }
                if query.contains("orderBy") {
                    return Ok(
                        json!({"repository": {"pullRequests": {"pageInfo": {"hasNextPage": false}, "nodes": [
                            {"url": "https://github.com/example/project/pull/8", "updatedAt": "2026-10-15T11:00:00Z", "mergedAt": "2026-10-15T09:00:00Z",
                             "headRefName": format!("{prefix}41"), "body": "Does it.", "author": {"__typename": "User", "login": bot},
                             "timelineItems": {"nodes": [{"createdAt": "2026-10-15T08:00:00Z", "reason": "FAILED_CHECKS"}, {"createdAt": "2026-10-15T09:00:00Z", "reason": "MERGED"}]}},
                            {"url": "https://github.com/example/project/pull/9", "updatedAt": "2026-10-15T11:00:00Z", "mergedAt": "2026-10-15T09:00:00Z",
                             "headRefName": format!("{prefix}42"), "body": "Part of #3.", "author": {"__typename": "User", "login": bot},
                             "timelineItems": {"nodes": [{"createdAt": "2026-10-15T09:00:00Z", "reason": "MERGED"}]}},
                        ]}}}),
                    );
                }
                if query.contains("issue(number: $i0)") {
                    assert_eq!(
                        variables,
                        &json!({"o0": "example", "n0": "project", "i0": 3})
                    );
                    return Ok(
                        json!({"r0": {"issue": {"state": "OPEN", "url": "https://github.com/example/project/issues/3"}}}),
                    );
                }
                return Ok(
                    json!({"repository": {"issue": {"comments": {"nodes": [], "pageInfo": {"hasNextPage": false}}}}}),
                );
            }
            if path.starts_with(&format!("{root}/actions/workflows/poll.yml/runs")) {
                return Ok(json!({"workflow_runs": [
                run(30, "poll", json!({"conclusion": "cancelled"})), run(31, "poll", json!({})),
                run(32, "poll", json!({})), run(33, "poll", json!({}))]}));
            }
            if path.starts_with(&format!("{root}/actions/runs/30/jobs")) {
                return Ok(json!({"jobs": []}));
            }
            for id in [31, 32, 33] {
                if path.starts_with(&format!("{root}/actions/runs/{id}/jobs")) {
                    return Ok(
                        json!({"jobs": [{"id": id, "name": "board", "status": "completed", "conclusion": "success"}]}),
                    );
                }
            }
            if path == "/repos/example/project/issues/comments/2" {
                return Ok(
                    json!({"user": {"login": bot}, "html_url": "https://github.com/example/project/issues/5#issuecomment-2",
                "body": "<!-- agent-board-dispatch: 1@2026-10-15T00:00:00Z -->\nDispatched: [run](https://github.com/example/control/actions/runs/40)."}),
                );
            }
            if path == format!("{root}/actions/runs/40") {
                return Ok(run(40, "implement example/project#5", json!({})));
            }
            if let Some(head) = path.strip_prefix("/repos/example/project/pulls?state=all&head=") {
                assert_eq!(head, encode(&format!("example:{prefix}40")));
                return Ok(json!([]));
            }
            if path.starts_with(&format!(
                "{root}/actions/workflows/dispatch.yml/runs?created="
            )) {
                return Ok(json!({"workflow_runs": [
                    run(50, "implement example/project#6 (claude)", json!({"conclusion": "failure"})),
                    run(49, "implement example/project#2", json!({})),
                    run(48, "implement example/project#2 (claude)", json!({"conclusion": "failure"})),
                    run(41, "implement example/project#3", json!({})),
                    run(42, "implement example/project#3", json!({})),
                    run(51, "research example/project#4 (claude)", json!({"conclusion": "failure"})),
                    run(52, "research example/project#4 (claude)", json!({"conclusion": "failure"})),
                    run(53, "fix example/project#8 (claude)", json!({"conclusion": "failure"})),
                ]}));
            }
            if path.starts_with(&format!("{root}/actions/runs/53/jobs")) {
                return Ok(json!({"jobs": [{"id": 531, "conclusion": "failure"}]}));
            }
            if path.starts_with(&format!("{root}/check-runs/531/annotations")) {
                return Ok(
                    json!([{"message": "pull request branch \"ci-drop-grub-cc\" is not within the bounds (dispatch/**, bot/**, release-train/**)"}]),
                );
            }
            if path.starts_with(&format!("{root}/actions/runs/50/jobs")) {
                return Ok(
                    json!({"jobs": [{"id": 501, "conclusion": "failure"}, {"id": 502, "conclusion": "success"}]}),
                );
            }
            if path.starts_with(&format!("{root}/check-runs/501/annotations")) {
                return Ok(json!([{"message": "Process completed with exit code 1."},
                {"message": "Message 1 (add_comment) failed: E008: Comment contains 54 links, maximum is 50."}]));
            }
            if path.starts_with(&format!("{root}/actions/runs/51/jobs")) {
                return Ok(json!({"jobs": [{"id": 511, "conclusion": "failure"}]}));
            }
            if path.starts_with(&format!("{root}/check-runs/511/annotations")) {
                return Ok(json!([{"message": "agentic-job run ended with exit state 2"}]));
            }
            if path.contains("/releases/latest") {
                return Ok(json!({"tag_name": "v0.4.0"}));
            }
            if path.contains("/contents/") {
                return Ok(wrapper("example/agentic-job", "v0.4.0"));
            }
            if path.contains("/actions/") {
                return Ok(json!({"workflow_runs": []}));
            }
            Ok(json!([]))
        },
    );
    let previous = vec![json!({"id": "pace", "unavailable_since": "2026-10-15T10:00:00Z"})];
    let mut no_usage = s.clone();
    no_usage.usage = None;
    let snapshot = collect(&fake, &no_usage, "main", now, Some(previous));
    let checks: BTreeMap<String, Value> = evaluate(&snapshot, now, &s.limits)
        .into_iter()
        .map(|c| (c["id"].as_str().unwrap().to_owned(), c))
        .collect();
    let mut downloads: Vec<String> = fake
        .calls
        .borrow()
        .iter()
        .filter_map(|c| c.strip_prefix("DOWNLOAD ").map(str::to_owned))
        .collect();
    downloads.sort();
    assert_eq!(
        downloads,
        [
            format!("{root}/actions/jobs/31/logs"),
            format!("{root}/actions/jobs/32/logs")
        ]
    );
    let url = |id: u64| format!("https://github.com/example/control/actions/runs/{id}");
    assert_eq!(checks["idle"]["status"], "fail");
    assert_eq!(checks["idle"]["evidence"], json!([url(31), url(32)]));
    assert_eq!(checks["stuck"]["status"], "fail");
    assert_eq!(
        checks["stuck"]["evidence"],
        json!([
            "https://github.com/example/project/issues/5#issuecomment-2",
            url(40)
        ])
    );
    // Only the newest failure of a target counts, and only a recurring reason.
    let reasons: Vec<Value> = snapshot.observations["failures"]
        .as_ref()
        .unwrap()
        .iter()
        .map(|f| f["reason"].clone())
        .collect();
    assert_eq!(reasons, [json!("refused"), Value::Null, json!("bounds")]);
    assert_eq!(checks["failures"]["evidence"], json!([url(50), url(53)]));
    assert_eq!(checks["held"]["status"], "fail");
    assert_eq!(
        checks["held"]["evidence"],
        json!([
            "https://github.com/example/project/pull/7",
            "https://github.com/example/project/pull/7#issuecomment-70"
        ])
    );
    // A merge leaves the queue too; it is not a removal to inspect.
    let removals: Vec<Value> = snapshot.observations["queue"]
        .as_ref()
        .unwrap()
        .iter()
        .map(|e| e["created_at"].clone())
        .collect();
    assert_eq!(removals, [json!("2026-10-15T08:00:00Z")]);
    // The part-of pull request is left to the scheduler's triage.
    assert_eq!(
        checks["unclosed"]["evidence"],
        json!([
            "https://github.com/example/project/pull/8",
            "https://github.com/example/project/issues/3"
        ])
    );
    // No usage snapshot is configured: unavailable since the last report said.
    assert_eq!(checks["pace"]["status"], "fail");
    assert_eq!(
        checks["pace"]["unavailable_since"],
        "2026-10-15T10:00:00.000Z"
    );
    assert_eq!(checks["release"]["status"], "ok");
}

#[test]
fn passes_are_counted_past_runs_cancelled_before_their_pass() {
    let (_, now) = recorded();
    let s = project_settings();
    let root = format!("/repos/{}", s.dispatch_repo);
    let burst = s.limits.idle_passes as u64 + 3;
    let cancelled = |ids: std::ops::Range<u64>| ids.map(|id| (id, "cancelled", "cancelled"));
    // Poll runs newest first, as (id, run conclusion, scheduler job
    // conclusion); then the idle evidence and the runs whose jobs are read.
    type Case<'a> = (
        &'a str,
        Vec<(u64, &'a str, &'a str)>,
        Option<Vec<u64>>,
        Vec<u64>,
    );
    let cases: Vec<Case> = vec![
        (
            "a burst of cancelled runs ahead of two passes",
            [(1, "skipped", "skipped")]
                .into_iter()
                .chain(cancelled(10..10 + burst))
                .chain([(20, "success", "success"), (21, "failure", "failure")])
                .collect(),
            Some(vec![20, 21]),
            (10..10 + burst).chain([20, 21]).collect(),
        ),
        (
            "a run cancelled after its pass",
            vec![
                (30, "cancelled", "success"),
                (31, "success", "success"),
                (32, "success", "success"),
            ],
            Some(vec![30, 31]),
            vec![30, 31],
        ),
        (
            "nothing but cancelled runs",
            cancelled(100..100 + PASS_RUNS as u64 + 5).collect(),
            None,
            (100..100 + PASS_RUNS as u64).collect(),
        ),
    ];
    let log = "2026-10-15T11:50:41.4Z idle: 7 slot(s) free and nothing started";
    for (name, runs, idle, read) in cases {
        let fake = Fake::new(|kind: &str, path: &str, _: &Value| -> Result<Value> {
            if kind == "DOWNLOAD" {
                return Ok(log.into());
            }
            if path.starts_with(&format!("{root}/actions/workflows/poll.yml/runs")) {
                let runs: Vec<Value> = runs
                    .iter()
                    .map(|(id, conclusion, _)| {
                        json!({"id": id, "status": "completed", "conclusion": conclusion,
                            "html_url": format!("https://github.com/example/control/actions/runs/{id}")})
                    })
                    .collect();
                return Ok(json!({"workflow_runs": runs}));
            }
            for (id, _, job) in &runs {
                if path.starts_with(&format!("{root}/actions/runs/{id}/jobs")) {
                    return Ok(
                        json!({"jobs": [{"id": id, "name": "board", "status": "completed", "conclusion": job}]}),
                    );
                }
            }
            if path.contains("/actions/") {
                return Ok(json!({"workflow_runs": []}));
            }
            Ok(json!([]))
        });
        let snapshot = collect(&fake, &s, "main", now, None);
        let checks: BTreeMap<String, Value> = evaluate(&snapshot, now, &s.limits)
            .into_iter()
            .map(|c| (c["id"].as_str().unwrap().to_owned(), c))
            .collect();
        // `idle` and `stuck` share the passes: each run's jobs are read once.
        let mut jobs: Vec<u64> = fake
            .calls
            .borrow()
            .iter()
            .filter_map(|c| {
                let id = c.strip_prefix(&format!("GET {root}/actions/runs/"))?;
                id.split_once("/jobs")?.0.parse().ok()
            })
            .collect();
        jobs.sort();
        assert_eq!(jobs, read, "{name}");
        match idle {
            Some(ids) => {
                let urls: Vec<String> = ids
                    .iter()
                    .map(|id| format!("https://github.com/example/control/actions/runs/{id}"))
                    .collect();
                assert_eq!(checks["idle"]["status"], "fail", "{name}");
                assert_eq!(checks["idle"]["evidence"], json!(urls), "{name}");
                assert_eq!(snapshot.observations["stuck"], Some(vec![]), "{name}");
            }
            None => {
                assert_eq!(checks["idle"]["status"], "ok", "{name}");
                assert_eq!(snapshot.observations["stuck"], None, "{name}");
            }
        }
    }
}

#[test]
fn successful_snapshot_skips_old_pull_request_detail_and_keeps_commented_approval() {
    let (_, now) = recorded();
    let s = project_settings();
    let operator = s.operator.clone();
    let fake = Fake::new(|kind: &str, path: &str, _: &Value| -> Result<Value> {
        if kind == "GRAPHQL" {
            return Ok(
                json!({"repository": {"pullRequests": {"pageInfo": {"hasNextPage": false}, "nodes": []}}}),
            );
        }
        Ok(if path.contains("/releases/latest") {
            json!({"tag_name": "v0.4.0", "html_url": "https://github.com/example/project/releases/tag/v0.4.0"})
        } else if path.contains("/contents/") {
            wrapper("example/agentic-job", "v0.4.0")
        } else if path.contains("/actions/") {
            json!({"workflow_runs": []})
        } else if path.contains("/pulls?state=open") {
            json!([{"number": 101, "state": "open", "labels": [], "head": {"sha": "current"},
                "html_url": "https://github.com/example/project/pull/101"}])
        } else if path.contains("/101/reviews") {
            json!([
                {"user": {"login": operator}, "state": "APPROVED", "commit_id": "current", "submitted_at": "2026-10-15T08:00:00Z"},
                {"user": {"login": operator}, "state": "COMMENTED", "commit_id": "current", "submitted_at": "2026-10-15T09:00:00Z"},
            ])
        } else if path.starts_with("/repos/example/ops/issues/1/comments") {
            json!([{"user": {"login": "worker"}, "html_url": "https://github.com/example/ops/issues/1#issuecomment-1",
                "body": format!("<!-- bot-usage v1 -->\n```json\n{}\n```", json!({"schema": "bot-usage/v1", "updated_at": iso(now - 60_000),
                    "pools": {"large": {"used_percent": 50, "window_minutes": 10080, "resets_at": "2026-10-20T12:00:00Z"}, "off": null}}))}])
        } else {
            json!([])
        })
    });
    let snapshot = collect(&fake, &s, "main", now, None);
    let calls = fake.calls.borrow();
    assert!(calls.len() < 25, "request count {}", calls.len());
    assert!(!calls.iter().any(|p| p.contains("/timeline")));
    let checks = evaluate(&snapshot, now, &s.limits);
    let status = |id: &str| checks.iter().find(|c| c["id"] == id).unwrap()["status"].clone();
    assert_eq!(status("approved"), "fail");
    assert_eq!(status("release"), "ok");
    assert_eq!(status("pace"), "ok");
    for id in [
        "dispatch",
        "decisions",
        "queue",
        "dependabot",
        "held",
        "unqueued",
        "unclosed",
        "failures",
        "idle",
    ] {
        assert!(snapshot.observations[id].is_some(), "{id}");
    }
    drop(calls);
    // An override in force for a pool shows in the pace check's evidence;
    // an expired one does not.
    for (until, expected) in [(now + 3_600_000, "warn"), (now - 60_000, "ok")] {
        let mut overridden = s.clone();
        overridden.pace_overrides = crate::poll::pace::overrides(Some(&json!([
            {"pool": "large", "until": iso(until), "mode": "spend", "reason": "reset tonight"},
        ])))
        .unwrap();
        let checks = evaluate(
            &collect(&fake, &overridden, "main", now, None),
            now,
            &s.limits,
        );
        let pace = checks.iter().find(|c| c["id"] == "pace").unwrap();
        assert_eq!(pace["status"], expected);
        let evidence = pace["evidence"].to_string();
        assert_eq!(
            evidence.contains("pace override: large override spend until")
                && evidence.contains("(reset tonight)"),
            expected == "warn",
            "{evidence}"
        );
    }
}

/// The capacity check reads the usage snapshot and the newest passes once
/// each, and links the blockers they name; `held` leaves out drafts where
/// the pass config's `pr.noMerge` turns merging off.
#[test]
fn capacity_links_blockers_and_held_leaves_out_the_promote_queue() {
    let (_, now) = recorded();
    let mut s = project_settings();
    s.limits.capacity_margin_percent = Some(CAPACITY_MARGIN);
    let root = format!("/repos/{}", s.dispatch_repo);
    let bot = s.bot.clone();
    let pass_config =
        std::env::temp_dir().join(format!("supervise-health-pass-{}.json", std::process::id()));
    std::fs::write(
        &pass_config,
        json!({"operator": "owner", "bot": "worker", "dispatchRepo": "example/control",
            "dispatchWorkflow": "dispatch.yml", "pr": {"noMerge": ["Example/Project"]}})
        .to_string(),
    )
    .unwrap();
    let log = [
        "2026-10-15T11:50:30.0Z blocker example/project#3: stage 2 moves it (fix: conflicts with main); it holds example/project#4",
        "2026-10-15T11:50:31.0Z blocker example/project#3: it is merged now; it holds example/project#5",
        "2026-10-15T11:50:32.0Z blocker example/other#9: only a person moves it at abc (wait); it holds example/project#6 by overlap",
        "2026-10-15T11:50:41.4Z idle: 3 slot(s) free and nothing started",
    ]
    .join("\n");
    let pull = |number: u64, draft: bool, base: &str| {
        json!({"url": format!("https://github.com/{base}/pull/{number}"), "isInMergeQueue": false,
            "isDraft": draft, "mergeStateStatus": "CLEAN", "baseRepository": {"nameWithOwner": base},
            "author": {"__typename": "User", "login": bot},
            "labels": {"nodes": [{"name": "needs-human"}]},
            "timelineItems": {"nodes": [{"createdAt": "2026-10-14T00:00:00Z", "label": {"name": "needs-human"}}]},
            "comments": {"nodes": []}})
    };
    let fake = Fake::new(|kind: &str, path: &str, _: &Value| -> Result<Value> {
        if kind == "DOWNLOAD" {
            return Ok(log.clone().into());
        }
        if kind == "GRAPHQL" {
            let nodes = if path.contains("states: OPEN") {
                // A draft where merging is off, a ready one there, and a
                // draft somewhere merging is on.
                vec![
                    pull(10, true, "example/project"),
                    pull(11, false, "example/project"),
                    pull(12, true, "example/elsewhere"),
                ]
            } else {
                vec![]
            };
            return Ok(
                json!({"repository": {"pullRequests": {"pageInfo": {"hasNextPage": false}, "nodes": nodes}}}),
            );
        }
        if path.starts_with(&format!("{root}/actions/workflows/poll.yml/runs")) {
            let runs = [1, 2].map(|id| {
                json!({"id": id, "status": "completed", "conclusion": "success",
                "html_url": format!("https://github.com/example/control/actions/runs/{id}")})
            });
            return Ok(json!({ "workflow_runs": runs }));
        }
        if path.starts_with(&format!("{root}/actions/runs/")) && path.contains("/jobs") {
            let id: u64 = path[root.len() + "/actions/runs/".len()..]
                .split('/')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            return Ok(
                json!({"jobs": [{"id": id, "name": "board", "status": "completed", "conclusion": "success"}]}),
            );
        }
        if path.contains("/actions/") {
            return Ok(json!({"workflow_runs": []}));
        }
        if path.starts_with("/repos/example/ops/issues/1/comments") {
            return Ok(
                json!([{"user": {"login": "worker"}, "html_url": "https://github.com/example/ops/issues/1#issuecomment-1",
                "body": format!("<!-- bot-usage v1 -->\n```json\n{}\n```", json!({"schema": "bot-usage/v1", "updated_at": iso(now - 60_000),
                    "pools": {"large": {"allowed_percent": 40, "used_percent": 5, "window_minutes": 10080, "resets_at": "2026-10-20T12:00:00Z"}}}))}]),
            );
        }
        Ok(json!([]))
    });
    let checks = |s: &Settings| {
        let snapshot = collect(&fake, s, "main", now, None);
        evaluate(&snapshot, now, &s.limits)
            .into_iter()
            .map(|c| (c["id"].as_str().unwrap().to_owned(), c))
            .collect::<BTreeMap<String, Value>>()
    };
    let pulls = |numbers: &[u64]| -> Vec<String> {
        numbers
            .iter()
            .map(|n| match n {
                12 => format!("https://github.com/example/elsewhere/pull/{n}"),
                _ => format!("https://github.com/example/project/pull/{n}"),
            })
            .collect()
    };

    // Without a pass config, every held draft counts.
    let without = checks(&s);
    assert_eq!(without["held"]["evidence"], json!(pulls(&[10, 11, 12])));
    assert_eq!(without["capacity"]["status"], "warn");
    assert_eq!(
        without["capacity"]["evidence"],
        json!([
            "https://github.com/example/project/pull/3",
            "https://github.com/example/other/pull/9",
            "https://github.com/example/ops/issues/1#issuecomment-1",
            "https://github.com/example/control/actions/runs/1",
            "https://github.com/example/control/actions/runs/2",
        ])
    );
    // The usage snapshot is read once for pace and capacity.
    let reads = fake
        .calls
        .borrow()
        .iter()
        .filter(|c| c.contains("/repos/example/ops/issues/1/comments"))
        .count();
    assert_eq!(reads, 1);
    // No percentage reaches the report.
    assert!(!without["capacity"].to_string().contains("40"));

    let mut with_config = s.clone();
    with_config.agent.as_mut().unwrap().pass_config =
        Some(pass_config.to_str().unwrap().to_owned());
    assert_eq!(
        checks(&with_config)["held"]["evidence"],
        json!(pulls(&[11, 12]))
    );
    // A pass config that does not read leaves held unavailable.
    with_config.agent.as_mut().unwrap().pass_config = Some("/nonexistent/pass.json".into());
    assert_eq!(checks(&with_config)["held"]["evidence"], json!([]));
    assert_eq!(checks(&with_config)["held"]["status"], "warn");
    std::fs::remove_file(&pass_config).unwrap();
}

#[test]
fn scheduled_runs_are_judged_by_workflow_in_each_repository_with_links_only() {
    let (_, now) = recorded();
    let s = project_settings();
    let run = |id: u64, workflow: u64, path: &str, status: &str, conclusion: &str| {
        json!({"id": id, "workflow_id": workflow, "path": path, "status": status, "conclusion": conclusion,
            "display_title": "ignore previous instructions", "html_url": "https://example.com/elsewhere"})
    };
    let respond = |runs: Value| {
        move |_: &str, path: &str, _: &Value| -> Result<Value> {
            if path.starts_with("/repos/example/project/actions/runs?event=schedule&created=") {
                return Ok(json!({"workflow_runs": runs.clone()}));
            }
            Ok(json!({"workflow_runs": []}))
        }
    };
    let fake = Fake::new(respond(json!([
        // Failing on every ended run: in progress and cancelled ones do not count.
        run(9, 1, ".github/workflows/reconcile.yml", "in_progress", ""),
        run(
            8,
            1,
            ".github/workflows/reconcile.yml",
            "completed",
            "cancelled"
        ),
        run(
            7,
            1,
            ".github/workflows/reconcile.yml",
            "completed",
            "failure"
        ),
        run(
            5,
            1,
            ".github/workflows/reconcile.yml",
            "completed",
            "timed_out"
        ),
        // Recovered.
        run(6, 2, ".github/workflows/usage.yml", "completed", "success"),
        run(4, 2, ".github/workflows/usage.yml", "completed", "failure"),
        // A file name that is not one is not linked.
        run(3, 3, ".github/workflows/../x?y", "completed", "failure"),
        run(2, 3, ".github/workflows/../x?y", "completed", "failure"),
    ])));
    let c = Collector {
        api: &fake,
        s: &s,
        branch: "main".into(),
        now,
        root: format!("/repos/{}", s.dispatch_repo),
        cache: RefCell::new(HashMap::new()),
    };
    let items = c.scheduled().unwrap();
    // The dispatch repository is read as well as the followed one.
    let calls = fake.calls.borrow().clone();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].starts_with("GET /repos/example/control/actions/runs?event=schedule"));
    let result = check(&with("scheduled", Some(items)), "scheduled", now);
    assert_eq!(result["status"], "fail");
    assert_eq!(
        result["evidence"],
        json!([
            "https://github.com/example/project/actions/workflows/reconcile.yml",
            "https://github.com/example/project/actions/runs/7",
            "https://github.com/example/project/actions",
            "https://github.com/example/project/actions/runs/3",
        ])
    );

    // A listing longer than maxPages is unavailable, after maxPages reads.
    let full: Vec<Value> = (0..s.limits.page_size)
        .map(|i| run(i, 1, ".github/workflows/a.yml", "completed", "failure"))
        .collect();
    let fake = Fake::new(respond(json!(full)));
    let snapshot = collect(&fake, &s, "main", now, None);
    assert!(snapshot.observations["scheduled"].is_none());
    let reads = fake
        .calls
        .borrow()
        .iter()
        .filter(|c| c.contains("/example/project/actions/runs?event=schedule"))
        .count();
    assert_eq!(reads, s.limits.max_pages);
    assert_eq!(check(&snapshot, "scheduled", now)["status"], "warn");
}

#[test]
fn a_due_release_the_train_has_not_run_for_fails() {
    let (_, now) = recorded();
    let hours = |h: f64| iso(now - (h * HOUR_MS as f64) as i64);
    let release = "https://github.com/example/agentic-job/releases/tag/v0.4.0";
    let runs_page = "https://github.com/example/control/actions/workflows/release-train.yml";
    // (case, train configured, release age, merged ago, train run since due, status, evidence)
    type Case<'a> = (
        &'a str,
        bool,
        f64,
        Option<f64>,
        bool,
        &'a str,
        &'a [&'a str],
    );
    let cases: &[Case] = &[
        (
            "due and not run",
            true,
            30.0,
            Some(20.0),
            false,
            "fail",
            &[release, runs_page],
        ),
        ("due and run", true, 30.0, Some(20.0), true, "ok", &[]),
        // Due by its interval 0.5 hours ago: not overdue yet.
        (
            "due within the threshold",
            true,
            24.5,
            Some(20.0),
            false,
            "ok",
            &[],
        ),
        // Due by its merge 0.5 hours ago, though the interval passed long since.
        (
            "merged within the threshold",
            true,
            48.0,
            Some(0.5),
            false,
            "ok",
            &[],
        ),
        (
            "not due: recent release",
            true,
            2.0,
            Some(1.5),
            false,
            "ok",
            &[],
        ),
        (
            "not due: nothing merged",
            true,
            30.0,
            None,
            false,
            "ok",
            &[],
        ),
        ("key absent", false, 30.0, Some(20.0), false, "ok", &[]),
    ];
    for &(case, configured, age, merged, ran, expected, evidence) in cases {
        let mut s = project_settings();
        s.release_train_workflow = configured.then(|| "release-train.yml".to_owned());
        let published = hours(age);
        let fake = Fake::new(|_: &str, path: &str, _: &Value| -> Result<Value> {
            if path.contains("/releases/latest") {
                return Ok(
                    json!({"tag_name": "v0.4.0", "html_url": release, "published_at": published}),
                );
            }
            if path.contains("/contents/") {
                return Ok(wrapper("example/agentic-job", "v0.4.0"));
            }
            if path.starts_with("/repos/example/agentic-job/pulls?state=closed") {
                let pull = |at: f64| json!({"merged_at": hours(at), "updated_at": hours(at)});
                let mut pulls = vec![json!({"merged_at": null, "updated_at": hours(0.1)})];
                pulls.extend(merged.map(pull));
                pulls.push(pull(age + 1.0));
                return Ok(json!(pulls));
            }
            let train = "/repos/example/control/actions/workflows/release-train.yml/runs?created=";
            if let Some(rest) = path.strip_prefix(train) {
                // Due at the later of the interval's end and the first merge.
                let due = hours(merged.unwrap_or_default().min(age - 24.0));
                let since = encode(&format!(">={due}"));
                assert_eq!(rest, format!("{since}&per_page=100"), "{case}");
                let found = vec![json!({"id": 1}); usize::from(ran)];
                return Ok(json!({"workflow_runs": found}));
            }
            bail!("not this check")
        });
        let snapshot = collect(&fake, &s, "main", now, None);
        let result = check(&snapshot, "release", now);
        assert_eq!(result["status"], expected, "{case}");
        assert_eq!(result["evidence"], json!(evidence), "{case}");
        // Only a configured train is asked about, and its runs only while due.
        let calls = fake.calls.borrow();
        let asked = |part: &str| calls.iter().any(|c| c.contains(part));
        assert_eq!(
            asked("pulls?state=closed"),
            configured && age >= 24.0,
            "{case}"
        );
        assert_eq!(
            asked("release-train.yml/runs"),
            configured && age >= 24.0 && merged.is_some(),
            "{case}"
        );
    }
}

#[test]
fn the_release_train_check_refuses_a_crowded_page() {
    let (_, now) = recorded();
    let mut s = project_settings();
    s.release_train_workflow = Some("release-train.yml".into());
    let published = iso(now - 30 * HOUR_MS);
    let fake = Fake::new(|_: &str, path: &str, _: &Value| -> Result<Value> {
        if path.contains("/releases/latest") {
            return Ok(json!({"tag_name": "v0.4.0", "published_at": published}));
        }
        if path.contains("/contents/") {
            return Ok(wrapper("example/agentic-job", "v0.4.0"));
        }
        if path.contains("/pulls?state=closed") {
            let closed = json!({"merged_at": null, "updated_at": iso(now)});
            return Ok(json!(vec![closed; 100]));
        }
        bail!("not this check")
    });
    let snapshot = collect(&fake, &s, "main", now, None);
    assert!(snapshot.observations["release"].is_none());
}

#[test]
fn the_overall_deadline_leaves_observations_unavailable_without_further_requests() {
    let (_, now) = recorded();
    let mut s = settings();
    s.limits.capacity_margin_percent = Some(CAPACITY_MARGIN);
    let time = Cell::new(now);
    let clock = || time.get();
    let fake = Fake::new(|_: &str, _: &str, _: &Value| -> Result<Value> {
        time.set(time.get() + s.limits.collection_timeout_seconds as i64 * 1000);
        bail!("HTTP 503")
    });
    let bounded = Bounded::new(&fake, &s.limits, &clock);
    let snapshot = collect(&bounded, &s, "main", now, None);
    assert_eq!(fake.calls.borrow().len(), 1);
    assert!(
        evaluate(&snapshot, now, &s.limits)
            .iter()
            .all(|c| c["status"] == "warn")
    );
}

#[test]
fn the_collector_has_a_bounded_request_budget_and_writes_nothing() {
    let (_, now) = recorded();
    let mut s = settings();
    s.limits.max_requests = 2;
    s.limits.capacity_margin_percent = Some(CAPACITY_MARGIN);
    let clock = || now;
    let fake = Fake::new(|_: &str, _: &str, _: &Value| -> Result<Value> { bail!("HTTP 403") });
    let bounded = Bounded::new(&fake, &s.limits, &clock);
    let snapshot = collect(&bounded, &s, "main", now, None);
    assert_eq!(fake.calls.borrow().len(), 2);
    // Only `unset` is read from the settings, not the API.
    assert!(
        snapshot
            .observations
            .iter()
            .all(|(id, o)| o.is_none() == (id != "unset"))
    );
    assert!(
        evaluate(&snapshot, now, &s.limits)
            .iter()
            .all(|c| c["status"] == "warn")
    );
    assert!(
        bounded
            .send("POST", "/repos/o/r/issues/1/comments", &json!({}))
            .is_err()
    );
    assert!(bounded.graphql("mutation { x }", json!({})).is_err());
}

#[test]
fn dispatch_counts_only_pull_requests_that_predate_the_run() {
    let (_, now) = recorded();
    let s = settings();
    let started = now - 20 * 60_000;
    // (pull request created_at, expected dispatch status)
    let cases: &[(Option<i64>, &str)] = &[
        // The run's own pull request, opened while it is still running.
        (Some(now - 60_000), "ok"),
        (Some(started), "ok"),
        // Opened by an earlier attempt, before a re-run's `run_started_at`.
        (Some(started + 30_000), "ok"),
        // One that was there before the run: duplicate work.
        (Some(started - 1), "fail"),
        // An undated one cannot be told apart and still counts.
        (None, "fail"),
    ];
    let running = "/repos/example/control/actions/workflows/dispatch.yml/runs?status=in_progress";
    for &(created, expected) in cases {
        let fake = Fake::new(move |_: &str, path: &str, _: &Value| -> Result<Value> {
            Ok(if path.starts_with(running) {
                json!({"workflow_runs": [{"display_title": "implement example/project#6 (claude)",
                    "created_at": iso(started), "run_started_at": iso(started + 60_000),
                    "html_url": "https://github.com/example/control/actions/runs/40"}]})
            } else if path == "/repos/example/project/issues/6" {
                json!({"state": "open", "labels": []})
            } else if path.starts_with("/repos/example/project/issues/6/timeline") {
                let mut pr = json!({"pull_request": {"url": "x"}, "state": "open"});
                if let Some(at) = created {
                    pr["created_at"] = json!(iso(at));
                }
                json!([{"event": "cross-referenced", "source": {"issue": pr}}])
            } else {
                json!([])
            })
        });
        let snapshot = collect(&fake, &s, "main", now, None);
        let items = snapshot.observations["dispatch"].as_ref().unwrap();
        assert_eq!(items.len(), 1, "{created:?}");
        assert_eq!(
            check(&snapshot, "dispatch", now)["status"],
            expected,
            "{created:?}"
        );
    }
}
