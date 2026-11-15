fn recovery_policy() -> PollPolicy {
    let mut policy = PollPolicy::from_json(&json!({"agentRecovery": true})).unwrap();
    policy.assign_holds = true;
    policy
}

#[test]
fn open_pr_cap_keeps_landing_work_and_serializes_oldest_paths() {
    for cap in [None, Some(1), Some(2), Some(3)] {
        for shared in [false, true] {
            for (change, action) in [(json!({}), "merge"), (json!({"contexts":{"ci":{"conclusion":"FAILURE"}}}), "fix"), (json!({"mergeable":CONFLICTING}), "rebase")] {
                let mut fake = github();
                fake.page(&format!("repos/{REPO}/pulls?state=open&per_page=100"), None, None,
                    json!([{"number":471,"user":{"login":BOT}}, {"number":470,"user":{"login":BOT}}]));
                fake.page(&format!("repos/{REPO}/pulls/470/files?per_page=100"), None, None, json!([{"filename":"src/a.rs"}]));
                let files = |path: &str| json!({"nodes":[{"path":path}],"pageInfo":{"hasNextPage":false}});
                let mut later = pull("pull-470", change.clone());
                later["number"] = 471.into();
                later["files"] = files(if shared { "src/a.rs" } else { "src/b.rs" });
                fake.pulls.insert(471, later);
                fake.page(&format!("repos/{REPO}/issues/471/comments?per_page=100"), None, None, json!([]));
                let policy = PollPolicy { open_pr_cap:cap, rebase_rounds:2, ..PollPolicy::default() };
                let mut first = change;
                first["files"] = files("src/a.rs");
                let (out, _, _) = applied_policy(fake, policy, first, json!({"rounds":0}));
                assert_eq!(out.errors, 0, "{cap:?}/{shared}/{action}: {:?}", out.lines);
                assert_eq!(out.steps[0].number, 470);
                assert_eq!(out.steps[0].action, action, "{cap:?}/{shared}: {:?}", out.lines);
                let held = shared && cap.is_some_and(|cap| cap <= 2);
                assert_eq!(out.steps[1].action, if held { "wait" } else { action }, "{cap:?}/{shared}/{action}: {:?}", out.lines);
                if held { assert!(out.steps[1].reason.contains("open-pr-cap: land")); }
            }
        }
    }
}

#[test]
fn open_pr_cap_preserves_cursor_progress_past_parked_older_work() {
    let mut fake = github();
    fake.page(&format!("repos/{REPO}/pulls?state=open&per_page=100"), None, None, json!([{"number":470,"user":{"login":BOT}}, {"number":471,"user":{"login":BOT}}]));
    fake.pulls.insert(470, pull("pull-470", json!({"labels":{"nodes":[{"name":"Hold"}],"pageInfo":{"hasNextPage":false}}})));
    let mut later = pull("pull-470", json!({"files":{"nodes":[{"path":"other.rs"}],"pageInfo":{"hasNextPage":false}}}));
    later["number"] = 471.into();
    fake.pulls.insert(471, later);
    fake.page(&format!("repos/{REPO}/issues/471/comments?per_page=100"), None, None, json!([]));
    fake.page(&format!("repos/{REPO}/pulls/470/files?per_page=100"), None, None, json!([{"filename":"src/a.rs"}]));
    let mut cfg = config();
    cfg.policy.open_pr_cap = Some(2);
    let turn = |state| {
        let shared = Shared::new(&fake, 2);
        run(&shared, &fake, &cfg, state, &[], &[REPO.into()], Some(Apply {dispatcher:&shared, workflow:"dispatch.yml".into(), pools:vec!["claude".into()], git_ref:None}), now()).unwrap()
    };
    let first = turn(State::default());
    assert_eq!(first.errors, 0, "{:?}", first.lines);
    assert_eq!(first.state.share_cursor, Some(format!("{REPO}#471")));
    let second = turn(State::from_json(&first.state.to_json()).unwrap());
    assert_eq!(second.errors, 0, "{:?}", second.lines);
    assert!(second.steps.iter().any(|s| s.number == 471 && s.action == "merge"), "{:?}", second.lines);
}

#[test]
fn open_pr_cap_reuses_repository_and_path_reads() {
    let mut fake = github();
    fake.page(&format!("repos/{REPO}/pulls?state=open&per_page=100"), None, None, json!((470..=472).map(|number| json!({"number":number,"user":{"login":BOT}})).collect::<Vec<_>>()));
    for number in [470, 471] {
        fake.page(&format!("repos/{REPO}/pulls/{number}/files?per_page=100"), None, None, json!([{"filename":format!("{number}.rs")} ]));
    }
    let mut cfg = config();
    cfg.policy.open_pr_cap = Some(2);
    let mut pass = Pass::new(&fake, &fake, &cfg, now());
    for number in [472, 471, 472] {
        let mut p = pull("pull-470", json!({"files":{"nodes":[{"path":"disjoint.rs"}],"pageInfo":{"hasNextPage":false}}}));
        p["number"] = number.into();
        assert_eq!(pass.pressure_ahead(REPO, &p).unwrap(), None);
    }
    assert_eq!(fake.sent.borrow().len(), 3, "one census and two file lists: {:?}", fake.sent.borrow());
}

#[test]
fn open_pr_cap_skips_parked_older_work() {
    let stop = |stopped: bool| json!([{"id":5, "user":{"login":BOT}, "body":marker_body(&json!({"rounds":2, "stopped":stopped}), "Stopped")}]);
    let operator = json!([{"login":config().operator}]);
    let label = |name: &str| json!([{"name":name}]);
    // Reads: the census, then the stop's comments, then the older PR's files.
    for (name, older, comments, assign_holds, held, reads) in [
        ("unparked", json!({}), None, true, true, 2),
        ("hold", json!({"labels":label("Hold")}), None, true, false, 1),
        ("needs-human", json!({"labels":label(LABEL)}), None, true, false, 1),
        ("other label", json!({"labels":label("bug")}), None, true, true, 2),
        ("stop held by review", json!({"requested_reviewers":operator}), Some(stop(true)), true, false, 2),
        ("review without a stop", json!({"requested_reviewers":operator}), Some(stop(false)), true, true, 3),
        ("review holds off", json!({"requested_reviewers":operator}), Some(stop(true)), false, true, 2),
        ("stop released", json!({}), Some(stop(true)), true, true, 2),
        // The fixture's comments list fails on its second page.
        ("unreadable stop", json!({"requested_reviewers":operator}), None, true, true, 4),
    ] {
        let mut fake = github();
        let mut first = older;
        first["number"] = 470.into();
        first["user"] = json!({"login":BOT});
        fake.page(&format!("repos/{REPO}/pulls?state=open&per_page=100"), None, None, json!([first, {"number":471,"user":{"login":BOT}}]));
        fake.page(&format!("repos/{REPO}/pulls/470/files?per_page=100"), None, None, json!([{"filename":"src/a.rs"}]));
        if let Some(comments) = comments {
            fake.page(&format!("repos/{REPO}/issues/470/comments?per_page=100"), None, None, comments);
        }
        let mut cfg = config();
        cfg.policy.open_pr_cap = Some(2);
        cfg.policy.assign_holds = assign_holds;
        let mut pass = Pass::new(&fake, &fake, &cfg, now());
        let mut p = pull("pull-470", json!({"files":{"nodes":[{"path":"src/a.rs"}],"pageInfo":{"hasNextPage":false}}}));
        p["number"] = 471.into();
        assert_eq!(pass.pressure_ahead(REPO, &p).unwrap(), held.then_some(470), "{name}");
        assert_eq!(fake.sent.borrow().len(), reads, "{name}: {:?}", fake.sent.borrow());
    }
}

fn assert_no_operator_park(fake: &Fake, dispatcher: &Fake) {
    for (url, body) in fake.writes.borrow().iter().chain(dispatcher.writes.borrow().iter()) {
        assert!(!url.contains("requested_reviewers"), "{url}");
        assert!(!url.ends_with("/labels"), "{url}: {body}");
        assert!(!body.to_string().contains("\"stopped\":true"), "{body}");
    }
}

#[test]
fn recovery_exhausted_rounds_dispatch_triage_without_operator_parking() {
    for (name, change, state) in [
        ("fix", json!({"contexts": {"ci": {"conclusion": "FAILURE"}}}), json!({"rounds": 2, "sha": "older"})),
        ("continuation", json!({"isDraft": true, "body": "```checkpoint\n{\"reason\":\"cap\",\"done\":\"half\",\"left\":\"rest\",\"next\":\"finish\"}\n```"}), json!({"rounds": 0, "continued": 3, "sha": "older"})),
        ("rebase", json!({"mergeable": CONFLICTING}), json!({"rounds": 0, "rebased": 2, "sha": "older"})),
        ("unfinished", json!({"isDraft": true, "title": "[partial: incomplete] Work", "body": "**Partial work.**"}), json!({"rounds": 0, "continued": 3, "sha": "older"})),
    ] {
        let mut policy = recovery_policy();
        policy.rebase_rounds = 2;
        policy.unstall = true;
        let (out, fake, dispatcher) = applied_policy(github(), policy, change, state);
        assert_eq!(out.errors, 0, "{name}: {:?}", out.lines);
        assert_eq!(out.steps[0].action, "triage", "{name}: {:?}", out.lines);
        assert!(out.waits.is_empty(), "{name}");
        assert_no_operator_park(&fake, &dispatcher);
        let writes = dispatcher.writes.borrow();
        assert_eq!(writes.len(), 1, "{name}");
        assert_eq!(writes[0].1["inputs"]["kind"], "triage");
        assert!(writes[0].1["inputs"]["task"].as_str().unwrap().contains("independently checked"));
        let saved = fake.writes.borrow().iter().rev().find_map(|(_, b)| parse_marker(MARKER, text(b, "/body"))).unwrap();
        assert_eq!(saved["recovery"]["run"], 1234);
        let read = find_marker(&[json!({"id": 99, "user": {"login": BOT}, "body": marker_body(&saved, "Triage")})], BOT);
        assert!(read.recovery.is_some());
    }
}

#[test]
fn recovery_is_opt_in_and_changes_the_parking_symptom() {
    for enabled in [false, true] {
        let mut policy = recovery_policy();
        policy.agent_recovery = enabled;
        let (out, fake, dispatcher) = applied_policy(github(), policy,
            json!({"contexts": {"ci": {"conclusion": "FAILURE"}}}),
            json!({"rounds": 2, "sha": "older"}));
        assert_eq!(out.errors, 0, "{:?}", out.lines);
        let expected = if enabled { "triage" } else { "needs-human" };
        assert_eq!(out.steps[0].action, expected);
        println!("exhausted recorded-style PR: agentRecovery={enabled}: {expected}");
        if enabled { assert_no_operator_park(&fake, &dispatcher); }
        else { assert!(fake.writes.borrow().iter().any(|(url, _)| url.contains("requested_reviewers"))); }
    }
}

#[test]
fn recovery_queue_reason_table() {
    for (reason, tries, expected) in [
        ("merge_conflict", 0, "fix"),
        ("merge_conflict", 3, "fix"),
        ("stack_invalidated", 0, "merge"),
        ("stack_invalidated", 3, "triage"),
        ("failed_checks", 3, "triage"),
        ("checks_timed_out", 3, "triage"),
        ("merged", 3, "merge"),
    ] {
        let mut fake = github();
        fake.rest.get_mut(&format!("{API_ROOT}repos/{REPO}/rules/branches/main")).unwrap().3.as_array_mut().unwrap().push(json!({"type": "merge_queue"}));
        fake.page(&format!("repos/{REPO}/actions/runs?event=merge_group&per_page=100"), None, None, json!({"workflow_runs": []}));
        let mut policy = recovery_policy();
        policy.merge_queue = Some(3);
        let change = json!({"timelineItems": {"nodes": [{"id": "removal-new", "reason": reason, "createdAt": "2026-10-12T01:30:00Z", "beforeCommit": {"oid": HEAD}}]}});
        let state = json!({"rounds": 0, "queue": {"head": HEAD, "seen": "old", "tries": tries, "jobs": [], "reasons": [], "queued": HEAD}});
        let (out, fake, dispatcher) = applied_policy(fake, policy, change, state);
        assert_eq!(out.errors, 0, "{reason}: {:?}", out.lines);
        assert_eq!(out.steps[0].action, expected, "{reason}: {:?}", out.lines);
        assert_no_operator_park(&fake, &dispatcher);
        if reason == "merge_conflict" {
            let writes = dispatcher.writes.borrow();
            let task = writes[0].1["inputs"]["task"].as_str().unwrap();
            assert!(task.contains("rebase"), "{task}");
            assert!(!fake.writes.borrow().iter().any(|(url, _)| url.contains("enqueuePullRequest")));
        }
    }
}

#[test]
fn recovery_serializes_overlapping_queue_paths_but_not_disjoint_entries() {
    for (paths, expected) in [(Some(vec!["src/a.rs"]), "wait"), (Some(vec!["src/b.rs"]), "merge"), (None, "wait")] {
        let mut fake = github();
        fake.rest.get_mut(&format!("{API_ROOT}repos/{REPO}/rules/branches/main")).unwrap().3.as_array_mut().unwrap().push(json!({"type": "merge_queue"}));
        let files = |paths: Vec<&str>| json!({"nodes": paths.iter().map(|p| json!({"path": p})).collect::<Vec<_>>(), "pageInfo": {"hasNextPage": false}});
        fake.queue = vec![json!({"pullRequest": {"number": 471, "files": paths.map(files)}})];
        let mut policy = recovery_policy();
        policy.merge_queue = Some(3);
        let (out, fake, dispatcher) = applied_policy(fake, policy, json!({"files": files(vec!["src/a.rs"])}), json!({"rounds": 0}));
        assert_eq!(out.errors, 0, "{:?}", out.lines);
        assert_eq!(out.steps[0].action, expected, "{:?}", out.lines);
        assert_no_operator_park(&fake, &dispatcher);
    }
}

fn recovery_result_pass(mut fake: Fake, result: Value, state: Value, run_change: Value) -> (Outcome, Fake, Fake) {
    let mut cfg = config();
    cfg.policy = recovery_policy();
    let mut run_value = json!({"id": 1234, "repository": {"full_name": cfg.dispatch_repo}, "path": ".github/workflows/dispatch.yml", "event": DISPATCH_EVENT, "head_branch": DISPATCH_REF, "status": "completed", "conclusion": "success"});
    for (key, value) in run_change.as_object().unwrap() { run_value[key] = value.clone(); }
    fake.page(&format!("repos/{}/actions/runs/1234", cfg.dispatch_repo), None, None, run_value);
    let mut comments = vec![json!({"id": 5, "user": {"login": BOT}, "body": marker_body(&state, "Triage reserved")})];
    if !result.is_null() { comments.push(result); }
    fake.rest.get_mut(&format!("{API_ROOT}repos/{REPO}/issues/470/comments?per_page=100")).unwrap().3 = json!(comments);
    let dispatcher = Fake::default();
    let out = run(&fake, &fake, &cfg, State::default(), &[], &[REPO.into()], applying(&dispatcher), now()).unwrap();
    (out, fake, dispatcher)
}

#[test]
fn recovery_dispositions_and_negative_evidence_table() {
    let state = json!({"rounds": 2, "sha": "older", "recovery": {"head": HEAD, "run": 1234, "at": "2026-10-12T01:00:00Z", "granted": false, "pending": true, "attempts": 2}});
    for (decision, question, author, head, run, run_change, expected) in [
        ("more-rounds", Value::Null, BOT, HEAD, 1234, json!({}), "triage-grant"),
        ("obsolete", Value::Null, BOT, HEAD, 1234, json!({}), "wait"),
        ("replan", Value::Null, BOT, HEAD, 1234, json!({}), "wait"),
        ("architecture", json!("Which storage durability contract should this implement?"), BOT, HEAD, 1234, json!({}), "needs-human"),
        ("architecture", Value::Null, BOT, HEAD, 1234, json!({}), "wait"),
        ("more-rounds", Value::Null, "visitor", HEAD, 1234, json!({}), "wait"),
        ("more-rounds", Value::Null, BOT, "old", 1234, json!({}), "wait"),
        ("more-rounds", Value::Null, BOT, HEAD, 99, json!({}), "wait"),
        ("more-rounds", Value::Null, BOT, HEAD, 1234, json!({"conclusion": "failure"}), "wait"),
        ("more-rounds", Value::Null, BOT, HEAD, 1234, json!({"status": "in_progress"}), "wait"),
        ("more-rounds", Value::Null, BOT, HEAD, 1234, json!({"event": "pull_request"}), "wait"),
        ("more-rounds", Value::Null, BOT, HEAD, 1234, json!({"head_branch": "feature"}), "wait"),
        ("more-rounds", Value::Null, BOT, HEAD, 1234, json!({"repository": {"full_name": "other/repo"}}), "wait"),
        ("more-rounds", Value::Null, BOT, HEAD, 1234, json!({"path": ".github/workflows/other.yml"}), "wait"),
    ] {
        let body = format!("```agent-board-pr-triage\n{}\n```", json!({"head": head, "run": run, "decision": decision, "reason": "Examined the failed run: a fresh base fixes the cause", "question": question}));
        let result = json!({"id": 6, "user": {"login": author}, "body": body});
        let (out, fake, dispatcher) = recovery_result_pass(github(), result, state.clone(), run_change.clone());
        assert_eq!(out.errors, 0, "{decision} {run_change}: {:?}", out.lines);
        assert_eq!(out.steps[0].action, expected, "{decision} {run_change}: {:?}", out.lines);
        assert!(dispatcher.writes.borrow().is_empty());
        if expected != "needs-human" { assert_no_operator_park(&fake, &dispatcher); }
        if expected == "triage-grant" {
            let saved = fake.writes.borrow().iter().rev().find_map(|(_, b)| parse_marker(MARKER, text(b, "/body"))).unwrap();
            assert_eq!(saved["rounds"], 0);
            assert_eq!(saved["recovery"]["granted"], true);
            let mut exhausted = saved;
            exhausted["rounds"] = 2.into();
            let mut next = github();
            next.pulls.insert(470, pull("pull-470", json!({"contexts": {"ci": {"conclusion": "FAILURE"}}})));
            let (out, fake, dispatcher) = recovery_result_pass(next, Value::Null, exhausted, json!({}));
            assert_eq!(out.steps[0].action, "wait", "{:?}", out.lines);
            assert_no_operator_park(&fake, &dispatcher);
            assert!(dispatcher.writes.borrow().is_empty());
        }
    }
}

#[test]
fn recovery_respects_explicit_holds_and_resumes_released_parks() {
    for (change, expected) in [
        (json!({"labels": {"nodes": [{"name": LABEL}], "pageInfo": {"hasNextPage": false}}}), "skip"),
        (json!({"reviewRequests": {"nodes": [{"requestedReviewer": {"login": "cgwalters"}}], "pageInfo": {"hasNextPage": false}}}), "skip"),
        (json!({}), "merge"),
    ] {
        let (out, fake, dispatcher) = applied_policy(github(), recovery_policy(), change, json!({"rounds": 2, "stopped": true, "sha": HEAD}));
        assert_eq!(out.errors, 0, "{:?}", out.lines);
        assert_eq!(out.steps[0].action, expected, "{:?}", out.lines);
        assert_no_operator_park(&fake, &dispatcher);
    }
}

#[test]
fn recovery_failed_triage_and_exhausted_grant_are_bounded() {
    for (attempts, granted, pending, run_change, expected) in [
        (1, false, true, json!({"conclusion": "failure"}), "triage"),
        (2, false, true, json!({"conclusion": "failure"}), "wait"),
        (1, true, false, json!({}), "triage"),
        (2, true, false, json!({}), "wait"),
    ] {
        let mut fake = github();
        fake.pulls.insert(470, pull("pull-470", json!({"contexts": {"ci": {"conclusion": "FAILURE"}}})));
        let state = json!({"rounds": 2, "recovery": {"head": HEAD, "run": 1234, "at": "2026-10-12T01:00:00Z", "granted": granted, "pending": pending, "attempts": attempts}});
        let (out, fake, dispatcher) = recovery_result_pass(fake, Value::Null, state, run_change);
        assert_eq!(out.errors, 0, "{:?}", out.lines);
        assert_eq!(out.steps[0].action, expected, "{:?}", out.lines);
        assert_no_operator_park(&fake, &dispatcher);
        if expected == "wait" { assert!(dispatcher.writes.borrow().is_empty()); }
    }
    let (out, fake, dispatcher) = recovery_result_pass(github(), Value::Null, json!({"rounds": 2, "recovery": {"unknown": true}}), json!({}));
    assert_eq!(out.errors, 0, "{:?}", out.lines);
    assert_eq!(out.steps[0].action, "wait");
    assert_no_operator_park(&fake, &dispatcher);
    assert!(dispatcher.writes.borrow().is_empty());
}

#[test]
fn recovery_triage_obeys_dispatch_caps_and_late_holds() {
    for (cap, held, expected_writes) in [(Some(0), false, false), (None, true, false), (None, false, true)] {
        let mut fake = github();
        fake.pulls.insert(470, pull("pull-470", json!({"contexts": {"ci": {"conclusion": "FAILURE"}}})));
        fake.page(&format!("repos/{REPO}/issues/470/comments?per_page=100"), None, None,
            json!([{"id": 5, "user": {"login": BOT}, "body": marker_body(&json!({"rounds": 2, "sha": "older"}), "Exhausted")}]) );
        if held { fake.hold_after = Some(1); }
        let mut cfg = config();
        cfg.policy = recovery_policy();
        let dispatcher = Fake::default();
        let caps = caps::Caps::new(caps::Limits { dispatch: cap, ..caps::Limits::default() });
        let out = run_capped(&fake, &fake, &cfg, State::default(), (&[], &[REPO.into()]), applying(&dispatcher), Some(&caps), now()).unwrap();
        assert_eq!(out.errors, 0, "{:?}", out.lines);
        assert_eq!(!dispatcher.writes.borrow().is_empty(), expected_writes, "{:?}", out.lines);
        assert_eq!(!fake.writes.borrow().is_empty(), expected_writes, "{:?}", out.lines);
        assert_no_operator_park(&fake, &dispatcher);
    }
}

#[test]
fn recovery_never_overrides_incomplete_hold_coverage() {
    for late in [false, true] {
        for field in ["labels", "reviewRequests"] {
            let mut fake = github();
            let fields = json!({field: {"nodes": [], "pageInfo": {"hasNextPage": true}}});
            if late { fake.late_pull_fields = Some(fields); }
            else { fake.pulls.insert(470, pull("pull-470", fields)); }
            let state = json!({"rounds": 2, "recovery": {"head": HEAD, "run": 1234, "at": "2026-10-12T01:00:00Z", "granted": false, "pending": true, "attempts": 1}});
            let (out, fake, dispatcher) = recovery_result_pass(fake, Value::Null, state, json!({"conclusion": "failure"}));
            assert_eq!(out.errors, 0, "{field} late={late}: {:?}", out.lines);
            assert!(dispatcher.writes.borrow().is_empty(), "{field} late={late}: {:?}", out.lines);
            assert!(fake.writes.borrow().is_empty(), "{field} late={late}: {:?}", out.lines);
            assert_no_operator_park(&fake, &dispatcher);
        }
    }
}

#[test]
fn recovery_exhaustion_respects_manual_and_unknown_queue_stops() {
    for reason in ["manually_removed", "unknown_future_reason"] {
        for (change, state) in [
            (json!({"contexts": {"ci": {"conclusion": "FAILURE"}}}), json!({"rounds": 2})),
            (json!({"isDraft": true, "body": checkpointed("cap")}), json!({"rounds": 0, "continued": 3})),
            (json!({"mergeable": CONFLICTING}), json!({"rounds": 0, "rebased": 2})),
        ] {
            let mut change = change;
            change["timelineItems"] = json!({"nodes": [{"id": "manual-removal", "reason": reason, "createdAt": "2026-10-12T01:30:00Z", "beforeCommit": {"oid": HEAD}}]});
            let mut policy = recovery_policy();
            policy.merge_queue = Some(3);
            policy.rebase_rounds = 2;
            let (out, fake, dispatcher) = applied_policy(github(), policy, change, state);
            assert_eq!(out.errors, 0, "{reason}: {:?}", out.lines);
            assert_eq!(out.steps[0].action, "needs-human", "{reason}: {:?}", out.lines);
            assert!(out.steps[0].reason.contains("which the poll does not retry"));
            assert!(dispatcher.writes.borrow().is_empty());
            assert!(!fake.writes.borrow().iter().any(|(_, b)| text(b, "/body").contains("\"recovery\"")));
        }
    }
}

#[test]
fn recovery_replacement_head_uses_its_own_review_gates_without_refunding_triage() {
    for (change, expected) in [(json!({}), "merge"), (json!({"contexts": {"review": null}}), "wait")] {
        let mut fake = github();
        fake.pulls.insert(470, pull("pull-470", change));
        let state = json!({"rounds": 2, "recovery": {"head": "older-head", "run": 1234, "at": "2026-10-12T01:00:00Z", "granted": false, "pending": true, "attempts": 1}});
        let (out, fake, dispatcher) = recovery_result_pass(fake, Value::Null, state, json!({}));
        assert_eq!(out.errors, 0, "{:?}", out.lines);
        assert_eq!(out.steps[0].action, expected, "{:?}", out.lines);
        assert_no_operator_park(&fake, &dispatcher);
        assert!(dispatcher.writes.borrow().is_empty());
        assert!(!fake.writes.borrow().iter().any(|(_, b)| text(b, "/body").contains("\"granted\":true")));
    }
}

#[test]
fn recovery_exhaustion_offers_ci_its_existing_retry_first() {
    let mut fake = failed_check_github();
    fake.page(&format!("repos/{REPO}/issues/470/comments?per_page=100"), None, None,
        json!([{"id": 5, "user": {"login": BOT}, "body": marker_body(&json!({"rounds": 2, "sha": "older"}), "Exhausted")}]) );
    let mut cfg = config();
    cfg.policy = recovery_policy();
    cfg.policy.check_reruns = true;
    let dispatcher = Fake::default();
    let out = run(&fake, &fake, &cfg, State::default(), &[], &[REPO.into()], applying(&dispatcher), now()).unwrap();
    assert_eq!(out.errors, 0, "{:?}", out.lines);
    assert_eq!(out.steps[0].action, CHECK_RERUN, "{:?}", out.lines);
    assert!(dispatcher.writes.borrow().is_empty());
    assert_no_operator_park(&fake, &dispatcher);
    assert!(fake.writes.borrow().iter().any(|(url, _)| url.ends_with("/runs/77/rerun-failed-jobs")));
}
