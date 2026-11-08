use super::*;
use std::cell::RefCell;

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
