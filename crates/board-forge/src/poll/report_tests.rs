fn maintainer_config() -> Config {
    let mut c = config();
    c.policy.maintainer_report = PollPolicy::from_json(&json!({
        "maintainerReport": {"repository": "cgwalters-forge/tracker", "issue": 1356}
    })).unwrap().maintainer_report;
    c
}

fn report_json(body: &str) -> Value {
    let start = format!("```{}\n", report::SCHEMA);
    serde_json::from_str(body.split_once(&start).unwrap().1.split_once("\n```").unwrap().0).unwrap()
}

fn public_queue_json(body: &str) -> Value {
    let start = "```agent-board/public-merge-queues/v1\n";
    serde_json::from_str(body.split_once(start).unwrap().1.split_once("\n```").unwrap().0).unwrap()
}

#[test]
fn maintainer_report_fixture_preserves_poll_decisions() {
    let baseline_fake = github();
    let baseline = configured_pass(&baseline_fake, &config(), State::default(), &[], &[REPO]);
    let fake = github();
    let outcome = configured_pass(&fake, &maintainer_config(), State::default(), &[], &[REPO]);
    assert_eq!(outcome.steps, baseline.steps);
    assert_eq!(outcome.waits, baseline.waits);
    assert_eq!(outcome.lines, baseline.lines);
    assert_eq!(*fake.sent.borrow(), *baseline_fake.sent.borrow());
    let actual = report_json(outcome.maintainer_report.as_ref().unwrap());
    let expected: Value = serde_json::from_str(include_str!("../../../../fixtures/poll/maintainer-report.json")).unwrap();
    assert_eq!(actual, expected);
    assert!(baseline.maintainer_report.is_none());
}

#[test]
fn maintainer_report_includes_queue_events_and_human_waits() {
    for (patch, action, human) in [
        (json!({"labels":{"nodes":[{"name":LABEL}]}}), "skip", true),
        (json!({}), "merge", false),
    ] {
        let mut fake = github();
        let mut p = pull("pull-470", patch);
        p["timelineItems"] = json!({"nodes":[{"id":"event-1", "createdAt":"2026-10-12T01:00:00Z", "reason":"stack_invalidated"}]});
        fake.pulls.insert(470, p);
        let outcome = configured_pass(&fake, &maintainer_config(), State::default(), &[], &[REPO]);
        let snapshot = report_json(outcome.maintainer_report.as_ref().unwrap());
        // Labelled skips may use the separate labelled-list observation rather
        // than reading this PR's timeline. Never fabricate an event in that case.
        if action == "merge" {
            assert_eq!(snapshot["merge_queue_events"][0]["event"]["id"], "event-1");
            assert_eq!(snapshot["merge_queue_events"][0]["step"]["action"], action);
        }
        assert_eq!(!snapshot["needs_human"].as_array().unwrap().is_empty(), human);
    }
}

#[test]
fn maintainer_report_reads_each_merge_queue_without_changing_decisions() {
    let entry = json!({"position": 1, "state": "AWAITING_CHECKS",
        "headCommit": {"oid": "abc"}, "pullRequest": {"number": 470}});
    let queue = |nodes: Value, more: bool| {
        Some(json!({"entries": {"nodes": nodes, "pageInfo": {"hasNextPage": more}}}))
    };
    let baseline = configured_pass(&github(), &config(), State::default(), &[], &[REPO]);
    // No merge queue, an empty one, one entry, more than are read, a failed read.
    for (queue, expected, errors) in [
        (None, json!([]), 0),
        (queue(json!([]), false), json!([]), 0),
        (
            queue(json!([entry]), false),
            json!([{"number": 470, "head": "abc", "position": 1, "state": "AWAITING_CHECKS"}]),
            0,
        ),
        (queue(json!([entry]), true), json!(null), 1),
        (Some(json!(null)), json!(null), 1),
    ] {
        let mut fake = github();
        fake.report_queue = queue;
        let outcome = configured_pass(&fake, &maintainer_config(), State::default(), &[], &[REPO]);
        assert_eq!(outcome.steps, baseline.steps);
        assert_eq!(outcome.errors, baseline.errors + errors, "{:?}", outcome.lines);
        let snapshot = report_json(outcome.maintainer_report.as_ref().unwrap());
        assert_eq!(snapshot["merge_queues"], json!({REPO: expected}));
        assert_eq!(snapshot["errors"], errors);
    }
}

#[test]
fn maintainer_report_creates_edits_and_leaves_unchanged() {
    let c = maintainer_config();
    let repo = "cgwalters-forge/tracker";
    let path = format!("repos/{repo}/issues/1356/comments?per_page=100");
    let mut published = String::new();
    for kind in ["create", "unchanged", "edit", "foreign", "pr", "read-error", "cap"] {
        let mut fake = github();
        // Keep this fixture's PR read-only so every write is the report's.
        fake.pulls.get_mut(&470).unwrap()["reviews"]["nodes"] = json!([]);
        fake.page(&format!("repos/{repo}"), None, None, json!({"private":true}));
        fake.page(
            &format!("repos/{REPO}"),
            None,
            None,
            json!({"private":true, "visibility":"private"}),
        );
        fake.page(&format!("repos/{repo}/issues/1356"), None, None,
            if kind == "pr" { json!({"pull_request":{}}) } else { json!({}) });
        let comments = match kind {
            "unchanged" => json!([{"id":42,"user":{"login":BOT},"body":published}]),
            "edit" => json!([{"id":42,"user":{"login":BOT},"body":format!("{}\nold",report::MARKER)}]),
            "foreign" => json!([{"id":42,"user":{"login":"someone"},"body":published}]),
            _ => json!([]),
        };
        if kind != "read-error" { fake.page(&path, None, None, comments); }
        let caps = caps::Caps::new(caps::Limits { writes: Some(0), ..Default::default() });
        let outcome = run_capped(&fake, &fake, &c, State::default(), (&[], &[REPO.into()]),
            Some(Apply { dispatcher:&fake, workflow:"dispatch.yml".into(), pools:vec![], git_ref:None }),
            (kind == "cap").then_some(&caps), now()).unwrap();
        let writes = fake.writes.borrow();
        let followed_metadata = format!("{API_ROOT}repos/{REPO}");
        assert!(!fake.sent.borrow().iter().any(|(url, _)| *url == followed_metadata));
        match kind {
            "create" | "foreign" | "edit" => {
                assert_eq!(outcome.errors, 0, "{kind}: {:?}", outcome.lines);
                assert_eq!(writes.len(), 1, "{kind}: {writes:?}");
                assert!(writes[0].0.starts_with(if kind == "edit" { "PATCH " } else { "POST " }));
                published = text(&writes[0].1, "/body").into();
                assert_eq!(report_json(&published)["mode"], "apply");
            }
            "unchanged" | "cap" => {
                assert_eq!(outcome.errors, 0, "{kind}: {:?}", outcome.lines);
                assert!(writes.is_empty(), "{kind}: {writes:?}");
            }
            _ => { assert_eq!(outcome.errors, 1, "{kind}: {:?}", outcome.lines); assert!(writes.is_empty()); }
        }
    }
}

#[test]
fn public_maintainer_report_does_not_publish_private_poll_evidence() {
    const OUTAGE: &str = "cgwalters-forge/private-outages";
    const WATCH: &str = "cgwalters-forge/private-watch";
    let mut fake = github();
    let mut c = maintainer_config();
    c.policy.maintainer_report.as_mut().unwrap().repository = REPO.into();
    c.policy.broker_outage = PollPolicy::from_json(&json!({"brokerOutage": {
        "repository": OUTAGE, "author": BOT, "leadSeconds": 0
    }})).unwrap().broker_outage;
    fake.pulls.insert(470, pull("pull-470", json!({"mergeable": "CONFLICTING"})));
    fake.page(&format!("repos/{REPO}/issues/470/comments?per_page=100"), None, None, json!([
        {"id": 5, "user": {"login": BOT}, "body": marker_body(&json!({
            "rounds": 1, "sha": HEAD, "run": 7, "at": "2026-10-12T01:00:00Z"
        }), "Fix round.")}
    ]));
    fake.page(&format!("repos/{}/actions/runs/7", c.dispatch_repo), None, None, json!({
        "status": "completed", "conclusion": "failure", "updated_at": "2026-10-12T01:55:00Z"
    }));
    let issues = format!("repos/{OUTAGE}/issues?labels=outage&per_page=30");
    fake.page(&format!("{issues}&state=open"), None, None, json!([{
        "number": 9, "state": "open", "created_at": "2026-10-12T00:00:00Z",
        "user": {"login": BOT}, "body": OUTAGE_MARKER
    }]));
    fake.page(&format!("{issues}&state=closed&since=2026-10-10T02:00:00Z"), None, None, json!([]));
    fake.failing.push(events_url(WATCH));
    let issue = format!("repos/{REPO}/issues/1356");
    fake.page(&issue, None, None, json!({}));
    fake.page(&format!("{issue}/comments?per_page=100"), None, None, json!([]));
    let run = |fake: &Fake, c: &Config| run_capped(
        fake, fake, c, State::default(), (&[WATCH.into()], &[REPO.into()]),
        applying(fake), None, now(),
    ).unwrap();
    fake.page(&format!("repos/{REPO}"), None, None, json!({"private": true}));
    let private = run(&fake, &c);
    let publish = format!("POST {API_ROOT}{issue}/comments");
    let body = |fake: &Fake| {
        let writes = fake.writes.borrow();
        assert_eq!(writes.len(), 1, "{writes:?}");
        assert_eq!(writes[0].0, publish);
        text(&writes[0].1, "/body").to_owned()
    };
    let private_body = report_json(&body(&fake));
    let reason = text(&private_body, "/pull_requests/wait/0/reason");
    assert!(reason.contains(OUTAGE) && reason.contains("actions/runs/7 ended (failure)"), "{reason}");
    assert_eq!(private_body["errors"], 1);
    fake.writes.borrow_mut().clear();
    fake.page(&format!("repos/{REPO}"), None, None, json!({"private": false, "visibility": "public"}));
    fake.report_visibility.insert(REPO.into(), json!("PUBLIC"));
    fake.report_queue = Some(json!({"entries": {"nodes": [{"position": 1, "state": "QUEUED",
        "headCommit": {"oid": HEAD}, "pullRequest": {"number": 470}}], "pageInfo": {"hasNextPage": false}}}));
    c.policy.maintainer_report.as_mut().unwrap().public_queues = true;
    let public = run(&fake, &c);
    assert_eq!(public.steps, private.steps);
    assert_eq!(public.waits, private.waits);
    assert_eq!(public.errors, private.errors);
    let public_body = body(&fake);
    assert!(!public_body.contains(OUTAGE), "public report exposed private outage: {public_body}");
    assert_eq!(public_queue_json(&public_body), json!({
        "schema": "agent-board/public-merge-queues/v1",
        "generated_at": "2026-10-12T02:00:00Z",
        "merge_queues": { REPO: [{"number": 470, "head": HEAD, "position": 1, "state": "QUEUED"}] },
    }));
    for (nodes, more) in [(json!([]), true), (json!([null]), false)] {
        fake.writes.borrow_mut().clear();
        fake.report_queue = Some(json!({"entries": {"nodes": nodes, "pageInfo": {"hasNextPage": more}}}));
        let refused = run(&fake, &c);
        assert_eq!(refused.steps, private.steps);
        assert_eq!(refused.errors, private.errors + 1, "{:?}", refused.lines);
        assert!(fake.writes.borrow().is_empty());
    }
}

#[test]
fn maintainer_report_publication_requires_every_followed_repository_to_be_public() {
    const OTHER: &str = "cgwalters-forge/zzz";
    const WATCH: &str = "cgwalters-forge/watch-only";
    let follow = [REPO.to_owned(), OTHER.to_owned()];
    let watch = [WATCH.to_owned()];
    let mut c = maintainer_config();
    c.policy.maintainer_report.as_mut().unwrap().repository = REPO.into();
    c.policy.maintainer_report.as_mut().unwrap().public_queues = true;
    c.policy.repositories_per_pass = Some(1);
    let apply = |fake: &Fake, c: &Config| {
        run_capped(
            fake, fake, c, State::default(), (&watch, &follow), applying(fake), None, now(),
        )
        .unwrap()
    };
    let mut baseline = config();
    baseline.policy.repositories_per_pass = Some(1);
    let public = json!({"private": false, "visibility": "public"});
    for (name, followed, other) in [
        ("public", "PUBLIC", Some("PUBLIC")),
        ("private", "PUBLIC", Some("PRIVATE")),
        ("missing visibility", "PUBLIC", None),
        ("internal", "PUBLIC", Some("INTERNAL")),
        ("destination's queue visibility changed", "PRIVATE", Some("PUBLIC")),
    ] {
        let mut fake = github();
        fake.pulls.get_mut(&470).unwrap()["reviews"]["nodes"] = json!([]);
        let issue = format!("repos/{REPO}/issues/1356");
        fake.page(&format!("repos/{REPO}"), None, None, public.clone());
        fake.page(&issue, None, None, json!({}));
        fake.page(&format!("{issue}/comments?per_page=100"), None, None, json!([]));
        // The envelope does not visit OTHER's PRs, but the public inventory includes it.
        fake.page(&format!("repos/{OTHER}/events?per_page=100"), None, None, json!([]));
        fake.page(&format!("repos/{WATCH}/events?per_page=100"), None, None, json!([]));
        fake.report_visibility.insert(REPO.into(), json!(followed));
        if let Some(other) = other {
            fake.report_visibility.insert(OTHER.into(), json!(other));
        }
        let expected = apply(&fake, &baseline);
        assert_eq!(expected.errors, 0, "{:?}", expected.lines);
        assert!(!expected.steps.is_empty());
        fake.sent.borrow_mut().clear();
        let actual = apply(&fake, &c);
        assert_eq!(actual.steps, expected.steps, "{name}");
        assert_eq!(actual.waits, expected.waits, "{name}");
        assert!(actual.undecided.contains(OTHER));
        let writes = fake.writes.borrow();
        if name == "public" {
            assert_eq!(actual.errors, 0, "{:?}", actual.lines);
            assert_eq!(writes.len(), 1);
            assert_eq!(writes[0].0, format!("POST {API_ROOT}{issue}/comments"));
            let snapshot = public_queue_json(text(&writes[0].1, "/body"));
            assert_eq!(snapshot["merge_queues"], json!({REPO: [], OTHER: []}));
        } else {
            assert_eq!(actual.errors, 1, "{name}: {:?}", actual.lines);
            let repo = if followed == "PUBLIC" { OTHER } else { REPO };
            assert!(
                actual.lines.iter().any(|line| line.contains("maintainer report") && line.contains(repo)),
                "{name}: {:?}", actual.lines
            );
            assert!(writes.is_empty(), "{name}: {writes:?}");
        }
    }
}

#[test]
fn public_maintainer_report_is_independent_of_hold_dependencies() {
    // A dependency outside the follow set, possibly private: its state decides
    // the public PR's action, so a public report would disclose it.
    for (state, action) in [("open", "blocked"), ("closed", "needs-human")] {
        let (mut fake, mut cfg) = dependency_fixture();
        cfg.policy.maintainer_report = maintainer_config().policy.maintainer_report;
        cfg.policy.maintainer_report.as_mut().unwrap().repository = REPO.into();
        fake.pulls.get_mut(&470).unwrap()["isDraft"] = true.into();
        fake.page("repos/other/repo/issues/12", None, None, json!({"number":12,"state":state,"pull_request":{}}));
        fake.page("repos/other/repo/pulls/12", None, None, json!({"number":12,"state":state,"merged":false}));
        dependency_record(&mut fake, json!({"update_head":"","released":false,"reason":"blocked by other/repo#12"}));
        let issue = format!("repos/{REPO}/issues/1356");
        let post = format!("POST {API_ROOT}{issue}/comments");
        fake.page(&issue, None, None, json!({}));
        fake.page(&format!("{issue}/comments?per_page=100"), None, None, json!([]));
        fake.report_visibility.insert(REPO.into(), json!("PUBLIC"));
        let mut decided = Vec::new();
        for (visibility, public) in [("private", false), ("public", true)] {
            cfg.policy.maintainer_report.as_mut().unwrap().public_queues = public;
            fake.page(&format!("repos/{REPO}"), None, None,
                json!({"private": !public, "visibility": visibility}));
            fake.writes.borrow_mut().clear();
            let out = dependency_pass(&fake, &cfg, true, None);
            assert!(out.steps.iter().any(|s| s.action == action), "{state} {visibility}: {:?}", out.steps);
            decided.push((out.steps.clone(), out.waits.clone()));
            let writes = fake.writes.borrow();
            let (_, body) = writes.iter().find(|(url, _)| *url == post).unwrap();
            assert_eq!(out.errors, 0, "{state} {visibility}: {:?}", out.lines);
            if public {
                assert_eq!(public_queue_json(text(body, "/body"))["merge_queues"], json!({REPO: []}));
            }
        }
        assert_eq!(decided[0], decided[1], "{state}");
    }
}

#[test]
fn maintainer_report_reserves_publication_requests_before_reading_queues() {
    // Model the hosted client's read share, which does not charge writes to
    // that share. Both REST and GraphQL reads use it.
    struct SharedReads<'a>(&'a Fake, Cell<usize>);

    impl Rest for SharedReads<'_> {
        fn share_left(&self) -> Option<usize> {
            Some(self.1.get())
        }

        fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
            self.charge()?;
            self.0.get(url, validator)
        }

        fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
            self.0.send(method, url, body)
        }
    }

    impl Transport for SharedReads<'_> {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            self.charge()?;
            self.0.graphql(query, variables)
        }
    }

    impl SharedReads<'_> {
        fn charge(&self) -> Result<()> {
            let left = self.1.get();
            ensure!(left > 0, "read share is spent");
            self.1.set(left - 1);
            Ok(())
        }
    }

    let mut c = maintainer_config();
    let destination = c.policy.maintainer_report.as_ref().unwrap().repository.clone();
    let issue = format!("repos/{destination}/issues/1356");
    let mut fake = github();
    fake.page(&format!("repos/{destination}"), None, None, json!({"private":true}));
    fake.page(&issue, None, None, json!({}));
    fake.page(&format!("{issue}/comments?per_page=100"), None, None, json!([]));
    fn report<R: Rest + Transport>(client: &R, c: &Config, remaining: usize) -> (Outcome, usize) {
        let mut pass = Pass::new(client, client, c, now());
        pass.apply = applying(client);
        pass.remaining = remaining;
        let mut outcome = Outcome::default();
        pass.maintainer_report(&BTreeSet::from([&REPO.to_owned()]), &mut outcome);
        (outcome, pass.remaining)
    }

    // Exactly enough for destination visibility and publication, but not the queue.
    let shared = SharedReads(&fake, Cell::new(3));
    let (standalone, remaining) = report(&fake, &c, 4);
    let (hosted, _) = report(&shared, &c, usize::MAX);
    assert_eq!(remaining, 0);
    assert_eq!(shared.1.get(), 0);
    for outcome in [standalone, hosted] {
        assert_eq!(outcome.errors, 1, "{:?}", outcome.lines);
        assert!(outcome.lines.iter().any(|line| line.contains("merge queue") && line.contains("request budget")));
    }
    let writes = fake.writes.borrow();
    assert_eq!(writes.len(), 2);
    for (path, body) in writes.iter() {
        assert_eq!(*path, format!("POST {API_ROOT}{issue}/comments"));
        assert_eq!(report_json(text(body, "/body"))["merge_queues"], json!({REPO: null}));
    }
    drop(writes);

    // The public format cannot be populated with a partial queue inventory.
    fake.writes.borrow_mut().clear();
    c.policy.maintainer_report.as_mut().unwrap().public_queues = true;
    fake.page(&format!("repos/{destination}"), None, None, json!({"private":false,"visibility":"public"}));
    fake.report_visibility.insert(REPO.into(), json!("PUBLIC"));
    let shared = SharedReads(&fake, Cell::new(3));
    let (public, _) = report(&shared, &c, usize::MAX);
    assert_eq!(public.errors, 1, "{:?}", public.lines);
    assert!(fake.writes.borrow().is_empty());
}

#[test]
fn maintainer_report_configuration_rejects_invalid_targets() {
    for target in [json!(null), json!({}), json!({"repository":"o/r","issue":0}),
        json!({"repository":"../r","issue":1}), json!({"repository":"o/r","issue":1,"extra":true}),
        json!({"repository":"o/r","issue":1,"publicQueues":null})] {
        assert!(PollPolicy::from_json(&json!({"maintainerReport":target})).is_err());
    }
    let policy = PollPolicy::from_json(&json!({"maintainerReport": {
        "repository": "o/r", "issue": 1, "publicQueues": true
    }})).unwrap();
    assert!(policy.maintainer_report.unwrap().public_queues);
}

#[test]
fn maintainer_report_refuses_destination_visibility_changes() {
    struct ChangingDestination<'a> {
        fake: &'a Fake,
        url: String,
        public_before_queue_read: bool,
        read_queue: Cell<bool>,
    }

    impl Rest for ChangingDestination<'_> {
        fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
            let mut response = self.fake.get(url, validator)?;
            if url == self.url {
                assert!(validator.is_none());
                let public = self.public_before_queue_read != self.read_queue.get();
                response.body = json!({"private": !public, "visibility": if public { "public" } else { "private" }});
            }
            Ok(response)
        }

        fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
            self.fake.send(method, url, body)
        }
    }

    impl Transport for ChangingDestination<'_> {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            if query == report::QUEUE_QUERY {
                self.read_queue.set(true);
            }
            self.fake.graphql(query, variables)
        }
    }

    for public_queues in [false, true] {
        let mut fake = github();
        fake.pulls.get_mut(&470).unwrap()["reviews"]["nodes"] = json!([]);
        fake.report_visibility.insert(REPO.into(), json!("PUBLIC"));
        let mut c = maintainer_config();
        let target = c.policy.maintainer_report.as_mut().unwrap();
        target.public_queues = public_queues;
        let repo = format!("repos/{}", target.repository);
        let issue = format!("{repo}/issues/{}", target.issue);
        fake.page(&repo, None, None, json!({}));
        fake.page(&issue, None, None, json!({}));
        fake.page(&format!("{issue}/comments?per_page=100"), None, None, json!([]));
        let client = ChangingDestination {
            fake: &fake, url: format!("{API_ROOT}{repo}"),
            public_before_queue_read: public_queues, read_queue: Cell::new(false),
        };
        let outcome = run_capped(&client, &client, &c, State::default(), (&[], &[REPO.into()]),
            applying(&client), None, now()).unwrap();
        assert!(client.read_queue.get());
        assert!(fake.writes.borrow().is_empty());
        assert_eq!(outcome.errors, 1, "{:?}", outcome.lines);
        let expected = if public_queues { "public" } else { "private" };
        assert!(outcome.lines.iter().any(|line| line.contains(&format!("maintainer report requires a {expected} destination"))));
    }
}
