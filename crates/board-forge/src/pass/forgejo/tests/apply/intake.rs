use super::*;

struct Directory(std::path::PathBuf);

impl Directory {
    fn new() -> Self {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).unwrap();
        let path =
            std::env::temp_dir().join(format!("native-intake-{:032x}", u128::from_le_bytes(nonce)));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn setup(directory: &Directory) -> (Applying, Value) {
    let fake = Applying::new();
    fake.comments.borrow_mut().clear();
    fake.add(7, 400, OPERATOR, "/bot investigate the issue");
    let mut value = file();
    value["board"]["url"] = URL.into();
    value["board"]["intake"] = json!({"repositories":["example/work"],"operatorId":OPERATOR});
    value["forgejoIntake"] = json!({"state":directory.path().join("cursor.json")});
    value["wip"] = 2.into();
    value["workRepositories"] = json!(["example/work"]);
    value["pools"] = json!({"claude":{}});
    value["planPools"] = json!(["claude"]);
    value["leases"] = true.into();
    (fake, value)
}

fn pass(fake: &Applying, value: &Value, mode: Mode<'_>, limits: Limits) -> Report {
    run(
        fake,
        &BrokerFake::default(),
        &config(value),
        (mode, limits),
        usage::tests::now(),
    )
    .unwrap()
}

fn plans(fake: &Applying) -> usize {
    fake.comments
        .borrow()
        .values()
        .flatten()
        .filter(|c| text(c, "/body").starts_with("<!-- agent-board-intake:"))
        .count()
}

#[test]
fn addressed_issue_asks_use_native_start_once_with_trailers_as_metadata() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../../../../fixtures/forgejo/pass-intake.json"
    ))
    .unwrap();
    for ask in fixture["asks"].as_array().unwrap() {
        let body = ask["body"].as_str().unwrap();
        let dir = Directory::new();
        let (fake, value) = setup(&dir);
        fake.comments.borrow_mut().get_mut(&7).unwrap()[0]["body"] = body.into();
        let report = pass(&fake, &value, Mode::Local, Limits::default());
        assert_eq!(report.failures, 0, "{:?}", report.lines);
        assert_eq!(fake.posts("/dispatches"), 1, "{:?}", report.lines);
        assert_eq!(plans(&fake), 1);
        assert!(
            fake.runs.borrow()[0]["title"]
                .as_str()
                .unwrap()
                .starts_with(ask["kind"].as_str().unwrap())
        );
        assert!(fake.comments.borrow()[&7].iter().any(|c| text(c, "/body").contains("> /bot") || text(c, "/body").contains("> @BOT")));
        // A lost cursor replays the source discovery, not the plan or start.
        std::fs::remove_file(dir.path().join("cursor.json")).unwrap();
        pass(&fake, &value, Mode::Local, Limits::default());
        assert_eq!(plans(&fake), 1);
        assert_eq!(fake.posts("/dispatches"), 1);
        assert!(!report.lines.join("\n").contains(body));
    }
}

#[test]
fn caps_keep_pending_plans_or_cursors_and_boundary_asks_survive() {
    for writes in [0, 1] {
        let dir = Directory::new();
        let (fake, value) = setup(&dir);
        let report = pass(
            &fake,
            &value,
            Mode::Local,
            Limits {
                writes: Some(writes),
                dispatch: Some(0),
                ..Limits::default()
            },
        );
        assert_eq!(plans(&fake), writes);
        if writes == 0 {
            assert!(
                report
                    .holds
                    .iter()
                    .any(|e| e.hold.reason.contains("--max-writes"))
            );
        }
        assert_eq!(dir.path().join("cursor.json").exists(), writes > 0);
        assert_eq!(fake.posts("/dispatches"), 0);
        let report = pass(&fake, &value, Mode::Local, Limits::default());
        assert_eq!(fake.posts("/dispatches"), 1, "{:?}", report.lines);
        assert_eq!(plans(&fake), 1);
    }
    let dir = Directory::new();
    let (fake, value) = setup(&dir);
    fake.add(7, 401, OPERATOR, "/bot triage another question");
    pass(
        &fake,
        &value,
        Mode::Local,
        Limits {
            dispatch: Some(0),
            ..Limits::default()
        },
    );
    assert_eq!(plans(&fake), 1);
    assert!(!dir.path().join("cursor.json").exists());
    pass(&fake, &value, Mode::Local, Limits::default());
    assert_eq!(plans(&fake), 1);
    assert_eq!(fake.posts("/dispatches"), 1);
    // Completing the first run releases its capacity; the second ask has not
    // been lost behind a cursor or replaced an unclaimed first plan.
    fake.runs.borrow_mut()[0]["status"] = "success".into();
    pass(&fake, &value, Mode::Local, Limits::default());
    assert_eq!(plans(&fake), 2);
    assert_eq!(fake.posts("/dispatches"), 2);
}

#[test]
fn crashes_and_ambiguous_replies_never_duplicate_a_plan_or_start() {
    for fault in [
        "plan-reply-lost",
        "claim-reply-lost",
        "receipt-before",
        "receipt-lost",
        "lost-reply",
        "204",
    ] {
        let dir = Directory::new();
        let (mut fake, value) = setup(&dir);
        fake.fault = fault;
        let report = pass(&fake, &value, Mode::Local, Limits::default());
        if fault == "plan-reply-lost" {
            assert!(report.failures > 0, "{:?}", report.lines);
            assert!(
                report
                    .holds
                    .iter()
                    .any(|e| e.hold.kind == HoldKind::Unreadable
                        && e.hold.reason.contains("intake planning"))
            );
            assert!(!dir.path().join("cursor.json").exists());
        }
        let starts = fake.posts("/dispatches");
        fake.fault = "";
        let _ = std::fs::remove_file(dir.path().join("cursor.json"));
        pass(&fake, &value, Mode::Local, Limits::default());
        assert_eq!(plans(&fake), 1, "{fault}");
        assert_eq!(fake.posts("/dispatches"), starts, "{fault}");
    }
}

#[test]
fn identity_and_source_refusals() {
    for refusal in [
        "same-id",
        "mismatch",
        "visitor",
        "bot",
        "edited",
        "unaddressed",
        "scope",
        "membership",
        "closed",
        "visitor-issue",
        "needs-human",
        "pr",
    ] {
        let dir = Directory::new();
        let (mut fake, mut value) = setup(&dir);
        match refusal {
            "same-id" => fake.reads.bot["id"] = OPERATOR.into(),
            "mismatch" => value["board"]["intake"]["operatorId"] = 99.into(),
            "visitor" => {
                fake.comments.borrow_mut().get_mut(&7).unwrap()[0]["user"]["id"] = 999.into()
            }
            "bot" => fake.comments.borrow_mut().get_mut(&7).unwrap()[0]["user"]["id"] = BOT.into(),
            "edited" => {
                fake.comments.borrow_mut().get_mut(&7).unwrap()[0]["updated_at"] =
                    "2026-10-23T15:15:11Z".into()
            }
            "unaddressed" => {
                fake.comments.borrow_mut().get_mut(&7).unwrap()[0]["body"] = "a comment".into()
            }
            "scope" => value["workRepositories"] = json!(["example/other"]),
            "membership" => fake.reads.listed.issues[0]["labels"] = json!([]),
            "closed" => fake.reads.listed.issues[0]["state"] = "closed".into(),
            "visitor-issue" => fake.reads.listed.issues[0]["user"]["id"] = 999.into(),
            "needs-human" => fake.reads.listed.issues[0]["labels"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name":"needs-human"})),
            "pr" => fake.reads.listed.issues[0]["pull_request"] = json!({"id":7}),
            _ => unreachable!(),
        }
        let report = pass(&fake, &value, Mode::Local, Limits::default());
        assert_eq!(plans(&fake), 0, "{refusal}: {:?}", report.lines);
        assert_eq!(fake.posts("/dispatches"), 0, "{refusal}");
    }
}

#[test]
fn durable_source_and_plan_are_rechecked() {
    for change in ["body", "author", "timestamp", "marker", "plan"] {
        let dir = Directory::new();
        let (fake, value) = setup(&dir);
        pass(
            &fake,
            &value,
            Mode::Local,
            Limits {
                dispatch: Some(0),
                ..Limits::default()
            },
        );
        let mut comments = fake.comments.borrow_mut();
        let thread = comments.get_mut(&7).unwrap();
        match change {
            "body" => thread[0]["body"] = "/bot research something else".into(),
            "author" => thread[0]["user"]["id"] = BOT.into(),
            "timestamp" => thread[0]["updated_at"] = "2026-10-23T15:15:11Z".into(),
            "marker" => thread[1]["user"]["id"] = OPERATOR.into(),
            "plan" => {
                thread[1]["body"] = text(&thread[1], "/body")
                    .replace("research", "implement")
                    .into()
            }
            _ => unreachable!(),
        }
        drop(comments);
        pass(&fake, &value, Mode::Local, Limits::default());
        assert_eq!(fake.posts("/dispatches"), 0, "{change}");
        assert_eq!(plans(&fake), 1);
    }
}

#[test]
fn source_races_after_claim_never_dispatch_or_retry() {
    for change in [
        "source-body",
        "source-author",
        "source-time",
        "membership",
        "needs-human",
        "closed",
    ] {
        let dir = Directory::new();
        let (mut fake, value) = setup(&dir);
        fake.boundary = "claim";
        fake.change = change;
        pass(&fake, &value, Mode::Local, Limits::default());
        assert_eq!(fake.posts("/dispatches"), 0, "{change}");
        assert!(
            fake.comments.borrow()[&7]
                .iter()
                .any(|c| text(c, "/body").starts_with("<!-- agent-board-dispatch:")),
            "{change}"
        );
        let _ = std::fs::remove_file(dir.path().join("cursor.json"));
        pass(&fake, &value, Mode::Local, Limits::default());
        assert_eq!(plans(&fake), 1, "{change}");
        assert_eq!(fake.posts("/dispatches"), 0, "{change}");
    }
}

#[test]
fn paged_feed_and_late_boundary_comment_are_not_lost() {
    let dir = Directory::new();
    let (fake, value) = setup(&dir);
    for id in 401..455 {
        fake.add(7, id, 999, "metadata, not a request");
    }
    pass(&fake, &value, Mode::Local, Limits::default());
    assert_eq!(fake.posts("/dispatches"), 1);
    let state: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("cursor.json")).unwrap()).unwrap();
    assert_eq!(
        state["repositories"]["example/work"],
        "2026-10-23T15:15:10Z"
    );
    // Arrived after the prior feed read, in the cursor second.
    fake.add(7, 900, OPERATOR, "/bot triage the follow-up");
    fake.runs.borrow_mut()[0]["status"] = "success".into();
    let report = pass(&fake, &value, Mode::Local, Limits::default());
    assert_eq!(fake.posts("/dispatches"), 2, "{:?}", report.lines);
    assert_eq!(plans(&fake), 2);
    pass(&fake, &value, Mode::Local, Limits::default());
    assert_eq!(plans(&fake), 2);
    assert_eq!(fake.posts("/dispatches"), 2);
}

#[test]
fn unsupported_asks_do_not_pin_history_past_the_page_bound() {
    for refusal in ["edited", "pr", "closed", "scope", "membership"] {
        let dir = Directory::new();
        let (mut fake, mut value) = setup(&dir);
        let start = usage::tests::now();
        let old = (start - time::Duration::seconds(2))
            .format(&Rfc3339)
            .unwrap();
        {
            let mut comments = fake.comments.borrow_mut();
            let source = &mut comments.get_mut(&7).unwrap()[0];
            source["created_at"] = old.clone().into();
            source["updated_at"] = old.into();
            match refusal {
                "edited" => source["updated_at"] = start.format(&Rfc3339).unwrap().into(),
                "pr" => source["pull_request"] = json!({"id":7}),
                "closed" => fake.reads.listed.issues[0]["state"] = "closed".into(),
                "scope" => value["workRepositories"] = json!(["example/other"]),
                "membership" => fake.reads.listed.issues[0]["labels"] = json!([]),
                _ => unreachable!(),
            }
        }
        let first = pass(&fake, &value, Mode::Local, Limits::default());
        assert!(
            first.lines.iter().any(|l| l.contains("rejected:")),
            "{refusal}: {:?}",
            first.lines
        );
        assert_eq!(plans(&fake), 0, "{refusal}");
        assert!(dir.path().join("cursor.json").exists(), "{refusal}");
        fake.reads.listed.issues[0]["state"] = "open".into();
        value["workRepositories"] = json!(["example/work"]);
        // More than ten pages accumulate in repository history, but every
        // complete invocation accounts for its discoveries and moves on.
        let mut id = 10000;
        for tick in 1..=crate::forgejo::intake::PAGE_CAP + 1 {
            let now = start + time::Duration::seconds((tick * 3) as i64);
            let at = (now - time::Duration::seconds(1)).format(&Rfc3339).unwrap();
            for _ in 0..50 {
                fake.add(7, id, 999, "metadata");
                let mut comments = fake.comments.borrow_mut();
                let source = comments.get_mut(&7).unwrap().last_mut().unwrap();
                source["created_at"] = at.clone().into();
                source["updated_at"] = at.clone().into();
                id += 1;
            }
            let last = tick == crate::forgejo::intake::PAGE_CAP + 1;
            if last {
                fake.reads
                    .listed
                    .issues
                    .push(issue(9, OPERATOR, &["agent-board"]));
                fake.add(9, id, OPERATOR, "/bot triage the later ask");
                let mut comments = fake.comments.borrow_mut();
                let source = comments.get_mut(&9).unwrap().last_mut().unwrap();
                source["created_at"] = at.clone().into();
                source["updated_at"] = at.into();
            }
            let report = run(
                &fake,
                &BrokerFake::default(),
                &config(&value),
                (Mode::Local, Limits::default()),
                now,
            )
            .unwrap();
            assert!(
                !report
                    .lines
                    .iter()
                    .any(|l| l.starts_with("intake example/work error:")),
                "{refusal}: {:?}",
                report.lines
            );
            assert_eq!(
                plans(&fake),
                usize::from(last),
                "{refusal}: {:?}",
                report.lines
            );
            if last {
                assert_eq!(
                    fake.posts("/dispatches"),
                    1,
                    "{refusal}: {:?}",
                    report.lines
                );
            }
        }
        assert_eq!(fake.posts("/dispatches"), 1, "{refusal}");
    }
}

#[test]
fn differently_cased_selectors_use_the_configured_writer_target() {
    let dir = Directory::new();
    let (fake, mut value) = setup(&dir);
    value["board"]["intake"]["repositories"] = json!(["Example/Work"]);
    let fake = Isolated(fake);
    let report = run(
        &fake,
        &BrokerFake::default(),
        &config(&value),
        (Mode::Local, Limits::default()),
        usage::tests::now(),
    )
    .unwrap();
    assert_eq!(report.failures, 0, "{:?}", report.lines);
    assert_eq!(plans(&fake.0), 1);
    assert_eq!(fake.0.posts("/dispatches"), 1);
}

#[test]
fn isolated_intake_read_failure_marks_an_otherwise_healthy_pass_failed() {
    let dir = Directory::new();
    let (fake, mut value) = setup(&dir);
    value["board"]["intake"]["repositories"] = json!(["example/work", "example/missing"]);
    let fake = Isolated(fake);
    let report = run(
        &fake,
        &BrokerFake::default(),
        &config(&value),
        (Mode::Local, Limits::default()),
        usage::tests::now(),
    )
    .unwrap();
    assert_eq!(fake.0.posts("/dispatches"), 1, "{:?}", report.lines);
    assert_eq!(report.failures, 1, "{:?}", report.lines);
}

#[test]
fn preview_replay_never_reads_or_mutates_cursor_and_uses_no_github() {
    let dir = Directory::new();
    let (fake, value) = setup(&dir);
    let cfg = config(&value);
    let broker = BrokerFake::default();
    let recorder = Recorder::new(&fake, &broker, cfg.forgejo_board().unwrap());
    let preview = run(
        &recorder,
        &recorder,
        &cfg,
        (Mode::Preview, Limits::default()),
        usage::tests::now(),
    )
    .unwrap();
    let fixture: Value =
        serde_json::from_str(&recorder.fixture(usage::tests::now()).unwrap()).unwrap();
    let replay = Replay::from_json(&fixture, cfg.forgejo_board().unwrap()).unwrap();
    std::fs::write(dir.path().join("cursor.json"), "not even valid state").unwrap();
    let report = run(
        &replay,
        &replay,
        &cfg,
        (Mode::Preview, Limits::default()),
        replay.now,
    )
    .unwrap();
    replay.finish().unwrap();
    assert_eq!(report.lines, preview.lines);
    assert_eq!(report.failures, 0, "{:?}", report.lines);
    assert!(fake.writes.borrow().is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("cursor.json")).unwrap(),
        "not even valid state"
    );
    assert!(
        fixture["requests"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["request"]["get"].as_str())
            .all(|u| u.starts_with(URL))
    );
}

struct Isolated(Applying);

impl ForgejoTransport for Isolated {
    fn get(&self, url: &url::Url) -> Result<Response> {
        self.0.check_url(url);
        let path = url.path();
        if path.ends_with("/users/example/repos") {
            return Ok(Response {
                body: json!([
                {"full_name":"Example/Work"},{"full_name":"example/missing"},
                {"full_name":"example/denied"},{"full_name":"example/long"},
                {"full_name":"outside/ignored"}]),
                total: Some(5),
            });
        }
        if path.contains("/repos/example/missing/") {
            return Err(crate::forgejo::NotFound.into());
        }
        if path.contains("/repos/example/denied/") {
            return Err(crate::forgejo::AuthenticationRequired("TEST_TOKEN".into()).into());
        }
        if path.contains("/repos/example/long/") {
            return Ok(Response {
                body: json!([{"id":1}]),
                total: Some(999),
            });
        }
        let mut canonical = url.clone();
        canonical.set_path(&path.replace("Example/Work", "example/work"));
        self.0.get(&canonical)
    }

    fn send(&self, method: &str, url: &url::Url, body: &Value) -> Result<(u16, Value)> {
        self.0.send(method, url, body)
    }

    fn write(&self, method: &str, url: &url::Url, body: &Value) -> Result<Value> {
        self.0.write(method, url, body)
    }
}

#[test]
fn owner_expansion_isolates_denied_missing_and_incomplete_repositories() {
    let dir = Directory::new();
    let (fake, mut value) = setup(&dir);
    value["board"]["intake"]["repositories"] = json!(["example/*"]);
    let fake = Isolated(fake);
    let cfg = config(&value);
    let report = run(
        &fake,
        &BrokerFake::default(),
        &cfg,
        (Mode::Local, Limits::default()),
        usage::tests::now(),
    )
    .unwrap();
    assert_eq!(fake.0.posts("/dispatches"), 1, "{:?}", report.lines);
    assert_eq!(report.failures, 3, "{:?}", report.lines);
    let state: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("cursor.json")).unwrap()).unwrap();
    assert_eq!(
        state["repositories"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["example/work"]
    );
    for repository in ["denied", "missing", "long"] {
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.starts_with(&format!("intake example/{repository} error:"))),
            "{:?}",
            report.lines
        );
    }
    let broker = BrokerFake::default();
    let recorder = Recorder::new(&fake, &broker, cfg.forgejo_board().unwrap());
    run(
        &recorder,
        &recorder,
        &cfg,
        (Mode::Preview, Limits::default()),
        usage::tests::now(),
    )
    .unwrap();
    let fixture: Value =
        serde_json::from_str(&recorder.fixture(usage::tests::now()).unwrap()).unwrap();
    let replay = Replay::from_json(&fixture, cfg.forgejo_board().unwrap()).unwrap();
    run(
        &replay,
        &replay,
        &cfg,
        (Mode::Preview, Limits::default()),
        replay.now,
    )
    .unwrap();
    replay.finish().unwrap();
    assert!(!fixture["requests"].as_array().unwrap().iter().any(|r| {
        r["request"]["get"]
            .as_str()
            .is_some_and(|u| u.contains("outside/ignored"))
    }));
}
