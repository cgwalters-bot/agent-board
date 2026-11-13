fn dependency_fixture() -> (Fake, Config) {
    let mut fake = github();
    let mut cfg = config();
    cfg.policy.hold_dependencies = true;
    fake.page(&format!("repos/{REPO}/issues/470/dependencies/blocked_by?per_page=11"), None, None, json!([]));
    fake.pulls.insert(470, pull("pull-470", json!({"body":"Depends on: other/repo#12"})));
    fake.page("repos/other/repo/issues/12", None, None,
        json!({"number":12,"state":"open","pull_request":{}}));
    fake.page("repos/other/repo/pulls/12", None, None,
        json!({"number":12,"state":"open","merged":false}));
    (fake, cfg)
}

fn dependency_pass(fake: &Fake, cfg: &Config, apply: bool, caps: Option<&caps::Caps>) -> Outcome {
    let dispatcher = Fake::default();
    run_capped(fake, fake, cfg, State::default(), (&[], &[REPO.into()]),
        if apply { applying(&dispatcher) } else { None }, caps, now()).unwrap()
}

fn dependency_record(fake: &mut Fake, mut state: Value) {
    if state.get("owns_draft").is_none() {
        state["owns_draft"] = true.into();
    }
    // GraphQL comments are deliberately incomplete in these fixtures; use the
    // same REST comment history as real passes with more than one page.
    fake.page(&format!("repos/{REPO}/issues/470/comments?per_page=100"), None, None,
        json!([{"id":77,"user":{"login":BOT},"body":format!("<!-- agent-board-dependencies: {state} -->")} ]));
}

#[test]
fn dependency_state_table() {
    for (name, issue, pr, record, comparison, expected) in [
        ("open PR", json!({"number":12,"state":"open","pull_request":{}}), Some(json!({"number":12,"state":"open","merged":false})), false, "", "blocked"),
        ("merged PR", json!({"number":12,"state":"closed","pull_request":{}}), Some(json!({"number":12,"state":"closed","merged":true})), true, "ahead", "dependency-ready"),
        ("merged needs rebase", json!({"number":12,"state":"closed","pull_request":{}}), Some(json!({"number":12,"state":"closed","merged":true})), true, "diverged", "dependency-update"),
        ("unmerged PR", json!({"number":12,"state":"closed","pull_request":{}}), Some(json!({"number":12,"state":"closed","merged":false})), true, "", "needs-human"),
        ("cross repo open issue", json!({"number":12,"state":"open"}), None, false, "", "blocked"),
        ("completed issue", json!({"number":12,"state":"closed","state_reason":"completed"}), None, true, "identical", "dependency-ready"),
        ("abandoned issue", json!({"number":12,"state":"closed","state_reason":"not_planned"}), None, true, "", "needs-human"),
        ("unknown completion", json!({"number":12,"state":"closed"}), None, true, "", "needs-human"),
    ] {
        for apply in [false, true] {
            let (mut fake, cfg) = dependency_fixture();
            fake.pulls.get_mut(&470).unwrap()["isDraft"] = record.into();
            fake.page("repos/other/repo/issues/12", None, None, issue.clone());
            if let Some(pr) = &pr { fake.page("repos/other/repo/pulls/12", None, None, pr.clone()); }
            if record { dependency_record(&mut fake, json!({"update_head":"","released":false,"reason":"blocked by other/repo#12"})); }
            fake.page(&format!("repos/{REPO}/compare/main...{HEAD}"), None, None, json!({"status":comparison}));
            let out = dependency_pass(&fake, &cfg, apply, None);
            assert_eq!(out.errors, 0, "{name}: {:?}", out.lines);
            assert_eq!(out.steps[0].action, expected, "{name}: {:?}", out.lines);
            assert!(out.steps[0].reason.contains(if expected == "blocked" { "other/repo#12" } else { "dependenc" }) || expected == "needs-human", "{name}");
            let writes = fake.writes.borrow();
            if !apply { assert!(writes.is_empty()); continue; }
            assert!(!writes.iter().any(|(url, _)| url.contains("dispatch") || url.contains("enqueuePullRequest") || url.contains("mergePullRequest")), "{name}: {writes:?}");
            assert_eq!(writes.iter().filter(|(url, _)| url.contains("markPullRequestReadyForReview")).count(), usize::from(expected == "dependency-ready"), "{name}: {writes:?}");
            assert_eq!(writes.iter().filter(|(url, _)| url.contains("updatePullRequestBranch")).count(), usize::from(expected == "dependency-update"), "{name}: {writes:?}");
        }
    }
}

#[test]
fn dependency_holds_are_opt_in_bounded_and_repeatable() {
    let (mut fake, mut cfg) = dependency_fixture();
    cfg.policy.hold_dependencies = false;
    let out = dependency_pass(&fake, &cfg, false, None);
    assert_ne!(out.steps[0].action, "blocked");
    assert!(!fake.sent.borrow().iter().any(|(url, _)| url.contains("other/repo")));
    cfg.policy.hold_dependencies = true;
    let out = dependency_pass(&fake, &cfg, true, None);
    assert_eq!(out.errors, 0, "{:?}", out.lines);
    let body = fake.writes.borrow().iter().find(|(url, _)| url.ends_with("/comments")).unwrap().1["body"].clone();
    fake.writes.borrow_mut().clear();
    fake.pulls.get_mut(&470).unwrap()["isDraft"] = true.into();
    fake.pulls.get_mut(&470).unwrap()["labels"]["nodes"] = json!([{"name":"blocked"}]);
    fake.page(&format!("repos/{REPO}/issues/470/comments?per_page=100"), None, None,
        json!([{"id":77,"user":{"login":BOT},"body":body}]));
    let out = dependency_pass(&fake, &cfg, true, None);
    assert_eq!(out.errors, 0, "{:?}", out.lines);
    assert_eq!(out.steps[0].action, "blocked");
    assert!(fake.writes.borrow().is_empty(), "repeat hold wrote");
    // A capped first hold still suppresses recovery and merge.
    fake.pulls.get_mut(&470).unwrap()["isDraft"] = false.into();
    let caps = caps::Caps::new(caps::Limits { writes: Some(0), ..Default::default() });
    let out = dependency_pass(&fake, &cfg, true, Some(&caps));
    assert_eq!(out.errors, 0, "{:?}", out.lines);
    assert!(!out.steps[0].ran);
    assert!(fake.writes.borrow().is_empty());
}

#[test]
fn dependency_release_is_once_and_updates_are_not_retried() {
    for released in [false, true] {
        let (mut fake, cfg) = dependency_fixture();
        fake.page("repos/other/repo/pulls/12", None, None, json!({"number":12,"state":"closed","merged":true}));
        dependency_record(&mut fake, json!({"update_head":HEAD,"released":released,"reason":"reserved update"}));
        fake.pulls.get_mut(&470).unwrap()["isDraft"] = true.into();
        fake.page(&format!("repos/{REPO}/compare/main...{HEAD}"), None, None, json!({"status":"diverged"}));
        let out = dependency_pass(&fake, &cfg, false, None);
        assert_eq!(out.errors, 0, "{:?}", out.lines);
        if released {
            assert_ne!(out.steps[0].action, "dependency-ready", "a human's later draft was promoted");
        } else {
            assert_eq!(out.steps[0].action, "needs-human");
        }
        assert!(fake.writes.borrow().is_empty());
    }
}

#[test]
fn native_dependency_and_none() {
    for native in [false, true] {
        let (mut fake, cfg) = dependency_fixture();
        fake.pulls.get_mut(&470).unwrap()["body"] = "No body declarations".into();
        if native {
            fake.page(&format!("repos/{REPO}/issues/470/dependencies/blocked_by?per_page=11"), None, None,
                json!([{"repository_url":"https://api.github.com/repos/other/repo","number":12}]));
        }
        let out = dependency_pass(&fake, &cfg, false, None);
        assert_eq!(out.errors, 0, "{:?}", out.lines);
        assert_eq!(out.steps[0].action == "blocked", native, "{:?}", out.lines);
    }
}

#[test]
fn dependency_negative_facts_never_start_rounds_or_promote() {
    for case in ["missing", "identity", "truncated", "self", "malformed", "mixed malformed", "partial", "hold", "late hold", "foreign", "scope", "scope default", "supervisor", "mixed-case supervisor", "supervisor update", "late title", "late labels"] {
        let (mut fake, mut cfg) = dependency_fixture();
        fake.page("repos/other/repo/pulls/12", None, None, json!({"number":12,"state":"closed","merged":true}));
        fake.pulls.get_mut(&470).unwrap()["isDraft"] = true.into();
        dependency_record(&mut fake, json!({"update_head":"","released":false,"reason":"blocked by other/repo#12"}));
        fake.page(&format!("repos/{REPO}/compare/main...{HEAD}"), None, None, json!({"status":"ahead"}));
        match case {
            "missing" => { fake.rest.remove(&format!("{API_ROOT}repos/other/repo/issues/12")); }
            "identity" => fake.page("repos/other/repo/issues/12", None, None, json!({"number":13,"state":"closed","state_reason":"completed"})),
            "truncated" => fake.pulls.get_mut(&470).unwrap()["labels"]["pageInfo"]["hasNextPage"] = true.into(),
            "self" => fake.pulls.get_mut(&470).unwrap()["body"] = "Depends on #470".into(),
            "malformed" => fake.pulls.get_mut(&470).unwrap()["body"] = "Depends on a release".into(),
            "mixed malformed" => fake.pulls.get_mut(&470).unwrap()["body"] = "Depends on other/repo#12 and other/repo#bad".into(),
            "partial" => fake.pulls.get_mut(&470).unwrap()["title"] = "[partial: tests] unfinished work".into(),
            "hold" => fake.pulls.get_mut(&470).unwrap()["labels"]["nodes"] = json!([{"name":HOLD_LABEL}]),
            "late hold" => fake.hold_after = Some(1),
            "foreign" => fake.pulls.get_mut(&470).unwrap()["headRepositoryOwner"]["login"] = "someone-else".into(),
            "scope" => cfg.policy.own_space_only = true,
            "scope default" => {},
            "supervisor" => fake.pulls.get_mut(&470).unwrap()["labels"]["nodes"] = json!([{"name":"needs-supervisor"}]),
            "mixed-case supervisor" | "supervisor update" => {
                fake.pulls.get_mut(&470).unwrap()["labels"]["nodes"] = json!([{"name":"Needs-Supervisor"}]);
                if case == "supervisor update" {
                    fake.page(&format!("repos/{REPO}/compare/main...{HEAD}"), None, None, json!({"status":"diverged"}));
                }
            }
            "late title" => fake.late_pull_fields = Some(json!({"title":"[partial: tests] unfinished work"})),
            "late labels" => fake.late_pull_fields = Some(json!({"labels":{"nodes":[],"pageInfo":{"hasNextPage":true}}})),
            _ => unreachable!(),
        }
        // A scope refusal uses the existing configured own-space predicate.
        if case.starts_with("scope") { cfg.dispatch_repo = "someone-else/tracker".into(); }
        let out = dependency_pass(&fake, &cfg, true, None);
        assert!(!fake.writes.borrow().iter().any(|(url, _)| url.contains("markPullRequestReady") || url.contains("updatePullRequestBranch") || url.contains("dispatch") || url.contains("mergePullRequest")), "{case}: {:?} {:?}", out.lines, fake.writes.borrow());
    }
}

fn dependency_second_pull(fake: &mut Fake, record: bool) {
    let mut second = fake.pulls[&470].clone();
    second["number"] = 471.into();
    second["id"] = "P_471".into();
    fake.pulls.insert(471, second);
    fake.page(&format!("repos/{REPO}/pulls?state=open&per_page=100"), None, None,
        json!([{"number":470,"user":{"login":BOT}},{"number":471,"user":{"login":BOT}}]));
    fake.page(&format!("repos/{REPO}/issues/471/dependencies/blocked_by?per_page=11"), None, None, json!([]));
    let comments = if record {
        json!([{"id":78,"user":{"login":BOT},"body": format!("<!-- agent-board-dependencies: {} -->", json!({"update_head":"","owns_draft":true,"released":false,"reason":"blocked by other/repo#12"}))}])
    } else { json!([]) };
    fake.page(&format!("repos/{REPO}/issues/471/comments?per_page=100"), None, None, comments);
}

#[test]
fn dependency_partial_failure_consumes_write_step_and_rebases_share_repo_limit() {
    for apply in [false, true] {
        let (mut fake, cfg) = dependency_fixture();
        fake.pulls.get_mut(&470).unwrap()["isDraft"] = true.into();
        dependency_record(&mut fake, json!({"update_head":"","released":false,"reason":"blocked by other/repo#12"}));
        dependency_second_pull(&mut fake, true);
        fake.page("repos/other/repo/pulls/12", None, None, json!({"number":12,"state":"closed","merged":true}));
        fake.page(&format!("repos/{REPO}/compare/main...{HEAD}"), None, None, json!({"status":"diverged"}));
        let out = dependency_pass(&fake, &cfg, apply, None);
        assert_eq!(out.errors, 0, "{:?}", out.lines);
        assert_eq!(out.steps.iter().filter(|s| s.action == "dependency-update").count(), 1, "{:?}", out.steps);
        assert_eq!(fake.writes.borrow().iter().filter(|(url, _)| url.contains("updatePullRequestBranch")).count(), usize::from(apply));
        assert!(out.steps.iter().any(|s| s.reason.contains("another PR was updated")));
    }
    let (mut fake, cfg) = dependency_fixture();
    dependency_second_pull(&mut fake, false);
    fake.refused.push(dependencies::DRAFT);
    let caps = caps::Caps::new(caps::Limits { writes: Some(1), ..Default::default() });
    let out = dependency_pass(&fake, &cfg, true, Some(&caps));
    assert_eq!(out.errors, 1, "{:?}", out.lines);
    assert_eq!(fake.writes.borrow().iter().filter(|(url, _)| url.ends_with("/comments")).count(), 1, "{:?}", fake.writes.borrow());
    assert!(out.steps.iter().any(|step| !step.ran));
}

#[test]
fn dependency_noop_cannot_repair_a_late_change_past_write_cap() {
    for fields in [json!({"isDraft":false}), json!({"labels":{"nodes":[],"pageInfo":{"hasNextPage":false}}})] {
        let (mut fake, cfg) = dependency_fixture();
        fake.pulls.get_mut(&470).unwrap()["isDraft"] = true.into();
        fake.pulls.get_mut(&470).unwrap()["labels"]["nodes"] = json!([{"name":"blocked"}]);
        dependency_record(&mut fake, json!({"update_head":"","released":false,"reason":"blocked by other/repo#12"}));
        fake.late_pull_fields = Some(fields);
        let caps = caps::Caps::new(caps::Limits { writes: Some(0), ..Default::default() });
        let out = dependency_pass(&fake, &cfg, true, Some(&caps));
        assert_eq!(out.errors, 0, "{:?}", out.lines);
        assert!(!out.steps[0].ran);
        assert!(fake.writes.borrow().is_empty());
    }
}

#[test]
fn dependency_fix_plan_preflight_and_charge_share_admission() {
    let (fake, mut cfg) = dependency_fixture();
    cfg.policy.fix_plans = true;
    // An otherwise admissible own-space plan, including after the poll's
    // write cap prevented it from adding any blocked label or draft marker.
    cfg.policy.hold_dependencies = false;
    assert!(planned_fix_control(&fake, &fake, &cfg, &[REPO.into()],
        (REPO, 470, &fake.pulls[&470]), now()).is_ok());
    assert!(planned_fix_hold(&fake, &fake, &cfg, (REPO, 470, HEAD), now()).unwrap().is_none());
    cfg.policy.hold_dependencies = true;
    for _ in 0..2 {
        // The same preflight is used again immediately before dispatch.
        let why = planned_fix_control(&fake, &fake, &cfg, &[REPO.into()],
            (REPO, 470, &fake.pulls[&470]), now()).unwrap_err();
        assert!(why.contains("blocked by other/repo#12"), "{why}");
        assert!(planned_fix_hold(&fake, &fake, &cfg, (REPO, 470, HEAD), now())
            .unwrap().unwrap().contains("blocked by other/repo#12"));
    }
    assert!(matches!(charge_planned_fix(&fake, &fake, &cfg,
        (REPO, 470, HEAD, "claude"), now()).unwrap(), Err(why) if why.contains("blocked by other/repo#12")));
    assert!(fake.writes.borrow().is_empty());
}

fn dependency_work_path(action: &'static str) {
    for pending_release in [false, true] {
        let (mut fake, cfg) = dependency_fixture();
        if pending_release {
            fake.page("repos/other/repo/pulls/12", None, None,
                json!({"number":12,"state":"closed","merged":true}));
            dependency_record(&mut fake, json!({"update_head":"","released":false,"reason":"blocked"}));
        }
        let mut pass = Pass::new(&fake, &fake, &cfg, now());
        let pull = &fake.pulls[&470];
        let checks = Checks::default();
        let marker = Marker::default();
        let why = match action {
            "continue" | "rebase" => pass.start_fix(REPO, pull, &checks,
                (&marker, &[]), 1, action, None).unwrap(),
            "update" => pass.update(REPO, pull, "not rebased: ").unwrap(),
            "merge" => pass.take(REPO, pull, &checks, (&marker, &[]),
                &step("merge", "approved and green"), None).unwrap().unwrap(),
            _ => unreachable!(),
        };
        assert!(why.contains(if pending_release { "awaits branch update" } else { "blocked by other/repo#12" }), "{action}: {why}");
        assert!(fake.writes.borrow().is_empty(), "{action}");
        assert_eq!(pass.dispatched, 0);
        assert!(pass.updated.is_empty());
    }
}

#[test]
fn dependency_continuation_admission() {
    dependency_work_path("continue");
}

#[test]
fn dependency_rebase_admission() {
    dependency_work_path("rebase");
    dependency_work_path("update");
}

#[test]
fn dependency_enqueue_admission() {
    dependency_work_path("merge");
}

#[test]
fn dependency_deferred_review_uses_fresh_body() {
    for action in [REVIEW, REVIEW_RERUN] {
        let (mut fake, cfg) = dependency_fixture();
        fake.pulls.get_mut(&470).unwrap()["body"] = "No dependencies yet".into();
        fake.late_pull_fields = Some(json!({"body":"Depends on other/repo#12"}));
        let dispatcher = Fake::default();
        let mut pass = Pass::new(&fake, &fake, &cfg, now());
        pass.apply = applying(&dispatcher);
        let (owner, name) = REPO.split_once('/').unwrap();
        let pull = pass.read_pull(owner, name, 470).unwrap();
        let review = Deferred {
            rank: 0, repo: REPO.into(), number: 470, pull,
            marker: Marker::default(), comments: vec![], action,
            run: Some((7, None)), line: String::new(), at: (0, None),
        };
        let (done, ran) = pass.take_review(&review).unwrap();
        assert!(!ran);
        assert!(done.unwrap().contains("blocked by other/repo#12"));
        assert!(fake.writes.borrow().is_empty());
        assert!(dispatcher.writes.borrow().is_empty());
        assert_eq!(pass.dispatched, 0);
    }
}

fn dependency_save_marker(fake: &mut Fake) -> Value {
    let body = fake.writes.borrow().iter().rev()
        .find(|(url, _)| url.contains("/comments")).unwrap().1["body"].clone();
    fake.page(&format!("repos/{REPO}/issues/470/comments?per_page=100"), None, None,
        json!([{"id":77,"user":{"login":BOT},"body":body}]));
    fake.writes.borrow_mut().clear();
    let state = body.as_str().unwrap().lines().next().unwrap()
        .strip_prefix("<!-- agent-board-dependencies: ").unwrap().strip_suffix(" -->").unwrap();
    serde_json::from_str(state).unwrap()
}

#[test]
fn dependency_ambiguous_update_survives_reopened_prerequisite() {
    let (mut fake, cfg) = dependency_fixture();
    fake.pulls.get_mut(&470).unwrap()["isDraft"] = true.into();
    dependency_record(&mut fake, json!({"update_head":"","released":false,"reason":"blocked"}));
    fake.page("repos/other/repo/pulls/12", None, None,
        json!({"number":12,"state":"closed","merged":true}));
    fake.page(&format!("repos/{REPO}/compare/main...{HEAD}"), None, None, json!({"status":"diverged"}));
    fake.refused.push(UPDATE);
    let out = dependency_pass(&fake, &cfg, true, None);
    assert_eq!(out.errors, 1, "{:?}", out.lines);
    assert_eq!(dependency_save_marker(&mut fake)["update_head"], HEAD);
    fake.refused.clear();
    fake.page("repos/other/repo/pulls/12", None, None,
        json!({"number":12,"state":"open","merged":false}));
    let out = dependency_pass(&fake, &cfg, true, None);
    assert_eq!(out.errors, 0, "{:?}", out.lines);
    assert_eq!(dependency_save_marker(&mut fake)["update_head"], HEAD);
    fake.page("repos/other/repo/pulls/12", None, None,
        json!({"number":12,"state":"closed","merged":true}));
    let out = dependency_pass(&fake, &cfg, true, None);
    assert_eq!(out.errors, 0, "{:?}", out.lines);
    assert_eq!(out.steps[0].action, "needs-human");
    assert!(!fake.writes.borrow().iter().any(|(url, _)| url == UPDATE));
}

#[test]
fn dependency_preexisting_draft_is_not_ours_to_release() {
    for legacy in [false, true] {
        let (mut fake, cfg) = dependency_fixture();
        fake.pulls.get_mut(&470).unwrap()["isDraft"] = true.into();
        if legacy {
            fake.page(&format!("repos/{REPO}/issues/470/comments?per_page=100"), None, None,
                json!([{"id":77,"user":{"login":BOT},"body":"<!-- agent-board-dependencies: {\"update_head\":\"\",\"released\":false,\"reason\":\"blocked\"} -->"}]));
        }
        let out = dependency_pass(&fake, &cfg, true, None);
        assert_eq!(out.errors, 0, "{:?}", out.lines);
        assert_eq!(dependency_save_marker(&mut fake)["owns_draft"], false);
        fake.page("repos/other/repo/pulls/12", None, None,
            json!({"number":12,"state":"closed","merged":true}));
        let out = dependency_pass(&fake, &cfg, true, None);
        assert_eq!(out.errors, 0, "{:?}", out.lines);
        assert!(out.steps[0].reason.contains("pre-existing draft"));
        assert!(!fake.writes.borrow().iter().any(|(url, _)| url.contains("markPullRequestReady") || url == UPDATE));
    }
}
