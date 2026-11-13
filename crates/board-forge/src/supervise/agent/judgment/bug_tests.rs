use super::*;
use std::cell::RefCell;

/// One stateful forge: three failed passes, issue creation, dispatch reservation
/// and a second supervisor pass all observe the same persisted writes.
#[derive(Default)]
struct Recovery {
    bug: RefCell<Option<Value>>,
    comments: RefCell<Vec<Value>>,
    dispatches: std::cell::Cell<usize>,
    fail_dispatch: bool,
}

impl Api for Recovery {
    fn get(&self, path: &str) -> Result<Value> {
        if path.contains("/workflows/poll.yml/runs?") {
            return Ok(json!({"workflow_runs":(1..=3).rev().map(|n| json!({
                "id":n,"status":"completed","conclusion":"failure",
                "updated_at":"2026-10-16T11:00:00Z",
                "html_url":format!("https://github.com/example/control/actions/runs/{n}")
            })).collect::<Vec<_>>()}));
        }
        if path.ends_with("/jobs?per_page=100") {
            return Ok(json!({"total_count":1,"jobs":[{"id":1,"conclusion":"failure"}]}));
        }
        if path.contains("/workflows/dispatch.yml/runs?status=") {
            return Ok(json!({"workflow_runs":[]}));
        }
        if path == "/repos/example/project/issues?state=open&per_page=100&page=1" {
            return Ok(json!(self.bug.borrow().iter().cloned().collect::<Vec<_>>()));
        }
        if path == "/repos/example/project/issues/7" {
            return self.bug.borrow().clone().context("no bug yet");
        }
        if path == "/repos/example/project/issues/7/comments?per_page=100&page=1" {
            return Ok(json!(*self.comments.borrow()));
        }
        bail!("unrecorded read {path}")
    }

    fn graphql(&self, _: &str, _: Value) -> Result<Value> {
        Ok(
            json!({"repository":{"issue":{"body":self.bug.borrow().as_ref().map(|b| &b["body"]),
            "comments":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}),
        )
    }

    fn download(&self, _: &str, _: u64) -> Result<Vec<u8>> {
        Ok(b"2026-10-16T11:00:00Z Error: stage 2 follow pull requests\n2026-10-16T11:00:01Z Caused by: request budget 500 exhausted".to_vec())
    }

    fn send(&self, _: &str, path: &str, body: &Value) -> Result<Value> {
        match path {
            "/repos/example/project/issues" => {
                let bug = json!({"number":7,"state":"open","labels":[],"user":{"login":"worker"},"body":body["body"]});
                *self.bug.borrow_mut() = Some(bug.clone());
                Ok(bug)
            }
            "/repos/example/project/issues/7/comments" => {
                self.comments
                    .borrow_mut()
                    .push(json!({"user":{"login":"worker"},"body":body["body"]}));
                Ok(json!({}))
            }
            "/repos/example/control/actions/workflows/dispatch.yml/dispatches" => {
                self.dispatches.set(self.dispatches.get() + 1);
                ensure!(!self.fail_dispatch, "dispatch failed after reservation");
                Ok(json!({}))
            }
            _ => bail!("unexpected write {path}"),
        }
    }
}

#[test]
fn three_failed_passes_file_one_owner_bug_and_request_only_one_fix() {
    for fail_dispatch in [false, true] {
        let api = Recovery {
            fail_dispatch,
            ..Recovery::default()
        };
        let mut s = configured();
        let agent = s.agent.as_mut().unwrap();
        agent.pass_failure_repository = Some("example/project".into());
        agent.kinds.push("implement".into());
        s.kind_pools.insert("implement".into(), "large".into());
        let now = millis(&json!("2026-10-16T12:00:00Z")).unwrap();
        let snapshot = crate::supervise::health::collect(&api, &s, "main", now, None);
        let report = crate::supervise::health::evaluate(&snapshot, now, &s.limits);
        let errors = report.iter().find(|c| c["id"] == "pass-errors").unwrap();
        assert_eq!(errors["status"], "fail");
        let signature = errors["signatures"][0]["signature"].as_str().unwrap();
        assert_eq!(errors["signatures"][0]["count"], 3);
        let p = Policy::new(&s, &json!({"small":{},"large":{}}))
            .unwrap()
            .unwrap();
        let line = json!({"type":"supervisor_action","action":"file_bug","repository":"example/project",
            "title":"Recurring pass error", "body":format!("Stage 2: {}. Runs: {}", errors["signatures"][0]["text"], errors["evidence"]),
            "signature":signature}).to_string();
        let mut bug = parse_batch(&line, &p).unwrap();
        bug[0].preflight(&api, &p, now).unwrap();
        let run = Run {
            profile_commit: None,
            source: Source::Comment(String::new()),
            run_url: String::new(),
            default_branch: "main".into(),
            attempt: 1,
            now,
        };
        bug[0].apply(&api, &p, &run).unwrap();
        let dispatch =
            json!({"type":"supervisor_action","action":"dispatch","repository":"example/project",
            "item":7,"kind":"implement","pool":"large","reason":"recurring-pass-failure"})
            .to_string();
        let mut fix = parse_batch(&dispatch, &p).unwrap();
        fix[0].preflight(&api, &p, now).unwrap();
        assert_eq!(fix[0].apply(&api, &p, &run).is_ok(), !fail_dispatch);
        assert_eq!(api.dispatches.get(), 1);
        // The next pass sees the bug and the reservation written by this one.
        assert!(bug[0].preflight(&api, &p, now).is_err());
        assert!(fix[0].preflight(&api, &p, now).is_err());
        assert_eq!(api.comments.borrow().len(), 1);
        assert_eq!(api.dispatches.get(), 1);
    }
}

#[derive(Default)]
struct Forge {
    issues: Value,
    held: bool,
    calls: RefCell<Vec<(String, Value)>>,
}

impl Api for Forge {
    fn get(&self, path: &str) -> Result<Value> {
        if path.ends_with("issues?state=open&per_page=100&page=1") {
            return Ok(self.issues.clone());
        }
        if path.ends_with("/issues/10") {
            return Ok(
                json!({"state":"open", "labels": if self.held { json!([{"name":"epic"},{"name":"needs-human"}]) } else { json!([{"name":"epic"}]) }}),
            );
        }
        if path.ends_with("/events?per_page=100&page=1") {
            return Ok(
                json!([{"event":"labeled", "label":{"name":"epic"}, "actor":{"login":"owner"}}]),
            );
        }
        bail!("unexpected GET {path}")
    }

    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        if query.starts_with("query") {
            return Ok(json!({"node":{"field":{"id":"F", "options":[{"id":"O", "name":"P1"}]}}}));
        }
        self.calls.borrow_mut().push((query.into(), variables));
        if query.contains("addProjectV2ItemById") {
            Ok(json!({"addProjectV2ItemById":{"item":{"id":"M"}}}))
        } else {
            Ok(json!({"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"M"}}}))
        }
    }

    fn send(&self, method: &str, path: &str, body: &Value) -> Result<Value> {
        self.calls
            .borrow_mut()
            .push((format!("{method} {path}"), body.clone()));
        if path.ends_with("/issues") {
            Ok(json!({"id":123, "node_id":"I", "number":7}))
        } else {
            Ok(json!({}))
        }
    }

    fn download(&self, _: &str, _: u64) -> Result<Vec<u8>> {
        bail!("no logs")
    }
}

fn configured() -> Settings {
    let mut cfg = crate::supervise::tests::supervisor();
    cfg["agent"]["tracker"] = json!("example/control");
    cfg["agent"]["project"] = json!("P");
    cfg["agent"]["bugEpic"] = json!(10);
    cfg["agent"]["dailyCaps"] = json!({"file_bug":10});
    Settings::from_json(&crate::supervise::tests::board(), &cfg, None).unwrap()
}

fn request(signature: &str) -> String {
    json!({"type":"supervisor_action", "action":"file_bug", "title":"Recurring apply failure", "body":"Two runs failed: evidence URLs.", "signature":signature}).to_string()
}

#[test]
fn pass_bugs_route_only_to_the_opted_in_owner_without_tracker_placement() {
    let mut s = configured();
    let line = |repo: &str| {
        json!({"type":"supervisor_action", "action":"file_bug",
        "repository":repo, "title":"Pass stage 2 failure", "body":"Three failed runs and a normalized excerpt.",
        "signature":"a".repeat(64)}).to_string()
    };
    let pools = json!({"small":{}, "large":{}});
    assert!(
        parse_batch(
            &line("example/project"),
            &Policy::new(&s, &pools).unwrap().unwrap()
        )
        .is_err()
    );
    s.agent.as_mut().unwrap().pass_failure_repository = Some("example/project".into());
    let p = Policy::new(&s, &pools).unwrap().unwrap();
    assert!(parse_batch(&line("evil/repo"), &p).is_err());
    let api = Forge {
        issues: json!([]),
        ..Forge::default()
    };
    let mut requests = parse_batch(&line("example/project"), &p).unwrap();
    requests[0].preflight(&api, &p, 0).unwrap();
    let run = Run {
        profile_commit: None,
        source: Source::Comment(String::new()),
        run_url: String::new(),
        default_branch: "main".into(),
        attempt: 1,
        now: 0,
    };
    let receipt = requests[0].apply(&api, &p, &run).unwrap();
    assert_eq!(
        requests[0].link(&p, Some(&receipt)),
        "https://github.com/example/project/issues/7"
    );
    let calls = api.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "POST /repos/example/project/issues");
    assert!(!crate::input::is_supervisor_note(
        calls[0].1["body"].as_str().unwrap()
    ));
}

#[test]
fn filed_bug_arrives_under_fixed_epic_on_board_at_p1() {
    let s = configured();
    let p = Policy::new(&s, &json!({"small":{}, "large":{}}))
        .unwrap()
        .unwrap();
    let api = Forge {
        issues: json!([]),
        ..Forge::default()
    };
    let signature = "a".repeat(64);
    let mut requests = parse_batch(&request(&signature), &p).unwrap();
    requests[0].preflight(&api, &p, 0).unwrap();
    assert!(api.calls.borrow().is_empty());
    let run = Run {
        profile_commit: None,
        source: Source::Comment(String::new()),
        run_url: String::new(),
        default_branch: "main".into(),
        attempt: 1,
        now: 0,
    };
    requests[0].apply(&api, &p, &run).unwrap();
    let calls = api.calls.borrow();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0].0, "POST /repos/example/control/issues");
    let body = calls[0].1["body"].as_str().unwrap();
    assert!(body.contains(&bug_marker(&signature)));
    assert!(!crate::input::is_supervisor_note(body));
    assert!(calls[0].1["assignees"].is_null() && calls[0].1["labels"].is_null());
    assert_eq!(
        calls[1],
        (
            "POST /repos/example/control/issues/10/sub_issues".into(),
            json!({"sub_issue_id":123})
        )
    );
    assert_eq!(calls[2].1, json!({"project":"P", "content":"I"}));
    assert_eq!(
        calls[3].1,
        json!({"project":"P", "item":"M", "field":"F", "option":"O"})
    );
}

#[test]
fn duplicate_bug_and_incomplete_or_held_evidence_refuse_before_writes() {
    let s = configured();
    let p = Policy::new(&s, &json!({"small":{}, "large":{}}))
        .unwrap()
        .unwrap();
    let signature = "a".repeat(64);
    let request = request(&signature);
    assert!(parse_batch(&format!("{request}\n{request}"), &p).is_err());
    for (issues, held, allowed) in [
        (
            json!([{"user":{"login":"worker"},"body":bug_marker(&signature)}]),
            false,
            false,
        ),
        (json!(null), false, false),
        (json!([]), true, false),
        (
            json!([{"user":{"login":"visitor"},"body":bug_marker(&signature)}]),
            false,
            true,
        ),
        (json!([]), false, true),
    ] {
        let api = Forge {
            issues,
            held,
            ..Forge::default()
        };
        let mut requests = parse_batch(&request, &p).unwrap();
        assert_eq!(requests[0].preflight(&api, &p, 0).is_ok(), allowed);
        assert!(api.calls.borrow().is_empty());
    }
    for signature in ["", "abc", &"A".repeat(64)] {
        assert!(parse_batch(&self::request(signature), &p).is_err());
    }
}
