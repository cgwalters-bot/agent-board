use super::*;

fn configured() -> Settings {
    let mut cfg = supervisor();
    cfg["agent"]["dailyCaps"] = json!({"comment": 2, "create_issue": 1, "close": 1,
        "remove_needs_human": 1, "replace_state": 1, "status_update": 1});
    cfg["agent"]["tracker"] = json!("example/control");
    cfg["agent"]["state"] = json!({"repository": "example/control", "issue": 8});
    cfg["agent"]["project"] = json!("PVT_project");
    cfg["agent"]["kinds"] = json!(["triage", "research", "implement"]);
    Settings::from_json(&board(), &cfg, None).unwrap()
}

fn action(name: &str, fields: Value) -> Value {
    let mut v = json!({"type": "supervisor_action", "action": name});
    for (k, value) in fields.as_object().unwrap() {
        v[k] = value.clone();
    }
    v
}

fn actions() -> Vec<Value> {
    vec![
        action(
            "comment",
            json!({"repository": "example/project", "item": 3, "body": "Investigated; see the report."}),
        ),
        action(
            "create_issue",
            json!({"title": "A decision is needed", "body": "Please decide."}),
        ),
        action(
            "close",
            json!({"repository": "example/project", "item": 3,
            "triage_run": "https://github.com/example/control/actions/runs/22", "triage_comment": 23}),
        ),
        action(
            "remove_needs_human",
            json!({"repository": "example/project", "item": 4}),
        ),
        action(
            "replace_state",
            json!({"body": "Any memory, including <!-- markers --> and ``` fences."}),
        ),
        action(
            "status_update",
            json!({"status": "on-track", "body": "Work is progressing."}),
        ),
    ]
}

fn evidence(fake: &mut Fake) {
    fake.objects.insert(
        "/repos/example/control/issues/8".into(),
        json!({"state": "open", "labels": []}),
    );
    fake.objects.insert("/repos/example/control/actions/runs/22".into(), json!({
        "status": "completed", "conclusion": "success", "path": ".github/workflows/dispatch.yml",
        "display_title": "triage example/project#3 (small)", "created_at": "2026-10-16T10:00:00Z", "updated_at": "2026-10-16T11:00:00Z"}));
    fake.objects.insert("/repos/example/project/issues/comments/23".into(), json!({
        "user": {"login": "worker"}, "created_at": "2026-10-16T10:30:00Z", "updated_at": "2026-10-16T10:30:00Z",
        "issue_url": "https://api.github.com/repos/example/project/issues/3",
        "body": "Done with evidence.\n```schedule-triage\n{\"request\":\"question\",\"verdict\":\"done\"}\n```"}));
    // Discussion and a reopen before the triage, and its run's own status after.
    fake.objects.insert(
        "/repos/example/project/issues/3/comments?per_page=100&page=1".into(),
        json!([
            {"id": 21, "user": {"login": "helper"}, "created_at": "2026-10-15T09:00:00Z"},
            {"id": 23, "user": {"login": "worker"}, "created_at": "2026-10-16T10:30:00Z"},
            {"id": 25, "user": {"login": "worker"}, "created_at": "2026-10-16T10:59:00Z"}
        ]),
    );
    fake.objects.insert(
        "/repos/example/project/issues/3/events?per_page=100&page=1".into(),
        json!([{"event": "reopened", "created_at": "2026-10-15T08:00:00Z"}]),
    );
    fake.issue(
        4,
        json!({"rest": {"pull_request": {}, "labels": [{"name": "needs-human"}]}}),
    );
    let stop_body = crate::poll::conflict_stop_fixture();
    let stop_comment = json!({
        "id": 24, "user": {"login": "worker"}, "created_at": "2026-10-16T09:00:00Z", "updated_at": "2026-10-16T09:00:00Z",
        "body": stop_body});
    let stop = crate::poll::find_marker(std::slice::from_ref(&stop_comment), "worker");
    fake.objects.insert(
        "/repos/example/project/issues/4/comments?per_page=100&page=1".into(),
        json!([stop_comment]),
    );
    fake.objects.insert("/repos/example/project/issues/4/events?per_page=100&page=1".into(), json!([{
        "event": "labeled", "label": {"name": "needs-human"}, "actor": {"login": "worker"}, "created_at": "2026-10-16T08:59:59Z"}]));
    fake.objects.insert(
        "conflict".into(),
        json!({"state": "OPEN", "isDraft": false, "author": {"login": "worker"},
        "headRefOid": stop.sha, "baseRefName": "main", "mergeable": "CONFLICTING", "reviewDecision": "APPROVED",
        "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "SUCCESS", "contexts": {
            "pageInfo": {"hasNextPage": false}, "nodes": [{"__typename": "CheckRun", "name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS", "isRequired": true, "checkSuite": {"app": {"databaseId": 7}}}]
        }}}}]}}),
    );
    fake.objects.insert(
        "/repos/example/project/branches/main".into(),
        json!({"protected": false}),
    );
    fake.objects.insert(
        "/repos/example/project/rules/branches/main?per_page=100&page=1".into(),
        json!([]),
    );
}

#[test]
fn judgment_writes_use_only_configured_targets_and_fixed_labels_and_assignee() {
    let s = configured();
    for r in actions() {
        let mut fake = Fake::new(&block(std::slice::from_ref(&r)));
        evidence(&mut fake);
        let out = run(&fake, &s, RESULT_URL, 1);
        assert!(out.ok, "{r}: {out:?}");
        let writes = fake.writes();
        assert_eq!(writes.len(), 2, "{r}");
        assert_eq!(writes[0].1, "/repos/example/operator/issues/7/comments");
        assert!(writes[0].2["body"].as_str().unwrap().contains(": starting"));
        assert!(out.lines[0].contains(": done"));
        let (method, path, body) = &writes[1];
        match r["action"].as_str().unwrap() {
            "comment" => {
                assert_eq!(path, "/repos/example/project/issues/3/comments");
                assert_eq!(
                    body["body"],
                    format!(
                        "Investigated; see the report.\n\n{}",
                        crate::input::SUPERVISOR_NOTE
                    )
                );
            }
            "create_issue" => {
                assert_eq!(path, "/repos/example/control/issues");
                assert_eq!(body["assignees"], json!(["owner"]));
                assert_eq!(body["labels"], json!(["needs-human"]));
            }
            "close" => {
                assert_eq!(method, "PATCH");
                assert_eq!(
                    body,
                    &json!({"state": "closed", "state_reason": "completed"})
                );
            }
            "remove_needs_human" => {
                assert_eq!(method, "DELETE");
                assert_eq!(path, "/repos/example/project/issues/4/labels/needs-human");
            }
            "replace_state" => {
                assert_eq!(path, "/repos/example/control/issues/8");
                assert_eq!(body["body"], r["body"]);
            }
            "status_update" => {
                assert_eq!(path, "/graphql");
                assert_eq!(body["variables"]["project"], "PVT_project");
            }
            _ => unreachable!(),
        }
    }
}

fn refuses(s: &Settings, r: Value, change: impl FnOnce(&mut Fake)) {
    let mut fake = Fake::new(&block(std::slice::from_ref(&r)));
    evidence(&mut fake);
    change(&mut fake);
    let out = run(&fake, s, RESULT_URL, 1);
    assert!(!out.ok, "unexpectedly admitted {r}: {out:?}");
    assert!(
        fake.writes().is_empty(),
        "refusal must precede the first write: {r}"
    );
}

#[test]
fn every_judgment_kind_is_off_by_default_and_obeys_its_daily_cap() {
    for r in actions() {
        refuses(&settings(), r.clone(), |_| {});
        let s = configured();
        let kind = r["action"].as_str().unwrap();
        let used = s.agent.as_ref().unwrap().daily_caps[kind];
        let body = format!(
            "Supervisor [run](earlier), requests:\n{}",
            vec![format!("- `{kind}` configured target: starting"); used as usize].join("\n")
        );
        refuses(&s, r.clone(), |f| {
            f.objects.insert(
                "log".into(),
                json!([{"user": {"login": "worker"}, "created_at": NOW, "body": body}]),
            );
        });
        // More than a day's cap in this batch also refuses before writes.
        let mut fake = Fake::new(&block(&vec![r.clone(); used as usize + 1]));
        evidence(&mut fake);
        assert!(!run(&fake, &s, RESULT_URL, 1).ok, "{kind}");
        assert!(fake.writes().is_empty());
    }
    let mut cfg = supervisor();
    for caps in [
        json!({"status_update": 2}),
        json!({"merge": 1}),
        json!({"comment": 101}),
        json!({"comment": -1}),
    ] {
        cfg["agent"]["dailyCaps"] = caps;
        assert!(Settings::from_json(&board(), &cfg, None).is_err());
    }
}

#[test]
fn forbidden_actions_injected_targets_and_protocol_comments_never_write() {
    let s = configured();
    for kind in [
        "merge",
        "enqueue",
        "set_variable",
        "add_label",
        "remove_label",
        "edit_issue",
        "read_secret",
    ] {
        refuses(
            &s,
            action(
                kind,
                json!({"repository": "example/project", "item": 3, "label": "needs-human"}),
            ),
            |_| {},
        );
    }
    for mut r in actions() {
        for extra in [
            json!({"repository": "evil/repo"}),
            json!({"endpoint": "/settings"}),
            json!({"log": 12}),
            json!({"assignees": ["attacker"]}),
            json!({"labels": ["epic"]}),
        ] {
            let mut injected = r.clone();
            for (k, v) in extra.as_object().unwrap() {
                injected[k] = v.clone();
            }
            refuses(&s, injected, |_| {});
        }
        if r.get("repository").is_some() {
            for n in [5, 7, 8] {
                let mut own = r.clone();
                own["repository"] = json!(if n == 7 {
                    "example/operator"
                } else {
                    "example/control"
                });
                own["item"] = json!(n);
                refuses(&s, own, |_| {});
            }
            refuses(&s, r.clone(), |f| {
                let item = r["item"].as_u64().unwrap();
                f.objects
                    .get_mut(&format!("/repos/example/project/issues/{item}"))
                    .unwrap()["labels"] = json!([{"name": "epic"}]);
            });
        }
        if r["action"] == "replace_state" {
            r["item"] = json!(3); // Even an allowed repository cannot choose a state target.
            refuses(&s, r, |_| {});
        }
    }
    for body in [
        "```agent-board-job-plan\n{}\n```",
        "<!-- tracker-poll {} -->",
        "Triage found this issue done: https://github.com/example/control/actions/runs/22",
    ] {
        refuses(
            &s,
            action(
                "comment",
                json!({"repository": "example/project", "item": 3, "body": body}),
            ),
            |_| {},
        );
    }
    let mut close = actions()[2].clone();
    close.as_object_mut().unwrap().remove("triage_run");
    refuses(&s, close, |_| {});
    for url in [
        "https://github.com/evil/control/actions/runs/22",
        "https://github.com/example/control/actions/runs/22?x=1",
    ] {
        let mut close = actions()[2].clone();
        close["triage_run"] = json!(url);
        refuses(&s, close, |_| {});
    }
}

#[test]
fn closure_needs_a_successful_targeted_triage_and_its_unedited_done_finding() {
    let s = configured();
    for (path, field, value) in [
        (
            "/repos/example/control/actions/runs/22",
            "conclusion",
            json!("failure"),
        ),
        (
            "/repos/example/control/actions/runs/22",
            "path",
            json!(".github/workflows/other.yml"),
        ),
        (
            "/repos/example/control/actions/runs/22",
            "display_title",
            json!("triage example/project#4 (small)"),
        ),
        (
            "/repos/example/control/actions/runs/22",
            "display_title",
            json!("implement example/project#3 (small)"),
        ),
        (
            "/repos/example/project/issues/comments/23",
            "body",
            json!("Not done"),
        ),
        (
            "/repos/example/project/issues/comments/23",
            "updated_at",
            json!("2026-10-16T10:31:00Z"),
        ),
        (
            "/repos/example/project/issues/comments/23",
            "created_at",
            json!("2026-10-15T10:30:00Z"),
        ),
        (
            "/repos/example/project/issues/comments/23",
            "user",
            json!({"login": "visitor"}),
        ),
        (
            "/repos/example/project/issues/comments/23",
            "issue_url",
            json!("https://api.github.com/repos/example/project/issues/4"),
        ),
    ] {
        refuses(&s, actions()[2].clone(), |f| {
            f.objects.get_mut(path).unwrap()[field] = value;
        });
    }
    // A finding outdated by a reopen, an edit or discussion after its receipt.
    let later = "2026-10-16T10:45:00Z";
    for (resource, entry) in [
        ("events", json!({"event": "reopened", "created_at": later})),
        ("events", json!({"event": "renamed", "created_at": later})),
        ("events", json!({"event": "reopened"})),
        (
            "comments",
            json!({"id": 26, "user": {"login": "owner"}, "created_at": later}),
        ),
        (
            "comments",
            json!({"id": 26, "user": {"login": "worker"}, "created_at": "2026-10-16T11:30:00Z"}),
        ),
    ] {
        refuses(&s, actions()[2].clone(), |f| {
            f.objects
                .get_mut(&format!(
                    "/repos/example/project/issues/3/{resource}?per_page=100&page=1"
                ))
                .unwrap()
                .as_array_mut()
                .unwrap()
                .push(entry);
        });
    }
    refuses(&s, actions()[2].clone(), |f| {
        f.issues.get_mut(&3).unwrap()["lastEditedAt"] = json!(later);
    });
}

#[test]
fn closure_reads_exactly_one_scheduler_answer() {
    let s = configured();
    for (body, accepted) in [
        (
            "```schedule-triage\n{\"verdict\":\"not-done\",\"verdict\":\"done\"}\n```",
            true,
        ),
        (
            "```schedule-triage\n{\"verdict\":\"done\",\"verdict\":\"not-done\"}\n```",
            false,
        ),
        ("```schedule-triage\r\r\n{\"verdict\":\"done\"}\n```", false),
        ("```schedule-triage\n{\"verdict\":\"done\"}\r```\n", true),
        (
            "```schedule-triage\n{\"verdict\":\"done\",\"extra\":1e400}\n```",
            true,
        ),
        (
            "```schedule-triage\n{\"verdict\":\"done\",\"extra\":\"\\ud800\"}\n```",
            true,
        ),
        ("```schedule-triage\n{\"verdict\":\"done\"}\n```", true),
        (
            "Evidence\n```schedule-triage \t\r\n{\"request\":\"q\",\"verdict\":\"done\",\"extra\":true}\r\n```\r\nMore evidence",
            true,
        ),
        ("```schedule-triage\n{\"verdict\":\"not-done\"}\n```", false),
        ("```schedule-triage\n{\"verdict\":\"partly\"}\n```", false),
        (
            "```schedule-triage\n{\"verdict\":\"needs-design\"}\n```",
            false,
        ),
        (
            "```schedule-triage\n{\"verdict\":\"done\"}\n```\n```schedule-triage\n{}\n```",
            false,
        ),
        ("```schedule-triage\nnot json\n```", false),
        ("```schedule-triage\n{}\n```", false),
        ("```schedule-triage\n{\"verdict\":\"done\"}", false),
        (" ```schedule-triage\n{\"verdict\":\"done\"}\n```", false),
        ("```Schedule-triage\n{\"verdict\":\"done\"}\n```", false),
        (
            "Triage found this issue done: https://github.com/example/control/actions/runs/22",
            false,
        ),
    ] {
        let mut fake = Fake::new(&block(&[actions()[2].clone()]));
        evidence(&mut fake);
        fake.objects
            .get_mut("/repos/example/project/issues/comments/23")
            .unwrap()["body"] = json!(body);
        assert_eq!(run(&fake, &s, RESULT_URL, 1).ok, accepted, "{body}");
        if !accepted {
            assert!(fake.writes().is_empty());
        }
    }
    for at in ["2026-10-16T09:59:59Z", "2026-10-16T11:00:01Z"] {
        refuses(&s, actions()[2].clone(), |f| {
            let c = f
                .objects
                .get_mut("/repos/example/project/issues/comments/23")
                .unwrap();
            c["created_at"] = json!(at);
            c["updated_at"] = json!(at);
        });
    }
}

#[test]
fn supervisor_comments_never_reach_a_dispatched_run() {
    let s = configured();
    let comment = actions()[0].clone();
    let dispatch = request(json!({}));
    // Every target is checked before the first write, in either order.
    for batch in [
        [comment.clone(), dispatch.clone()],
        [dispatch.clone(), comment.clone()],
    ] {
        let mut fake = Fake::new(&block(&batch));
        evidence(&mut fake);
        let out = run(&fake, &s, RESULT_URL, 1);
        assert!(out.lines[0].contains("A comment and a dispatch on one item"));
        assert_eq!(fake.writes(), []);
    }
    // A run already started reads comments unchecked.
    refuses(&s, comment, |f| {
        f.objects.insert(
            "runs".into(),
            json!({"workflow_runs": [{"display_title": "implement example/project#3 (large)", "status": "in_progress"}]}),
        );
    });
    // A later pass's dispatch refuses the conversation the note is in.
    let mut fake = Fake::new(&block(std::slice::from_ref(&dispatch)));
    evidence(&mut fake);
    assert!(run(&fake, &s, RESULT_URL, 1).ok);
    refuses(&s, dispatch, |f| {
        f.issues.get_mut(&3).unwrap()["comments"]["nodes"][0]["body"] =
            json!(format!("Do this.\n\n{}", crate::input::SUPERVISOR_NOTE));
    });
}

#[test]
fn a_human_hold_or_any_other_pull_problem_cannot_be_cleared() {
    let s = configured();
    let r = actions()[3].clone();
    for (field, value) in [
        ("mergeable", json!("UNKNOWN")),
        ("reviewDecision", json!("CHANGES_REQUESTED")),
        ("isDraft", json!(true)),
        ("headRefOid", json!("new-head")),
        ("commits", json!({"nodes": []})),
        ("author", json!({"login": "visitor"})),
    ] {
        refuses(&s, r.clone(), |f| {
            f.objects.get_mut("conflict").unwrap()[field] = value;
        });
    }
    refuses(&s, r.clone(), |f| {
        f.objects.get_mut("conflict").unwrap()["commits"]["nodes"][0]["commit"]["statusCheckRollup"]
            ["state"] = json!("FAILURE");
    });
    for event in [
        json!([]),
        json!([{"event": "labeled", "label": {"name": "needs-human"}, "actor": {"login": "owner"}, "created_at": NOW}]),
    ] {
        refuses(&s, r.clone(), |f| {
            f.objects.insert(
                "/repos/example/project/issues/4/events?per_page=100&page=1".into(),
                event,
            );
        });
    }
    refuses(&s, r, |f| {
        f.objects.insert(
            "/repos/example/project/issues/4/comments?per_page=100&page=1".into(),
            json!([]),
        );
    });
}

#[test]
fn aggregate_green_cannot_hide_missing_required_checks_or_incomplete_evidence() {
    let s = configured();
    let r = actions()[3].clone();
    for required in [
        json!({"context": "never-reported"}),
        json!({"context": "ci", "integration_id": 99}),
    ] {
        refuses(&s, r.clone(), |f| {
            f.objects.insert("/repos/example/project/rules/branches/main?per_page=100&page=1".into(),
                json!([{"type": "required_status_checks", "parameters": {"required_status_checks": [required]}}]));
        });
    }
    refuses(&s, r.clone(), |f| {
        f.objects.insert(
            "/repos/example/project/branches/main".into(),
            json!({"protected": true, "protection": {
                "enabled": true, "required_status_checks": {"contexts": ["never-reported"]}
            }}),
        );
    });
    for value in [json!(true), Value::Null] {
        refuses(&s, r.clone(), |f| {
            f.objects.get_mut("conflict").unwrap()["commits"]["nodes"][0]["commit"]["statusCheckRollup"]
                ["contexts"]["pageInfo"]["hasNextPage"] = value;
        });
    }
    refuses(&s, r, |f| {
        f.objects.insert(
            "/repos/example/project/branches/main".into(),
            json!({"protected": true}),
        );
    });
}

#[test]
fn implement_requires_native_membership_under_an_epic_labeled_plan() {
    let s = configured();
    let r = request(json!({"kind": "implement"}));
    refuses(&s, r.clone(), |_| {});
    for (repo, labels, state) in [
        ("evil/plan", json!([{"name": "epic"}]), "open"),
        ("example/control", json!([]), "open"),
        ("example/control", json!([{"name": "epic"}]), "closed"),
    ] {
        refuses(&s, r.clone(), |f| {
            f.objects.insert("/repos/example/project/issues/3/parent".into(), json!({"repository_url": format!("https://api.github.com/repos/{repo}"), "number": 10}));
            f.objects.insert(
                "/repos/example/control/issues/10".into(),
                json!({"state": state, "labels": labels}),
            );
        });
    }
    let mut fake = Fake::new(&block(&[r]));
    fake.objects.insert(
        "/repos/example/project/issues/3/parent".into(),
        json!({"repository_url": "https://api.github.com/repos/example/control", "number": 10}),
    );
    fake.objects.insert(
        "/repos/example/control/issues/10".into(),
        json!({"state": "open", "labels": [{"name": "epic"}]}),
    );
    assert!(run(&fake, &s, RESULT_URL, 1).ok);
}
