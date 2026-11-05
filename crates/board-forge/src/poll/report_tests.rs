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
    let baseline = configured_pass(&github(), &config(), State::default(), &[], &[REPO]);
    let outcome = configured_pass(&github(), &maintainer_config(), State::default(), &[], &[REPO]);
    assert_eq!(outcome.steps, baseline.steps);
    assert_eq!(outcome.waits, baseline.waits);
    assert_eq!(outcome.lines, baseline.lines);
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
    for kind in ["create", "unchanged", "edit", "foreign", "public", "pr", "read-error", "cap"] {
        let mut fake = github();
        // Keep this fixture's PR read-only so every write is the report's.
        fake.pulls.get_mut(&470).unwrap()["reviews"]["nodes"] = json!([]);
        fake.page(&format!("repos/{repo}"), None, None, json!({"private":kind != "public"}));
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
fn maintainer_report_configuration_rejects_invalid_targets() {
    for target in [json!(null), json!({}), json!({"repository":"o/r","issue":0}),
        json!({"repository":"../r","issue":1}), json!({"repository":"o/r","issue":1,"extra":true})] {
        assert!(PollPolicy::from_json(&json!({"maintainerReport":target})).is_err());
    }
}
