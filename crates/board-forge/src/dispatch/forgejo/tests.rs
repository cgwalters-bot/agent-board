use super::*;
use crate::forgejo::Response;
use std::cell::RefCell;

fn instance() -> Config {
    serde_json::from_value(json!({"url":"https://forge.example/", "repositories":["example/app"], "membership":"open", "recorders":[7]})).unwrap()
}

fn target() -> Target {
    Target {
        caller: "example/caller".into(),
        workflow: "dispatch.yml".into(),
        git_ref: "main".into(),
    }
}

#[test]
fn forgejo_target_names_and_path_are_not_github_rules() {
    let fake = Fake::new(vec![]);
    let mut instance = instance();
    instance.repositories = vec!["team.example/work.repo".into()];
    let mut target = target();
    target.caller = "team.example/control.repo".into();
    assert!(target.validate().is_err());
    let adapter = Forgejo::new(
        &fake,
        &instance,
        &target,
        ["task", "repo", "item", "kind"].map(String::from).into(),
        7,
    )
    .unwrap();
    assert_eq!(
        adapter.path("runs").unwrap().as_str(),
        "https://forge.example/api/v1/repos/team.example/control.repo/actions/runs"
    );
    let mut start = start();
    start.repository = "team.example/work.repo";
    start.pool = None;
    assert!(adapter.body(&start).is_ok());
    for bad in [
        "../repo",
        "a/..",
        "//evil.example/repo",
        "a/b?x",
        "a/b#x",
        "a/b\\c",
        "a/%2e%2e",
    ] {
        let mut target = target.clone();
        target.caller = bad.into();
        assert!(
            Forgejo::new(
                &fake,
                &instance,
                &target,
                ["task", "repo", "item", "kind"].map(String::from).into(),
                7
            )
            .is_err(),
            "{bad}"
        );
    }
}

fn start() -> Start<'static> {
    Start {
        repository: "example/app",
        item: 9,
        kind: "research",
        brief: "Look.",
        pool: Some("small"),
        runner: None,
        base: None,
    }
}

fn clock() -> OffsetDateTime {
    OffsetDateTime::parse("2026-10-30T12:00:00Z", &Rfc3339).unwrap()
}

// Source-shaped, minimized API fixture: v17.0.1 modules/structs/action.go
// and services/convert/action.go. event_payload is a string, jobs an array,
// and the URL uses index_in_repo (not the global id).
fn run() -> Value {
    json!({"id":42, "index_in_repo":3, "workflow_id":"dispatch.yml", "prettyref":"main", "trigger_user":{"id":7},
        "event":"", "trigger_event":"workflow_dispatch", "created":"2026-10-30T12:00:01Z",
        "title":"research example/app#9 (small)", "status":"waiting", "repository":{"full_name":"Example/Caller"}, "html_url":"https://public.example/Example/Caller/actions/runs/3",
        "event_payload":json!({"inputs":{"task":"Look.", "repo":"example/app", "item":"9", "kind":"research", "pool":"small"}}).to_string()})
}

struct Fake {
    runs: RefCell<Vec<Value>>,
    posts: RefCell<Vec<(String, Value)>>,
    reply: Option<u16>,
    total_delta: u64,
    page_size: usize,
    reads: Cell<usize>,
    move_status: bool,
    status_error: bool,
    jobs: RefCell<Value>,
}

impl Fake {
    fn new(runs: Vec<Value>) -> Self {
        Self {
            runs: runs.into(),
            posts: Default::default(),
            reply: Some(201),
            total_delta: 0,
            page_size: PAGE_SIZE,
            reads: Cell::new(0),
            move_status: false,
            status_error: false,
            jobs: RefCell::new(json!([{"id":1,"run_id":42,"attempt":1,"status":"failure"}])),
        }
    }
}

impl Transport for Fake {
    fn get(&self, url: &url::Url) -> Result<Response> {
        assert!(url.as_str().starts_with("https://forge.example/api/v1/"));
        let path = url.path();
        let body = if path.ends_with("/user") {
            json!({"id":7})
        } else if path.ends_with("/jobs") {
            self.jobs.borrow().clone()
        } else if path.ends_with("/runs/42") {
            ensure!(!self.status_error, "status read failed");
            self.runs
                .borrow()
                .iter()
                .find(|r| r["id"] == 42)
                .cloned()
                .context("missing run")?
        } else if path.ends_with("/runs") {
            let read = self.reads.get();
            self.reads.set(read + 1);
            if self.move_status && read == 1 {
                self.runs.borrow_mut()[0]["status"] = "running".into();
            }
            let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
            assert_eq!(query["workflow_id"], "dispatch.yml");
            assert_eq!(query["limit"], "50");
            let page: usize = query["page"].parse().unwrap();
            let runs: Vec<_> = self
                .runs
                .borrow()
                .iter()
                .filter(|r| query.get("status").is_none_or(|s| r["status"] == *s))
                .cloned()
                .collect();
            let batch: Vec<_> = runs
                .iter()
                .skip((page - 1) * self.page_size)
                .take(self.page_size)
                .cloned()
                .collect();
            json!({"total_count":runs.len() as u64 + self.total_delta, "workflow_runs":batch})
        } else {
            bail!("unexpected GET {url}")
        };
        Ok(Response { body, total: None })
    }

    fn send(&self, method: &str, url: &url::Url, body: &Value) -> Result<(u16, Value)> {
        assert_eq!(method, "POST");
        assert!(
            url.as_str()
                .starts_with("https://forge.example/api/v1/repos/example/caller/actions/")
        );
        self.posts
            .borrow_mut()
            .push((url.to_string(), body.clone()));
        let code = self.reply.context("lost reply")?;
        Ok((code, json!({"id":42})))
    }
}

fn backend<'a>(
    fake: &'a impl Transport,
    instance: &'a Config,
    target: &'a Target,
) -> Forgejo<'a, impl Transport + 'a> {
    Forgejo::new(
        fake,
        instance,
        target,
        ["task", "repo", "item", "kind", "pool", "base"]
            .map(String::from)
            .into(),
        7,
    )
    .unwrap()
}

#[test]
fn forgejo_start_outcomes_and_final_admission() {
    let instance = instance();
    let target = target();
    for (reply, runs, expected) in [
        (Some(201), vec![run()], "started"),
        (Some(204), vec![], "ambiguous"),
        (Some(201), vec![], "ambiguous"),
        (None, vec![run()], "ambiguous"),
        (Some(500), vec![run()], "ambiguous"),
        (Some(422), vec![], "refused"),
        (
            Some(201),
            vec![run(), {
                let mut r = run();
                r["id"] = 43.into();
                r
            }],
            "ambiguous",
        ),
    ] {
        let fake = Fake {
            reply,
            ..Fake::new(runs)
        };
        let backend = backend(&fake, &instance, &target);
        let calls = Cell::new(0);
        let outcome = backend.start(&start(), clock(), || {
            calls.set(calls.get() + 1);
            Ok(())
        });
        assert_eq!(calls.get(), 1);
        assert_eq!(
            match outcome {
                StartOutcome::Started(h) => {
                    assert_eq!(h.id, "42");
                    assert_eq!(h.url, "https://forge.example/example/caller/actions/runs/3");
                    "started"
                }
                StartOutcome::Ambiguous(_) => "ambiguous",
                StartOutcome::NotStarted(_) => "refused",
                StartOutcome::Unnamed => "unnamed",
            },
            expected
        );
        let posts = fake.posts.borrow();
        assert_eq!(
            posts
                .iter()
                .filter(|(u, _)| u.ends_with("dispatches"))
                .count(),
            1
        );
        assert_eq!(posts[0].1["return_run_info"], true);
        assert!(posts[0].1.get("return_run_details").is_none());
        assert_eq!(posts[0].1["ref"], "main");
    }
    // A full history refuses before admission: the start could not be listed.
    let full = |len: u64| {
        (1..=len)
            .map(|id| {
                let mut r = run();
                r["id"] = id.into();
                r["status"] = "success".into();
                r
            })
            .collect()
    };
    for (runs, admitted) in [(vec![], false), (full(500), false), (full(499), true)] {
        let fake = Fake {
            reply: Some(422),
            ..Fake::new(runs)
        };
        let calls = Cell::new(0);
        let outcome = backend(&fake, &instance, &target).start(&start(), clock(), || {
            calls.set(calls.get() + 1);
            ensure!(admitted, "cap spent");
            Ok(())
        });
        assert!(matches!(outcome, StartOutcome::NotStarted(_)));
        assert_eq!(fake.posts.borrow().len(), usize::from(admitted));
        assert_eq!(calls.get(), usize::from(fake.runs.borrow().len() < 500));
    }
}

#[test]
fn forgejo_start_same_second_claim_and_cancellation() {
    let instance = instance();
    let target = target();
    let claim = clock().replace_nanosecond(700_000_000).unwrap();
    for (case, created, trigger, read_error, cancelled, started) in [
        (
            "same second",
            "2026-10-30T12:00:00Z",
            "workflow_dispatch",
            false,
            false,
            true,
        ),
        (
            "previous second",
            "2026-10-30T11:59:59Z",
            "workflow_dispatch",
            false,
            true,
            false,
        ),
        (
            "identity mismatch",
            "2026-10-30T12:00:00Z",
            "push",
            false,
            true,
            false,
        ),
        (
            "read error",
            "2026-10-30T12:00:00Z",
            "workflow_dispatch",
            true,
            false,
            false,
        ),
    ] {
        let mut r = run();
        r["created"] = created.into();
        r["trigger_event"] = trigger.into();
        let fake = Fake {
            status_error: read_error,
            ..Fake::new(vec![r])
        };
        let outcome = backend(&fake, &instance, &target).start(&start(), claim, || Ok(()));
        assert_eq!(
            matches!(outcome, StartOutcome::Started(_)),
            started,
            "{case}"
        );
        if !started {
            assert!(matches!(outcome, StartOutcome::Ambiguous(_)), "{case}");
        }
        assert_eq!(
            fake.posts
                .borrow()
                .iter()
                .filter(|(url, _)| url.ends_with("/42/cancel"))
                .count(),
            usize::from(cancelled),
            "{case}"
        );
        assert_eq!(
            fake.posts.borrow().len(),
            1 + usize::from(cancelled),
            "{case}"
        );
    }
}

#[test]
fn forgejo_identity_and_missing_trigger_hold() {
    let instance = instance();
    let target = target();
    for (key, value) in [
        ("trigger_user", json!({"id":8})),
        ("event_payload", json!("{\"inputs\":{}}")),
        ("workflow_id", json!("other.yml")),
        ("prettyref", json!("feature")),
        ("trigger_event", json!("push")),
        ("event", json!("push")),
        ("repository", json!({"full_name":"other/caller"})),
        ("title", json!("research example/app#9")),
        ("created", json!("2026-10-30T11:59:59Z")),
        ("status", json!("unknown")),
    ] {
        let mut r = run();
        r[key] = value;
        let fake = Fake::new(vec![r]);
        let backend = backend(&fake, &instance, &target);
        assert!(backend.recover(&start(), clock()).is_err(), "{key}");
        assert!(fake.posts.borrow().is_empty());
    }
    // v17.0.1 answers "" (the fixture); a release naming the event still fits.
    let mut r = run();
    r["event"] = "workflow_dispatch".into();
    let fake = Fake::new(vec![r]);
    assert!(
        backend(&fake, &instance, &target)
            .recover(&start(), clock())
            .is_ok()
    );
    let mut r = run();
    r["trigger_event"] = "".into();
    let fake = Fake::new(vec![r]);
    assert!(matches!(
        backend(&fake, &instance, &target).start(&start(), clock(), || Ok(())),
        StartOutcome::Ambiguous(_)
    ));
    assert!(
        fake.posts
            .borrow()
            .iter()
            .any(|(url, _)| url.ends_with("/42/cancel"))
    );
}

#[test]
fn forgejo_pagination_and_status_transitions() {
    let instance = instance();
    let target = target();
    for status in [
        "blocked",
        "waiting",
        "running",
        "success",
        "failure",
        "cancelled",
        "skipped",
    ] {
        let mut r = run();
        r["status"] = status.into();
        let fake = Fake::new(vec![r]);
        assert_eq!(
            backend(&fake, &instance, &target)
                .active_runs()
                .unwrap()
                .len(),
            usize::from(ACTIVE.contains(&status))
        );
    }
    let mut fake = Fake::new(vec![run()]);
    fake.move_status = true;
    assert_eq!(
        backend(&fake, &instance, &target).active_runs().unwrap()[0]["status"],
        "running"
    );
    let runs = (1..=51)
        .map(|id| {
            let mut r = run();
            r["id"] = id.into();
            r
        })
        .collect();
    let fake = Fake::new(runs);
    assert_eq!(
        backend(&fake, &instance, &target)
            .active_runs()
            .unwrap()
            .len(),
        51
    );
    for (delta, size) in [(1, 50), (0, 0)] {
        let fake = Fake {
            total_delta: delta,
            page_size: size,
            ..Fake::new(vec![run()])
        };
        assert!(backend(&fake, &instance, &target).active_runs().is_err());
    }
    let fake = Fake::new(
        (1..=501)
            .map(|id| {
                let mut r = run();
                r["id"] = id.into();
                r
            })
            .collect(),
    );
    assert!(backend(&fake, &instance, &target).active_runs().is_err());
}

#[test]
fn forgejo_rerun_attempts_and_ambiguity() {
    let instance = instance();
    let target = target();
    for (status, attempt, reply, expected) in [
        ("failure", 1, Some(204), "started"),
        ("failure", 2, Some(204), "refused"),
        ("running", 1, Some(204), "refused"),
        ("failure", 1, None, "ambiguous"),
        ("failure", 1, Some(400), "refused"),
    ] {
        let mut r = run();
        r["status"] = status.into();
        let fake = Fake {
            reply,
            ..Fake::new(vec![r])
        };
        fake.jobs.borrow_mut()[0]["attempt"] = attempt.into();
        let outcome = backend(&fake, &instance, &target).rerun(42, &start(), clock(), || Ok(()));
        assert_eq!(
            match outcome {
                StartOutcome::Started(h) => {
                    assert_eq!(h.id, "42");
                    "started"
                }
                StartOutcome::NotStarted(_) => "refused",
                StartOutcome::Ambiguous(_) => "ambiguous",
                _ => "unnamed",
            },
            expected
        );
        assert!(fake.posts.borrow().len() <= 1);
    }
}

/// Models the pinned contents API: expected blob is tested on a snapshot;
/// commit is a child of that snapshot and a non-FF push is refused. The
/// atomic winner is decided at push, not at a subsequent read-back.
struct Contents {
    body: RefCell<Value>,
    version: Cell<u64>,
    lost: Cell<bool>,
    private: Cell<bool>,
}

impl Transport for Contents {
    fn get(&self, url: &url::Url) -> Result<Response> {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let body = if url.path().ends_with("/user") {
            json!({"id":7})
        } else if url.path().ends_with("/example/caller") {
            json!({"private":self.private.get()})
        } else {
            json!({"type":"file", "encoding":"base64", "sha":format!("{:040x}",self.version.get()), "content": STANDARD.encode(serde_json::to_vec(&*self.body.borrow())?)})
        };
        Ok(Response { body, total: None })
    }
    fn send(&self, method: &str, _: &url::Url, body: &Value) -> Result<(u16, Value)> {
        use base64::{Engine, engine::general_purpose::STANDARD};
        assert_eq!(method, "PUT");
        assert!(body.get("new_branch").is_none() && body.get("last_commit_id").is_none());
        if body["sha"] != format!("{:040x}", self.version.get()) {
            return Ok((409, Value::Null));
        }
        self.body.replace(serde_json::from_slice(
            &STANDARD.decode(body["content"].as_str().unwrap())?,
        )?);
        self.version.set(self.version.get() + 1);
        if self.lost.get() {
            bail!("reply lost after push")
        }
        Ok((200, json!({})))
    }
}

#[test]
fn forgejo_two_contenders_late_release_replacement_and_expiry() {
    let instance = instance();
    let target = target();
    let fake = Contents {
        body: RefCell::new(json!({"free":"initial"})),
        version: Cell::new(1),
        lost: Cell::new(false),
        private: Cell::new(true),
    };
    let backend = backend(&fake, &instance, &target);
    let lease = Lease {
        run: 0,
        claim: "unique-claim-1".into(),
        item: "example/app#9".into(),
        pool: "small".into(),
        taken_at: clock().format(&Rfc3339).unwrap(),
        expires_at: (clock() + time::Duration::days(1))
            .format(&Rfc3339)
            .unwrap(),
    };
    let a = backend.slot("admission", 0).unwrap();
    let bound = Lease {
        run: 42,
        ..lease.clone()
    };
    assert!(backend.admit(&a, &bound, clock(), || panic!()).is_err());
    // Refusals before the final check: an oversized lease, and a public
    // caller (its read must precede the callback, which only the PUT follows).
    let oversized = Lease {
        pool: "p".repeat(1024),
        ..lease.clone()
    };
    assert!(backend.admit(&a, &oversized, clock(), || panic!()).is_err());
    fake.private.set(false);
    assert!(backend.admit(&a, &lease, clock(), || panic!()).is_err());
    fake.private.set(true);
    assert_eq!(fake.version.get(), 1);
    let b = backend.slot("admission", 0).unwrap();
    assert!(backend.admit(&a, &lease, clock(), || Ok(())).unwrap());
    assert!(!backend.admit(&b, &lease, clock(), || Ok(())).unwrap());
    let held = backend.slot("admission", 0).unwrap();
    assert!(backend.release(&held, "foreign", 0, clock()).is_err());
    assert!(backend.release(&held, &lease.claim, 0, clock()).is_err());
    let expired = clock() + time::Duration::days(1);
    assert!(backend.bind(&held, &lease.claim, 42, expired).is_err());
    assert!(backend.release(&held, &lease.claim, 0, expired).unwrap());
    let free = backend.slot("admission", 0).unwrap();
    let replacement = Lease {
        claim: "unique-claim-2".into(),
        taken_at: expired.format(&Rfc3339).unwrap(),
        expires_at: (expired + time::Duration::days(1))
            .format(&Rfc3339)
            .unwrap(),
        ..lease.clone()
    };
    assert!(
        backend
            .admit(&free, &replacement, expired, || Ok(()))
            .unwrap()
    );
    assert!(!backend.release(&held, &lease.claim, 0, expired).unwrap());
    let held = backend.slot("admission", 0).unwrap();
    assert!(
        backend
            .bind(&held, &replacement.claim, 42, expired)
            .unwrap()
    );
    let held = backend.slot("admission", 0).unwrap();
    fake.lost.set(true);
    assert!(
        backend
            .release(
                &held,
                &replacement.claim,
                42,
                expired + time::Duration::days(1)
            )
            .is_err()
    );
    assert!(backend.slot("admission", 0).unwrap().lease().is_none());
}
