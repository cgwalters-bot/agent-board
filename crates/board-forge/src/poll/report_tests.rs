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
fn maintainer_report_publication_requires_every_followed_repository_to_be_public() {
    const OTHER: &str = "cgwalters-forge/zzz";
    const WATCH: &str = "cgwalters-forge/watch-only";
    let follow = [REPO.to_owned(), OTHER.to_owned()];
    let watch = [WATCH.to_owned()];
    let mut c = maintainer_config();
    c.policy.maintainer_report.as_mut().unwrap().repository = REPO.into();
    c.policy.repositories_per_pass = Some(1);
    let path = format!("repos/{OTHER}");
    let apply = |fake: &Fake, c: &Config| {
        run_capped(
            fake, fake, c, State::default(), (&watch, &follow), applying(fake), None, now(),
        )
        .unwrap()
    };
    let mut baseline = config();
    baseline.policy.repositories_per_pass = Some(1);
    let public = json!({"private": false, "visibility": "public"});
    for (name, source) in [
        ("public", Some(public.clone())),
        ("private", Some(json!({"private":true, "visibility":"private"}))),
        ("unread", None),
        ("missing visibility", Some(json!({"private":false}))),
        ("internal", Some(json!({"private":false, "visibility":"internal"}))),
        ("contradictory", Some(json!({"private":true, "visibility":"public"}))),
    ] {
        let mut fake = github();
        fake.pulls.get_mut(&470).unwrap()["reviews"]["nodes"] = json!([]);
        let issue = format!("repos/{REPO}/issues/1356");
        fake.page(&format!("repos/{REPO}"), None, None, public.clone());
        fake.page(&issue, None, None, json!({}));
        fake.page(&format!("{issue}/comments?per_page=100"), None, None, json!([]));
        // The envelope does not visit this repository's PRs, but the report names it.
        fake.page(&format!("{path}/events?per_page=100"), None, None, json!([]));
        fake.page(&format!("repos/{WATCH}/events?per_page=100"), None, None, json!([]));
        fake.page(&format!("repos/{WATCH}"), None, None, json!({"private":true,"visibility":"private"}));
        if let Some(source) = source {
            fake.page(&path, None, None, source);
        } else {
            fake.failing.push(format!("{API_ROOT}{path}"));
        }
        let expected = apply(&fake, &baseline);
        assert_eq!(expected.errors, 0, "{:?}", expected.lines);
        assert!(!expected.steps.is_empty());
        fake.sent.borrow_mut().clear();
        let actual = apply(&fake, &c);
        assert_eq!(actual.steps, expected.steps, "{name}");
        assert_eq!(actual.waits, expected.waits, "{name}");
        assert!(actual.undecided.contains(OTHER));
        let reads = fake.sent.borrow();
        assert!(!reads.iter().any(|(url, _)| *url == format!("{API_ROOT}repos/{WATCH}")));
        for repo in &follow {
            let url = format!("{API_ROOT}repos/{repo}");
            assert_eq!(reads.iter().filter(|(read, _)| *read == url).count(), 1, "{name}: {repo}");
        }
        let writes = fake.writes.borrow();
        if name == "public" {
            assert_eq!(actual.errors, 0, "{:?}", actual.lines);
            assert_eq!(writes.len(), 1);
            assert_eq!(writes[0].0, format!("POST {API_ROOT}{issue}/comments"));
            let snapshot = report_json(text(&writes[0].1, "/body"));
            assert_eq!(snapshot["repositories"], json!(follow));
        } else {
            assert_eq!(actual.errors, 1, "{name}: {:?}", actual.lines);
            assert!(
                actual.lines.iter().any(|line| line.contains("maintainer report") && line.contains(OTHER)),
                "{name}: {:?}", actual.lines
            );
            assert!(writes.is_empty(), "{name}: {writes:?}");
        }
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

    let c = maintainer_config();
    let destination = &c.policy.maintainer_report.as_ref().unwrap().repository;
    let issue = format!("repos/{destination}/issues/1356");
    let mut fake = github();
    for repo in [destination.as_str(), REPO] {
        fake.page(&format!("repos/{repo}"), None, None, json!({"private":false,"visibility":"public"}));
    }
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

    // Exactly enough for both visibility reads and publication, but not the queue.
    let shared = SharedReads(&fake, Cell::new(4));
    let (standalone, remaining) = report(&fake, &c, 5);
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
}

#[test]
fn maintainer_report_configuration_rejects_invalid_targets() {
    for target in [json!(null), json!({}), json!({"repository":"o/r","issue":0}),
        json!({"repository":"../r","issue":1}), json!({"repository":"o/r","issue":1,"extra":true})] {
        assert!(PollPolicy::from_json(&json!({"maintainerReport":target})).is_err());
    }
}
