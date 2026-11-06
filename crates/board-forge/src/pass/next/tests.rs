use super::*;
use std::cell::{Cell, RefCell};

const REPO: &str = "example/board";
const ME: u64 = 100;
const T0: &str = "2026-10-29T23:20:00Z";
const LATER: &str = "2026-10-29T23:25:00Z";
const EARLIER: &str = "2026-10-29T23:15:00Z";
const APPLY_TITLE: &str = "pass (apply)";

fn at(text: &str) -> OffsetDateTime {
    OffsetDateTime::parse(text, &Rfc3339).unwrap()
}

/// A run as GitHub's run listing answers it: a queued scheduled pass on
/// main unless `fields` say otherwise.
fn run(id: u64, created: &str, fields: Value) -> Value {
    let mut run = json!({
        "id": id, "created_at": created, "status": "queued", "event": "schedule",
        "display_title": "pass", "head_branch": "main",
    });
    for (key, value) in fields.as_object().unwrap() {
        run[key] = value.clone();
    }
    run
}

fn me() -> Value {
    run(
        ME,
        T0,
        json!({"status": "in_progress", "event": "workflow_dispatch", "display_title": APPLY_TITLE}),
    )
}

fn applying(status: &str) -> Value {
    json!({"event": "workflow_dispatch", "display_title": APPLY_TITLE, "status": status})
}

fn settings() -> Settings {
    let workflow = |file: &str, inputs: &[(&str, &str)]| Workflow {
        file: file.into(),
        inputs: inputs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    };
    Settings {
        repository: REPO.into(),
        run: ME,
        branch: "main".into(),
        pass: workflow("pass.yml", &[("apply", "true")]),
        apply_title: APPLY_TITLE.into(),
        interval: Duration::from_secs(600),
        supervisor: Some(Supervisor {
            workflow: workflow("supervisor.yml", &[("prompt", ""), ("session", "false")]),
            stale: Duration::from_secs(30 * 60),
        }),
        train: None,
        dry_run: false,
    }
}

#[test]
fn wait_arithmetic() {
    let now = at(T0);
    let minutes = |m: i64| time::Duration::minutes(m);
    for (started, interval, want) in [
        (now - minutes(1), 600, 540),
        (now, 600, 600),
        (now - minutes(10), 600, 0),
        (now - minutes(25), 600, 0),
        // A start the runner's clock puts in the future waits no longer.
        (now + minutes(2), 600, 600),
        (now - minutes(1), 300, 240),
        (now, 0, 0),
    ] {
        let interval = Duration::from_secs(interval);
        let got = wait_for(started, now, interval);
        assert_eq!(got, Duration::from_secs(want), "{started} {interval:?}");
    }
}

#[test]
fn supervisor_staleness() {
    let now = at(T0);
    let ago = |m: i64| (now - time::Duration::minutes(m)).format(&Rfc3339).unwrap();
    let comment = |id, m| run(id, &ago(m), json!({"event": "issue_comment"}));
    let event = |id, m, event: &str| run(id, &ago(m), json!({ "event": event }));
    for (name, runs, due) in [
        ("no runs", vec![], true),
        (
            "only issue_comment runs",
            vec![comment(3, 1), comment(2, 5)],
            true,
        ),
        ("fresh schedule run", vec![event(1, 10, "schedule")], false),
        (
            "comment hides a stale run",
            vec![comment(2, 1), event(1, 45, "schedule")],
            true,
        ),
        (
            "comment before a fresh dispatch",
            vec![comment(2, 1), event(1, 29, "workflow_dispatch")],
            false,
        ),
        (
            "exactly the threshold",
            vec![event(1, 30, "schedule")],
            true,
        ),
        (
            "past the threshold",
            vec![event(1, 31, "workflow_dispatch")],
            true,
        ),
    ] {
        let newest = newest_supervisor_run(&runs).unwrap();
        let stale = Duration::from_secs(30 * 60);
        assert_eq!(supervisor_due(newest, now, stale), due, "{name}");
    }
    let unreadable = [json!({"id": 1, "event": "schedule", "created_at": "yesterday"})];
    assert!(newest_supervisor_run(&unreadable).is_err());
}

#[test]
fn newer_run_selection() {
    let mut statuses: Vec<(String, Vec<Value>, Option<u64>)> =
        ["queued", "waiting", "pending", "requested", "in_progress"]
            .iter()
            .map(|s| {
                let runs = vec![run(105, LATER, applying(s)), me()];
                (format!("newer {s} run"), runs, Some(105))
            })
            .collect();
    let other = |fields| run(105, LATER, fields);
    let cases = [
        ("lone chain", vec![me()], None),
        // Two chains a few minutes apart (tracker#1019): the older stops.
        (
            "offset duplicate chain",
            vec![other(applying("in_progress")), me()],
            Some(105),
        ),
        (
            "newer schedule run",
            vec![other(json!({})), me()],
            Some(105),
        ),
        (
            "older queued run",
            vec![me(), run(90, EARLIER, applying("queued"))],
            None,
        ),
        (
            "newer completed run",
            vec![other(applying("completed")), me()],
            None,
        ),
        (
            "same second, higher id",
            vec![run(101, T0, applying("queued")), me()],
            Some(101),
        ),
        (
            "same second, lower id",
            vec![me(), run(99, T0, applying("queued"))],
            None,
        ),
        (
            "newer preview",
            vec![other(json!({"event": "workflow_dispatch"})), me()],
            None,
        ),
        (
            "newer run on another branch",
            vec![other(json!({"head_branch": "other"})), me()],
            None,
        ),
        (
            "newer push run",
            vec![other(json!({"event": "push"})), me()],
            None,
        ),
    ];
    statuses.extend(cases.map(|(n, r, w)| (n.to_string(), r, w)));
    for (name, runs, want) in statuses {
        assert_eq!(newer_run(&runs, &settings()).unwrap(), want, "{name}");
    }
    let beyond = [run(105, LATER, json!({"status": "completed"}))];
    let error = newer_run(&beyond, &settings()).unwrap_err();
    assert!(
        format!("{error:#}").contains("run 100 is not among"),
        "{error:#}"
    );
}

/// GitHub and the clock together, logging each call and sleep in order.
struct Fake {
    now: Cell<OffsetDateTime>,
    started: String,
    /// The pass workflow's runs, newest first, served a page at a time.
    runs: Vec<Value>,
    /// The runs listed once a dispatch was tried, as if it started one.
    successor: Option<Vec<Value>>,
    supervisor_runs: Vec<Value>,
    train_runs: Vec<Value>,
    /// How many calls of each kind fail before the rest succeed.
    fail: BTreeMap<&'static str, usize>,
    calls: RefCell<BTreeMap<&'static str, usize>>,
    log: RefCell<Vec<String>>,
    bodies: RefCell<Vec<(String, Value)>>,
}

impl Fake {
    fn new(runs: Vec<Value>) -> Self {
        Self {
            now: Cell::new(at(T0) + time::Duration::minutes(1)),
            started: T0.into(),
            runs,
            successor: None,
            // Due, so every chain case wakes it too.
            supervisor_runs: Vec::new(),
            train_runs: Vec::new(),
            fail: BTreeMap::new(),
            calls: RefCell::default(),
            log: RefCell::default(),
            bodies: RefCell::default(),
        }
    }

    /// Log the call; whether it fails.
    fn call(&self, kind: &'static str) -> bool {
        self.log.borrow_mut().push(kind.into());
        let mut calls = self.calls.borrow_mut();
        let n = calls.entry(kind).or_default();
        *n += 1;
        *n <= self.fail.get(kind).copied().unwrap_or_default()
    }

    fn count(&self, kind: &str) -> usize {
        self.log.borrow().iter().filter(|k| *k == kind).count()
    }

    fn page(runs: &[Value], url: &str) -> Value {
        let page: usize = url.rsplit_once("&page=").unwrap().1.parse().unwrap();
        let runs: Vec<_> = runs
            .iter()
            .skip((page - 1) * RUN_PAGE)
            .take(RUN_PAGE)
            .collect();
        json!({ "workflow_runs": runs })
    }
}

impl Rest for Fake {
    fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<crate::poll::RestResponse> {
        panic!("unexpected conditional GET {url}")
    }

    fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        let path = url.strip_prefix(API_ROOT).unwrap();
        let actions = format!("repos/{REPO}/actions/");
        let path = path.strip_prefix(&actions).unwrap();
        let workflow =
            |file: &str, rest: &str| path.starts_with(&format!("workflows/{file}/{rest}"));
        match method {
            "GET" if path == format!("runs/{ME}") => {
                if self.call("lookup") {
                    bail!("GitHub transport failed");
                }
                Ok((200, json!({ "run_started_at": self.started })))
            }
            "GET" if workflow("pass.yml", "runs?branch=main&") => {
                if self.call("list") {
                    return Ok((502, json!({})));
                }
                let dispatched = self.calls.borrow().contains_key("dispatch");
                let runs = match &self.successor {
                    Some(successor) if dispatched => successor,
                    _ => &self.runs,
                };
                Ok((200, Self::page(runs, path)))
            }
            "GET" if workflow("supervisor.yml", "runs?branch=main&") => {
                if self.call("supervisor list") {
                    return Ok((500, json!({})));
                }
                Ok((200, Self::page(&self.supervisor_runs, path)))
            }
            "GET" if workflow("train.yml", "runs?branch=main&") => {
                if self.call("train list") {
                    return Ok((500, json!({})));
                }
                Ok((200, Self::page(&self.train_runs, path)))
            }
            "POST"
                if workflow("pass.yml", "dispatches")
                    || workflow("supervisor.yml", "dispatches")
                    || workflow("train.yml", "dispatches") =>
            {
                let kind = if path.contains("pass.yml") {
                    "dispatch"
                } else if path.contains("train.yml") {
                    "train wake"
                } else {
                    "wake"
                };
                self.bodies.borrow_mut().push((kind.into(), body.clone()));
                if self.call(kind) {
                    return Ok((500, json!({"message": "Server Error"})));
                }
                Ok((204, Value::Null))
            }
            _ => panic!("unexpected {method} {url}"),
        }
    }
}

impl Clock for Fake {
    fn now(&self) -> OffsetDateTime {
        self.now.get()
    }

    fn sleep(&self, duration: Duration) {
        self.log
            .borrow_mut()
            .push(format!("sleep {}", duration.as_secs()));
        self.now.set(self.now.get() + duration);
    }
}

#[derive(Debug, PartialEq)]
enum Want {
    Dispatch,
    Skip,
    Fail,
}

fn run_next(fake: &Fake, settings: &Settings) -> (Result<()>, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let result = super::run(fake, fake, settings, &mut out, &mut err);
    let text = |b: Vec<u8>| String::from_utf8(b).unwrap();
    (result, text(out), text(err))
}

/// A name, the pass workflow's runs, the outcome, the calls of each kind
/// that fail, and the runs listed once a dispatch was tried.
type ChainCase = (
    &'static str,
    Vec<Value>,
    Want,
    BTreeMap<&'static str, usize>,
    Option<Vec<Value>>,
);

#[test]
fn chain() {
    let later = |fields| run(105, LATER, fields);
    let page_of = |n: u64| (0..n).map(|i| run(1000 - i, LATER, json!({"status": "completed"})));
    let fails = |pairs: &[(&'static str, usize)]| pairs.iter().copied().collect::<BTreeMap<_, _>>();
    let cases: Vec<ChainCase> = vec![
        ("lone chain", vec![me()], Want::Dispatch, fails(&[]), None),
        (
            "offset duplicate chain",
            vec![later(applying("in_progress")), me()],
            Want::Skip,
            fails(&[]),
            None,
        ),
        (
            "newer schedule run",
            vec![later(json!({})), me()],
            Want::Skip,
            fails(&[]),
            None,
        ),
        (
            "this run on the second page",
            page_of(RUN_PAGE as u64).chain([me()]).collect(),
            Want::Dispatch,
            fails(&[]),
            None,
        ),
        (
            "this run beyond the list",
            page_of((RUN_PAGE * RUN_PAGES) as u64)
                .chain([me()])
                .collect(),
            Want::Fail,
            fails(&[]),
            None,
        ),
        // agent-board#715: one listing's tries failing is one failed start.
        (
            "run list fails four times",
            vec![me()],
            Want::Dispatch,
            fails(&[("list", 4)]),
            None,
        ),
        (
            "run list keeps failing",
            vec![me()],
            Want::Fail,
            fails(&[("list", (TRIES * START_TRIES) as usize)]),
            None,
        ),
        (
            "transient lookup",
            vec![me()],
            Want::Dispatch,
            fails(&[("lookup", 1)]),
            None,
        ),
        (
            "lookup fails four times",
            vec![me()],
            Want::Fail,
            fails(&[("lookup", 4)]),
            None,
        ),
        (
            "transient listing",
            vec![me()],
            Want::Dispatch,
            fails(&[("list", 3)]),
            None,
        ),
        (
            "transient dispatch",
            vec![me()],
            Want::Dispatch,
            fails(&[("dispatch", 1)]),
            None,
        ),
        (
            "dispatch fails four times",
            vec![me()],
            Want::Dispatch,
            fails(&[("dispatch", 4)]),
            None,
        ),
        (
            "dispatch keeps failing",
            vec![me()],
            Want::Fail,
            fails(&[("dispatch", START_TRIES as usize)]),
            None,
        ),
        // tracker#1033: the failed dispatch started the next pass.
        (
            "dispatch errs but starts a run",
            vec![me()],
            Want::Skip,
            fails(&[("dispatch", 1)]),
            Some(vec![later(applying("queued")), me()]),
        ),
        // The supervisor's failures only warn.
        (
            "supervisor listing unreadable",
            vec![me()],
            Want::Dispatch,
            fails(&[("supervisor list", 4)]),
            None,
        ),
        (
            "supervisor dispatch fails",
            vec![me()],
            Want::Dispatch,
            fails(&[("wake", 4)]),
            None,
        ),
    ];
    for (name, runs, want, fail, successor) in cases {
        let mut fake = Fake::new(runs);
        fake.fail = fail;
        fake.successor = successor;
        let (result, out, err) = run_next(&fake, &settings());
        let detail = format!("{name}: {result:?}\n{out}{err}");
        let failed = |kind| fake.fail.get(kind).copied().unwrap_or_default();
        let dispatched = fake.count("dispatch") > failed("dispatch");
        let got = match (&result, dispatched) {
            (Err(_), _) => Want::Fail,
            (Ok(()), true) => Want::Dispatch,
            (Ok(()), false) => Want::Skip,
        };
        assert_eq!(got, want, "{detail}");
        let log = fake.log.borrow();
        // The lookup's tries, then the wait, from one minute past the start.
        let lookups = (failed("lookup") + 1).min(TRIES as usize);
        let (mut expected, mut retried) = (Vec::new(), 0);
        for attempt in 1..=lookups {
            if attempt > 1 {
                retried += retry_wait(attempt as u32).as_secs();
                expected.push(format!("sleep {}", retry_wait(attempt as u32).as_secs()));
            }
            expected.push("lookup".to_string());
        }
        // The retries' waits count toward the interval.
        if failed("lookup") < TRIES as usize {
            expected.push(format!("sleep {}", 540 - retried));
        }
        assert_eq!(log[..expected.len()], expected[..], "{detail}");
        assert!(fake.count("dispatch") <= START_TRIES as usize, "{detail}");
        assert!(
            fake.count("list") <= (TRIES * START_TRIES) as usize * RUN_PAGES,
            "{detail}"
        );
        // A dispatch is only ever tried right after the runs were listed.
        for (i, call) in log.iter().enumerate() {
            if call == "dispatch" {
                assert_eq!(log[i - 1], "list", "{detail}");
            }
        }
        if fake.successor.is_some() {
            assert_eq!(fake.count("dispatch"), 1, "{detail}");
        }
        if failed("supervisor list") >= TRIES as usize || failed("wake") >= TRIES as usize {
            assert!(
                err.contains("warning: could not start supervisor.yml"),
                "{detail}"
            );
        }
        for (kind, body) in fake.bodies.borrow().iter() {
            let inputs = match kind.as_str() {
                "dispatch" => json!({"apply": "true"}),
                _ => json!({"prompt": "", "session": "false"}),
            };
            assert_eq!(body, &json!({"ref": "main", "inputs": inputs}), "{detail}");
        }
    }
}

/// The 23:12Z pass of agent-board#715 gave up after 14 seconds of a GitHub
/// incident; starting the next pass now keeps trying for minutes.
#[test]
fn start_retries_outlast_a_short_outage() {
    let waits: Vec<Duration> = (2..=START_TRIES).map(retry_wait).collect();
    assert!(waits.iter().all(|w| *w <= MAX_RETRY_WAIT), "{waits:?}");
    let total: Duration = waits.iter().sum();
    assert!(
        (Duration::from_secs(5 * 60)..=Duration::from_secs(15 * 60)).contains(&total),
        "{total:?}"
    );
}

#[test]
fn supervisor_wake() {
    let stale = |event: &str| run(1, "2026-10-29T22:00:00Z", json!({ "event": event }));
    let fresh = run(2, T0, json!({"event": "issue_comment"}));
    for (name, supervisor_runs, enabled, wakes) in [
        ("fresh run", vec![run(1, T0, json!({}))], true, 0),
        ("stale run", vec![stale("schedule")], true, 1),
        (
            "a comment does not count",
            vec![fresh.clone(), stale("schedule")],
            true,
            1,
        ),
        (
            "only issue_comment runs",
            vec![fresh, stale("issue_comment")],
            true,
            1,
        ),
        ("no runs", vec![], true, 1),
        ("disabled", vec![stale("schedule")], false, 0),
    ] {
        let mut fake = Fake::new(vec![me()]);
        fake.supervisor_runs = supervisor_runs;
        let mut settings = settings();
        if !enabled {
            settings.supervisor = None;
        }
        let (result, out, err) = run_next(&fake, &settings);
        let detail = format!("{name}: {result:?}\n{out}{err}");
        assert!(result.is_ok(), "{detail}");
        assert_eq!(fake.count("wake"), wakes, "{detail}");
        assert_eq!(
            fake.count("supervisor list"),
            usize::from(enabled),
            "{detail}"
        );
        assert_eq!(fake.count("dispatch"), 1, "{detail}");
    }
}

#[test]
fn dry_run_starts_nothing() {
    let mut fake = Fake::new(vec![me()]);
    fake.fail = [
        ("lookup", 1),
        ("supervisor list", 4),
        ("train list", 1),
        ("list", 1),
    ]
    .into_iter()
    .collect();
    let mut settings = Settings {
        dry_run: true,
        ..settings()
    };
    settings.train = Some(train());
    let (result, out, _) = run_next(&fake, &settings);
    result.unwrap();
    assert_eq!(
        out,
        "would wait 540s\nwould start train.yml\nwould start pass.yml\n"
    );
    assert!(fake.bodies.borrow().is_empty());
    assert!(!fake.log.borrow().iter().any(|c| c.starts_with("sleep")));
}

fn train() -> Supervisor {
    Supervisor {
        workflow: Workflow {
            file: "train.yml".into(),
            inputs: [("release".into(), "true".into())].into_iter().collect(),
        },
        stale: Duration::from_secs(15 * 60),
    }
}

#[test]
fn train_without_supervisor_preserves_newer_pass_suppression() {
    for supervisor_enabled in [false, true] {
        let fake = Fake::new(vec![run(105, LATER, applying("queued")), me()]);
        let mut settings = settings();
        settings.train = Some(train());
        if !supervisor_enabled {
            settings.supervisor = None;
        }
        let (result, out, err) = run_next(&fake, &settings);
        result.unwrap();
        assert!(err.is_empty(), "{err}");
        assert!(out.contains("run 105 continues the chain"), "{out}");
        assert_eq!(fake.count("dispatch"), 0);
        assert_eq!(fake.count("train wake"), 1);
        assert_eq!(
            fake.count("supervisor list"),
            usize::from(supervisor_enabled)
        );
    }
}

#[test]
fn independent_auxiliary_wakes() {
    for (name, runs, enabled, fails, wakes) in [
        ("omitted", vec![], false, None, 0),
        ("recent", vec![run(1, T0, json!({}))], true, None, 0),
        // At wake time the clock is T0 + ten minutes: exactly 15 minutes old.
        ("threshold", vec![run(1, EARLIER, json!({}))], true, None, 1),
        ("missing", vec![], true, None, 1),
        (
            "comment counts",
            vec![run(1, T0, json!({"event": "issue_comment"}))],
            true,
            None,
            0,
        ),
        (
            "malformed timestamp",
            vec![run(1, "yesterday", json!({}))],
            true,
            None,
            0,
        ),
        ("unreadable listing", vec![], true, Some("train list"), 0),
        ("exhausted dispatch", vec![], true, Some("train wake"), 4),
        ("supervisor fails first", vec![], true, Some("wake"), 1),
    ] {
        let mut fake = Fake::new(vec![me()]);
        fake.train_runs = runs;
        if let Some(kind) = fails {
            fake.fail.insert(kind, TRIES as usize);
        }
        let mut settings = settings();
        settings.train = enabled.then(train);
        let (result, out, err) = run_next(&fake, &settings);
        let detail = format!("{name}: {result:?}\n{out}{err}");
        assert!(result.is_ok(), "{detail}");
        assert_eq!(fake.count("train wake"), wakes, "{detail}");
        assert_eq!(
            fake.count("train list"),
            if fails == Some("train list") {
                4
            } else {
                usize::from(enabled)
            },
            "{detail}"
        );
        assert_eq!(fake.count("dispatch"), 1, "{detail}");
        assert_eq!(
            fake.count("wake"),
            if fails == Some("wake") { 4 } else { 1 },
            "{detail}"
        );
        if fails.is_some() || name == "malformed timestamp" {
            assert!(err.contains("continuing the chain"), "{detail}");
        }
        for (kind, body) in fake.bodies.borrow().iter() {
            if kind == "train wake" {
                assert_eq!(
                    body,
                    &json!({"ref": "main", "inputs": {"release": "true"}}),
                    "{detail}"
                );
            }
        }
    }
}

#[test]
fn invalid_settings() {
    for (name, change) in [
        (
            "repository",
            (|s: &mut Settings| s.repository = "board".into()) as fn(&mut Settings),
        ),
        ("run", |s| s.run = 0),
        ("branch", |s| s.branch = String::new()),
        ("workflow path", |s| s.pass.file = "../pass.yml".into()),
        ("workflow query", |s| s.pass.file = "pass.yml?x=1".into()),
        ("train workflow path", |s| {
            s.train = Some(train());
            s.train.as_mut().unwrap().workflow.file = "../train.yml".into();
        }),
        ("apply title", |s| s.apply_title = String::new()),
        ("interval", |s| {
            s.interval = MAX_INTERVAL + Duration::from_secs(1)
        }),
    ] {
        let mut settings = settings();
        change(&mut settings);
        let fake = Fake::new(vec![me()]);
        assert!(run_next(&fake, &settings).0.is_err(), "{name}");
        assert!(fake.log.borrow().is_empty(), "{name}");
    }
}
