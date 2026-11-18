fn recovery_policy() -> PollPolicy {
    let mut policy = PollPolicy::from_json(&json!({"agentRecovery": true})).unwrap();
    policy.assign_holds = true;
    policy
}

/// A pass's share of `left` reads, and the reads made of it in order.
struct Metered<'a> {
    fake: &'a Fake,
    left: Cell<usize>,
    trace: RefCell<Vec<String>>,
}

impl Metered<'_> {
    fn read(&self, what: String) {
        self.left.set(self.left.get().saturating_sub(1));
        self.trace.borrow_mut().push(what);
    }
}

impl Transport for Metered<'_> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        if !query.trim_start().starts_with("mutation") {
            self.read(variables.to_string());
        }
        self.fake.graphql(query, variables)
    }
}

impl Rest for Metered<'_> {
    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
        self.read(url.into());
        self.fake.get(url, validator)
    }

    fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        self.fake.send(method, url, body)
    }

    fn share_left(&self) -> Option<usize> {
        Some(self.left.get())
    }
}

#[test]
fn overlap_recovery_prefilters_scope_checks_by_canonical_repository() {
    const MIXED_CASE: &str = "cgwalters-forge/Agentic-Job";
    let fake = github();
    let mut cfg = config();
    cfg.policy.overlap_recovery_hours = Some(1);
    cfg.policy.fix_bounds = bounds::FixBoundsSet::new(vec![bounds::FixBounds::parse(&format!(
        "repos = [\"{REPO}\"]\n[outputs]\npush_to_pull_request_branch = {{ branches = [\"dispatch/**\"] }}\n"
    )).unwrap()]);
    let (owner, name) = MIXED_CASE.split_once('/').unwrap();
    let pull = pull("pull-470", json!({"headRefName":"outside/470", "headRefOid":"b".repeat(40), "baseRefOid":"a".repeat(40), "isDraft":false}));
    for (size, undecided, eligible) in [
        (1, false, true),
        (1, true, false),
        (crate::pass::pressure::OLDER_LIMIT + 1, false, false),
    ] {
        let mut pass = Pass::new(&fake, &fake, &cfg, now());
        pass.pressure.open.insert(REPO.into(), vec![Value::Null; size]);
        pass.pressure.scope_checks.push((MIXED_CASE.into(), (owner.into(), name.into(), 470), pull.clone()));
        let undecided = undecided.then(|| REPO.to_owned()).into_iter().collect();
        pass.check_overlap_scope(&undecided, &mut Vec::new());
        let seen = pass.pressure.recovery.get(REPO).and_then(|repo| repo.get(&470));
        if eligible {
            assert!(matches!(seen, Some(overlap_recovery::Observation::Excluded(_))));
        } else {
            assert!(seen.is_none());
        }
    }
}

#[test]
fn overlap_recovery_replay_leaves_existing_steps_and_writes_alone() {
    use time::Duration;
    let base = "a".repeat(40);
    for case in ["off", "aged", "ready", "failing ci", "fix bounds", "disjoint fix bounds", "stale excluded head", "stale draft", "stale hold", "action first quiet", "action first new member", "draft", "old approval", "parked", "active", "claimed", "files truncated", "reviews truncated", "census truncated", "unrelated blocker", "unrelated park", "review holds off", "dependency hold", "dependency closed", "dependency unread", "late dependency hold", "late dependency unread", "late hold", "repair starts", "share boundary", "disjoint merge"] {
        let mut fake = github();
        let mut listed = Vec::new();
        let dependencies = case.starts_with("dependency ");
        let late_dependency = case.starts_with("late dependency ");
        let action_first = case.starts_with("action first ");
        // #474 is a later, landable PR outside the cluster.
        let last = if matches!(case, "share boundary" | "disjoint fix bounds" | "disjoint merge") { 474 } else if matches!(case, "unrelated park" | "stale excluded head" | "stale draft" | "stale hold") || dependencies || action_first { 473 } else { 472 };
        for number in 470..=last {
            let head = format!("{number:040x}");
            let paths = match number {
                470 => vec!["a"],
                471 => vec!["a", "b"],
                472 => vec!["b"],
                473 if matches!(case, "stale draft" | "stale hold" | "stale excluded head" | "action first new member") => vec!["a"],
                _ => vec!["z"],
            };
            let selected = number == 470;
            let failing_ci = number == 471 && case == "failing ci";
            let branch = if (number == 471 && case == "fix bounds") || (number == 474 && case == "disjoint fix bounds") { format!("outside/{number}") } else { format!("dispatch/{number}") };
            let draft = (selected && case == "draft") || (number == 473 && case == "stale draft");
            let labels = if (selected && case == "parked") || (number == 473 && case == "stale hold") { json!([{"name":"hold"}]) } else { json!([]) };
            let mut p = pull("pull-470", json!({
                "number":number,"headRefOid":head,"headRefName":branch,"baseRefOid":base,"isDraft":draft,
                "mergeable":if failing_ci || (number == 474 && matches!(case, "share boundary" | "disjoint merge")) || (selected && case == "ready") { "MERGEABLE" } else { CONFLICTING },
                "contexts":{"ci":{"conclusion": if failing_ci { "FAILURE" } else { "SUCCESS" }}},
                "files":{"nodes":paths.iter().map(|path| json!({"path":path,"changeType":"MODIFIED"})).collect::<Vec<_>>(),"pageInfo":{"hasNextPage":selected && case == "files truncated"}},
                "reviews":{"nodes":[review("APPROVED", user(&config().operator), if selected && case == "old approval" { HEAD } else { &head }, "") ]},
                "labels":{"nodes":labels},"truncated":{"reviews":selected && case == "reviews truncated"},
                "body":"Ordinary work.\n\nSupersedes #468\nSupersedes #469"
            }));
            p["headRepository"] = json!({"nameWithOwner": format!("{BOT}/repo")});
            if dependencies || late_dependency {
                fake.page(&format!("repos/{REPO}/issues/{number}/dependencies/blocked_by?per_page=11"), None, None, json!([]));
            }
            if number == 473 && dependencies {
                p["isCrossRepository"] = json!(false);
                p["body"] = json!("Depends on: other/repo#12");
                let issue = match case {
                    "dependency hold" => Some(json!({"number":12,"state":"open"})),
                    "dependency closed" => Some(json!({"number":12,"state":"closed","state_reason":"not_planned"})),
                    _ => None,
                };
                if let Some(issue) = issue {
                    fake.page("repos/other/repo/issues/12", None, None, issue);
                }
            } else if number == 473 && case == "action first quiet" {
                p["mergeable"] = json!("MERGEABLE");
                p["updatedAt"] = json!("2026-10-12T01:30:00Z");
                p["reviews"]["nodes"] = json!([review("COMMENTED", job(), &head, WITHHELD)]);
                fake.page(&format!("repos/{REPO}/actions/runs/{REVIEW_RUN}"), None, None, job_run(&p["headRefOid"]));
            } else if (number == 473 && !matches!(case, "disjoint fix bounds" | "stale draft" | "stale hold" | "action first new member"))
                // Without assignHolds the poll does not park a requested review.
                || (number == 471 && case == "review holds off")
            {
                p["reviewRequests"]["nodes"] = json!([{"requestedReviewer":{"login":config().operator}}]);
            }
            fake.pulls.insert(number, p);
            if number != 473 || case != "action first new member" {
                listed.push(json!({"number":number,"user":{"login":BOT},"head":{"sha":head},"base":{"sha":base},"draft":draft,"labels":labels}));
            }
            fake.page(&format!("repos/{REPO}/pulls/{number}/files?per_page=100"), None, None,
                json!(paths.into_iter().map(|filename| json!({"filename":filename})).collect::<Vec<_>>()));
            let comments = if selected && case == "active" {
                fake.page(&format!("repos/{}/actions/runs/7", config().dispatch_repo), None, None, json!({"status":"in_progress"}));
                json!([{"id":7,"user":{"login":BOT},"body":marker_body(&json!({"rounds":1,"run":7,"sha":head}),"Fix round") }])
            } else if selected && case == "claimed" {
                json!([{"id":7,"user":{"login":BOT},"body":format!("<!-- {CLAIM}: {} -->", json!({"from":"none","round":1,"sha":head}))}])
            } else { json!([]) };
            fake.page(&format!("repos/{REPO}/issues/{number}/comments?per_page=100"), None, None, comments);
        }
        if case == "late dependency hold" {
            fake.page("repos/other/repo/issues/12", None, None, json!({"number":12,"state":"open"}));
        }
        if case == "unrelated blocker" {
            // The ordinary conservative hold sees an apparent overlap which
            // the full decisions do not substantiate.
            fake.page(&format!("repos/{REPO}/pulls/470/files?per_page=100"), None, None, json!([{"filename":"b"}]));
        }
        let listing = format!("repos/{REPO}/pulls?state=open&per_page=100");
        fake.page(&listing, None, None, json!(listed));
        if case == "census truncated" {
            fake.rest.get_mut(&format!("{API_ROOT}{listing}")).unwrap().2 = Some(format!("<{API_ROOT}{listing}&page=2>; rel=\"next\""));
        }
        let dispatcher = Fake::default();
        dispatcher.answers.borrow_mut().insert(DISPATCHES.into(), vec![(200, json!({"workflow_run_id": 1234}))]);
        let mut cfg = config();
        cfg.policy.action_first = action_first;
        cfg.policy.quiet_waits = case == "action first quiet";
        if action_first {
            fake.page(&action_first::approved_path(&cfg), None, None, json!({"incomplete_results":false,"total_count":0,"items":[]}));
        }
        cfg.policy.open_pr_cap = Some(2);
        cfg.policy.rebase_rounds = 2;
        cfg.policy.assign_holds = case == "unrelated park";
        cfg.policy.hold_dependencies = dependencies || late_dependency;
        if matches!(case, "fix bounds" | "disjoint fix bounds") {
            cfg.policy.fix_bounds = bounds::FixBoundsSet::new(vec![bounds::FixBounds::parse(&format!(
                "repos = [\"{REPO}\"]\n[outputs]\npush_to_pull_request_branch = {{ branches = [\"dispatch/**\"] }}\n"
            )).unwrap()]);
        }
        cfg.policy.overlap_recovery_hours = (case != "off").then_some(1);
        let admit_repair = matches!(case, "late hold" | "repair starts" | "claimed");
        if case == "late hold" {
            fake.hold_after = Some(1);
        }
        if case == "share boundary" {
            // Measure the ordinary walk's reads before #474, then leave it
            // exactly the walk's margin there: the diagnostic's scope reads
            // must not take it, and have no share left after it.
            let metered = |cfg: &Config, left| {
                let meter = Metered { fake: &fake, left: Cell::new(left), trace: RefCell::default() };
                let dispatcher = Metered { fake: &dispatcher, left: Cell::new(usize::MAX), trace: RefCell::default() };
                let caps = caps::Caps::new(caps::Limits {dispatch: Some(0), writes: Some(0), status_changes: None});
                let outcome = run_capped(&meter, &meter, cfg, State::default(), (&[], &[REPO.to_owned()]), applying(&dispatcher), Some(&caps), now()).unwrap();
                (outcome, meter.trace.take())
            };
            cfg.policy.overlap_recovery_hours = None;
            let (_, trace) = metered(&cfg, usize::MAX);
            let before = trace.iter().position(|t| t.contains("/474") || t.contains("\"number\":474")).unwrap();
            cfg.policy.overlap_recovery_hours = Some(1);
            let (outcome, _) = metered(&cfg, before + PULL_SHARE_MARGIN);
            assert!(!outcome.lines.iter().any(|l| l.contains("request share is spent")), "{:?}", outcome.lines);
            assert!(outcome.lines.iter().any(|l| l.contains(&format!("{REPO}#474"))), "{:?}", outcome.lines);
            assert!(outcome.state.overlap_recovery.is_empty());
            // Unlike a repository with no aged cluster, the deferral is said.
            assert!(outcome.lines.iter().any(|l| l.starts_with("overlap recovery: ") && l.contains(" scope check(s) deferred for lack of budget")), "{:?}", outcome.lines);
            assert!(fake.writes.borrow().is_empty());
            continue;
        }
        let run = |fake: &Fake, state, at| {
            let admit_repair = admit_repair || (late_dependency && at > now());
            let caps = caps::Caps::new(caps::Limits {dispatch: Some(usize::from(admit_repair)), writes: Some(if admit_repair || (case == "disjoint merge" && at > now()) { 8 } else { 0 }), status_changes: None});
            run_capped(fake, fake, &cfg, state, (&[], &[REPO.to_owned()]), applying(&dispatcher), Some(&caps), at).unwrap()
        };
        let new_member = (case == "action first new member").then(|| fake.pulls.remove(&473).unwrap());
        let mut state = State::default();
        if case == "action first quiet" {
            state.bind_responses("same quiet scope");
        }
        let first = run(&fake, state, now());
        if let Some(new_member) = new_member {
            fake.pulls.insert(473, new_member);
        }
        if case == "claimed" {
            assert!(first.lines.iter().any(|l| l.contains("another pass went first")), "{:?}", first.lines);
        }
        if case == "repair starts" {
            assert!(first.state.overlap_recovery.is_empty());
            assert_eq!(first.dispatched, 1, "{:?}", first.lines);
            continue;
        }
        // A hosted export carries only the diagnostic history, not cached
        // responses or the credential scope.
        let saved = first.state.to_json();
        let mut projected = json!({"schema":saved["schema"],"interval_seconds":saved["interval_seconds"]});
        if let Some(history) = saved.get("overlap_recovery") {
            projected["overlap_recovery"] = history.clone();
        }
        if case == "action first quiet" {
            assert!(first.state.quiet_waits.contains_key(&quiet::key(REPO, 473)), "{:?}", first.lines);
            projected = saved;
        }
        let mut restored = State::from_json(&projected).unwrap();
        restored.bind_responses(if case == "action first quiet" { "same quiet scope" } else { "cold hosted scope" });
        if late_dependency {
            assert_eq!(first.state.overlap_recovery.len(), 1, "{case}: {:?}", first.lines);
            // The initial decision and take() both see no dependency. The
            // round's fresh check sees the prerequisites change just in time.
            fake.read_answers.borrow_mut().insert(
                format!("{API_ROOT}repos/{REPO}/issues/470/dependencies/blocked_by?per_page=11"),
                vec![json!([]), json!([]), json!([{"number":12,"repository_url":format!("{API_ROOT}repos/other/repo")}])],
            );
        }
        // Only the second census disagrees with the full PR decision.
        match case {
            "stale excluded head" => {
                let mut rows = fake.rest[&format!("{API_ROOT}{listing}")].3.clone();
                rows[3]["head"]["sha"] = json!("f".repeat(40));
                fake.read_answers.borrow_mut().insert(format!("{API_ROOT}{listing}"), vec![rows]);
            }
            "stale draft" => fake.pulls.get_mut(&473).unwrap()["isDraft"] = json!(false),
            "stale hold" => fake.pulls.get_mut(&473).unwrap()["labels"]["nodes"] = json!([]),
            _ => (),
        }
        let second = run(&fake, restored, now() + Duration::hours(1));
        let line = format!("{REPO}: overlap recovery for #470, #471, #472 held: caller must preserve ordered commits when opening one checked replacement PR");
        assert!(!first.lines.contains(&line), "{case}");
        let detected = matches!(case, "aged" | "disjoint fix bounds" | "unrelated park" | "review holds off" | "dependency hold" | "dependency closed" | "action first quiet");
        if case == "action first quiet" {
            assert!(second.lines.iter().any(|l| l.contains("quietWaits decided 1 ")), "{:?}", second.lines);
        }
        if case == "action first new member" {
            assert_eq!(first.state.overlap_recovery.len(), 1, "{:?}", first.lines);
            assert!(second.state.overlap_recovery.is_empty(), "{:?}", second.lines);
            assert!(second.lines.iter().any(|l| l.starts_with(&format!("{REPO}#473 ")) && l.contains("open-pr-cap: land")), "{:?}", second.lines);
        }
        if matches!(case, "stale excluded head" | "stale draft" | "stale hold") {
            assert_eq!(first.state.overlap_recovery.len(), 1, "{case}: {:?}", first.lines);
            assert!(second.state.overlap_recovery.is_empty(), "{case}: {:?}", second.lines);
            if matches!(case, "stale draft" | "stale hold") {
                assert!(second.lines.iter().any(|l| l.starts_with(&format!("{REPO}#473 ")) && l.contains("open-pr-cap: land")), "{:?}", second.lines);
            }
        }
        if case == "disjoint merge" {
            // The cluster aged against the base that #474 then moved.
            assert_eq!(first.state.overlap_recovery.len(), 1, "{:?}", first.lines);
            assert!(second.lines.iter().any(|l| l.starts_with(&format!("merge {REPO}#474: ")) && l.ends_with(" -> merged")), "{:?}", second.lines);
            assert!(second.lines.iter().any(|l| l.starts_with(&format!("{REPO}#471 ")) && l.contains("open-pr-cap: land")), "{:?}", second.lines);
            assert!(second.state.overlap_recovery.is_empty(), "{:?}", second.lines);
            let writes = fake.writes.borrow();
            assert!(writes.len() == 1 && writes[0].0.contains("mergePullRequest"), "{writes:?}");
            assert!(dispatcher.writes.borrow().is_empty());
            continue;
        }
        if case == "disjoint fix bounds" {
            assert!(second.lines.iter().any(|l| l.starts_with(&format!("{REPO}#474 ")) && l.contains("open-pr-cap: land") && l.contains("#473")), "{:?}", second.lines);
        }
        if dependencies {
            let reason = match case {
                "dependency hold" => "blocked by other/repo#12",
                "dependency closed" => "dependency hold: other/repo#12 closed without completion/merge",
                _ => "dependency hold:",
            };
            assert!(second.lines.iter().any(|l| l.starts_with(&format!("{REPO}#473 blocked: {reason}"))), "{case}: {:?}", second.lines);
        }
        if late_dependency {
            assert!(second.lines.iter().any(|l| l.starts_with(&format!("{REPO}#470 ")) && l.contains(if case == "late dependency hold" { "not started: blocked by other/repo#12" } else { "not started: dependency hold:" })), "{case}: {:?}", second.lines);
            assert!(second.state.overlap_recovery.is_empty(), "{case}");
        }
        assert_eq!(second.lines.contains(&line), detected, "{case}: {:?}", second.lines);
        if matches!(case, "failing ci" | "fix bounds") {
            assert!(second.lines.iter().any(|l| l.starts_with(&format!("{REPO}#471 ")) && l.contains("wait: open-pr-cap: land")), "{:?}", second.lines);
            assert!(second.state.overlap_recovery.is_empty());
        }
        if detected {
            assert_eq!(second.state.overlap_recovery.len(), 1);
            assert!(second.lines.iter().any(|l| l.contains("wait: open-pr-cap: land")));
        }
        assert!(dispatcher.writes.borrow().is_empty(), "{case}");
        assert!(fake.writes.borrow().is_empty(), "{case}: {:?}", fake.writes.borrow());
    }
}

#[test]
fn open_pr_cap_keeps_landing_work_and_serializes_oldest_paths() {
    // The pass's policy and a standalone poll's `openPrCap` decide alike.
    for (cap, standalone) in [None, Some(1), Some(2), Some(3)].into_iter().flat_map(|cap| [(cap, false), (cap, true)]) {
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
                let policy = if standalone {
                    let mut value = json!({"rebaseRounds": 2});
                    if let Some(cap) = cap { value["openPrCap"] = cap.into(); }
                    PollPolicy::from_json(&value).unwrap()
                } else {
                    PollPolicy { open_pr_cap:cap, rebase_rounds:2, ..PollPolicy::default() }
                };
                let mut first = change;
                first["files"] = files("src/a.rs");
                let (out, _, _) = applied_policy(fake, policy, first, json!({"rounds":0}));
                assert_eq!(out.errors, 0, "{cap:?}/{shared}/{action}: {:?}", out.lines);
                assert_eq!(out.steps[0].number, 470);
                assert_eq!(out.steps[0].action, action, "{cap:?}/{shared}: {:?}", out.lines);
                // A ready head merges past older overlapping work (#1035).
                let held = shared && action != "merge" && cap.is_some_and(|cap| cap <= 2);
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
        // A stopped draft cannot merge and waits on a person, hold or not.
        ("stopped draft", json!({"draft":true}), Some(stop(true)), true, false, 2),
        ("stopped draft, assign holds off", json!({"draft":true}), Some(stop(true)), false, false, 2),
        ("draft without a stop", json!({"draft":true}), Some(stop(false)), true, true, 3),
        // The fixture's comments list fails on its second page.
        ("unreadable stop", json!({"requested_reviewers":operator}), None, true, true, 4),
        ("unreadable draft stop", json!({"draft":true}), None, true, true, 4),
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
        ("fix", json!({"contexts": {"ci": {"conclusion": "FAILURE"}}}), json!({"rounds": FIX_ROUNDS, "sha": "older"})),
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
            json!({"rounds": FIX_ROUNDS, "sha": "older"}));
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
    let state = json!({"rounds": FIX_ROUNDS, "sha": "older", "recovery": {"head": HEAD, "run": 1234, "at": "2026-10-12T01:00:00Z", "granted": false, "pending": true, "attempts": 2}});
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
            exhausted["rounds"] = FIX_ROUNDS.into();
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
        let (out, fake, dispatcher) = applied_policy(github(), recovery_policy(), change, json!({"rounds": FIX_ROUNDS, "stopped": true, "sha": HEAD}));
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
        let state = json!({"rounds": FIX_ROUNDS, "recovery": {"head": HEAD, "run": 1234, "at": "2026-10-12T01:00:00Z", "granted": granted, "pending": pending, "attempts": attempts}});
        let (out, fake, dispatcher) = recovery_result_pass(fake, Value::Null, state, run_change);
        assert_eq!(out.errors, 0, "{:?}", out.lines);
        assert_eq!(out.steps[0].action, expected, "{:?}", out.lines);
        assert_no_operator_park(&fake, &dispatcher);
        if expected == "wait" { assert!(dispatcher.writes.borrow().is_empty()); }
    }
    let (out, fake, dispatcher) = recovery_result_pass(github(), Value::Null, json!({"rounds": FIX_ROUNDS, "recovery": {"unknown": true}}), json!({}));
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
            json!([{"id": 5, "user": {"login": BOT}, "body": marker_body(&json!({"rounds": FIX_ROUNDS, "sha": "older"}), "Exhausted")}]) );
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
            let state = json!({"rounds": FIX_ROUNDS, "recovery": {"head": HEAD, "run": 1234, "at": "2026-10-12T01:00:00Z", "granted": false, "pending": true, "attempts": 1}});
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
            (json!({"contexts": {"ci": {"conclusion": "FAILURE"}}}), json!({"rounds": FIX_ROUNDS})),
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
        let state = json!({"rounds": FIX_ROUNDS, "recovery": {"head": "older-head", "run": 1234, "at": "2026-10-12T01:00:00Z", "granted": false, "pending": true, "attempts": 1}});
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
        json!([{"id": 5, "user": {"login": BOT}, "body": marker_body(&json!({"rounds": FIX_ROUNDS, "sha": "older"}), "Exhausted")}]) );
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
