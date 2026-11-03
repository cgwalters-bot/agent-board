use super::*;

struct Fixture {
    values: BTreeMap<String, Value>,
    log: Option<String>,
}

impl Api for Fixture {
    fn get(&self, path: &str) -> Result<Value> {
        self.values
            .get(path)
            .cloned()
            .with_context(|| format!("unavailable fixture {path}"))
    }
    fn download(&self, _: &str, _: u64) -> Result<Vec<u8>> {
        self.log
            .as_ref()
            .map(|s| s.as_bytes().to_vec())
            .context("logs expired")
    }
    fn graphql(&self, _: &str, _: Value) -> Result<Value> {
        panic!("unexpected GraphQL")
    }
    fn send(&self, _: &str, _: &str, _: &Value) -> Result<Value> {
        panic!("unexpected write")
    }
}

fn until() -> i64 {
    millis(&json!("2026-11-01T00:00:00Z")).unwrap()
}

fn config() -> Config {
    Config::parse(
        r#"{"targets":[{"repository":"o/r","workflow":"ci.yml"}]}"#,
        until(),
    )
    .unwrap()
}

fn run(id: u64, at: i64, event: &str, branch: &str, conclusion: &str, attempt: u64) -> Value {
    json!({"id":id,"created_at":iso(at),"event":event,"head_branch":branch,
        "conclusion":conclusion,"status":if conclusion == "active" {"in_progress"} else {"completed"},
        "run_attempt":attempt,"html_url":format!("https://github.com/o/r/actions/runs/{id}")})
}

fn fixture(runs: Vec<Value>) -> Fixture {
    let mut values = BTreeMap::from([
        ("/repos/o/r".into(), json!({"default_branch":"main"})),
        ("/repos/o/r/actions/workflows/ci.yml/runs?created=2026-10-02..2026-11-01&per_page=100&page=1".into(), json!({"total_count":runs.len(),"workflow_runs":runs})),
    ]);
    for run in &runs {
        let id = run["id"].as_u64().unwrap();
        for attempt in 1..=run["run_attempt"].as_u64().unwrap() {
            values.insert(format!("/repos/o/r/actions/runs/{id}/attempts/{attempt}/jobs?per_page=100&page=1"), json!({"total_count":1,"jobs":[{"id":id*100+attempt,"name":"rust","conclusion":"failure"}]}));
        }
    }
    Fixture { values, log: Some("2026-10-10T01:00:00Z ---- test_42 stdout ----\n2026-10-10T01:00:01Z error: expected 42 got 43\n2026-10-10T01:00:02Z ##[error]Process completed with exit code 1.".into()) }
}

#[test]
fn boundaries_populations_attempts_and_denominators() {
    let end = until();
    let start = end - 30 * DAY;
    let mut f = fixture(vec![
        run(1, start, "push", "main", "success", 2),
        run(2, start - 1, "push", "main", "success", 1),
        run(3, end, "push", "main", "failure", 1),
        run(4, start, "pull_request", "feature", "success", 1),
        run(5, start, "pull_request_target", "main", "success", 1),
        run(6, start, "push", "feature", "success", 1),
        run(7, start, "push", "main", "cancelled", 1),
        run(8, start, "push", "main", "skipped", 1),
        run(9, start, "push", "main", "active", 1),
        run(10, start, "push", "main", "neutral", 1),
    ]);
    f.values.insert(
        "/repos/o/r/actions/runs/1/attempts/1".into(),
        run(1, start, "push", "main", "failure", 1),
    );
    let report = collect(&f, &config(), end, &|| 0);
    assert_eq!(report["complete"], true, "{report:#}");
    let counts = &report["targets"][0]["populations"]["default_push"];
    assert_eq!(counts["runs"], 5);
    assert_eq!(counts["attempts"], 6);
    assert_eq!(counts["denominator"], 2);
    assert_eq!(counts["pass_rate"], 0.5);
    assert_eq!(counts["first_attempt_failed"], 1);
    assert_eq!(counts["first_attempt_pass_rate"], 0.0);
    for field in ["cancelled", "skipped", "in_progress", "other"] {
        assert_eq!(counts[field], 1);
    }
    assert_eq!(
        report["targets"][0]["populations"]["pull_request"]["runs"],
        1
    );
    let group = &report["groups"][0];
    assert_eq!(group["class"], "unknown");
    assert_eq!(group["observation"]["test"], "test_42");
    assert_eq!(
        group["observation"]["first_error"],
        "error: expected 42 got 43"
    );
    assert_eq!(group["observation"]["samples"][0]["attempt"], 1);
}

#[test]
fn recurrence_index_and_seven_day_boundary() {
    let f = fixture(vec![
        run(1, until() - DAY, "push", "main", "failure", 1),
        run(2, until() - 2 * DAY, "pull_request", "pr", "failure", 1),
    ]);
    let mut cfg = config();
    let report = collect(&f, &cfg, until(), &|| 0);
    assert_eq!(report["groups"].as_array().unwrap().len(), 1);
    assert_eq!(
        report["groups"][0]["findings"],
        json!(["recurring_without_issue"])
    );
    let signature = report["groups"][0]["observation"]["signature"]
        .as_str()
        .unwrap()
        .to_owned();
    for (age, explained, expected) in [
        (7 * DAY, false, false),
        (7 * DAY + 1, false, true),
        (8 * DAY, true, false),
    ] {
        cfg.index.insert(
            signature.clone(),
            Classification {
                class: "flake".into(),
                evidence: vec!["https://github.com/o/r/actions/runs/1".into()],
                owner: "owner".into(),
                issue: Some("https://github.com/o/r/issues/1".into()),
                fix_status: "staged".into(),
                first_seen: iso(until() - age),
                explained_at: explained.then(|| iso(until() - DAY)),
                explanation: explained.then(|| "root cause reviewed".into()),
            },
        );
        let report = collect(&f, &cfg, until(), &|| 0);
        assert_eq!(
            !report["groups"][0]["findings"]
                .as_array()
                .unwrap()
                .is_empty(),
            expected
        );
        assert_eq!(
            report["groups"][0]["classification"]["fix_status"],
            "staged"
        );
    }
}

#[test]
fn missing_history_pages_logs_and_attempts_never_look_complete() {
    let base = run(1, until() - DAY, "push", "main", "failure", 1);
    for case in [
        "repository",
        "pages",
        "logs",
        "attempt",
        "empty",
        "jobs",
        "deadline",
    ] {
        let mut f = fixture(vec![base.clone()]);
        match case {
            "repository" => {
                f.values.clear();
            }
            "pages" => {
                for value in f
                    .values
                    .values_mut()
                    .filter(|v| v.get("workflow_runs").is_some())
                {
                    value["total_count"] = json!(101);
                }
            }
            "logs" => f.log = None,
            "attempt" => {
                for value in f
                    .values
                    .values_mut()
                    .filter(|v| v.get("workflow_runs").is_some())
                {
                    value["workflow_runs"][0]["run_attempt"] = json!(2);
                }
            }
            "empty" => f = fixture(vec![]),
            "jobs" => {
                for value in f.values.values_mut().filter(|v| v.get("jobs").is_some()) {
                    value["total_count"] = json!(101);
                }
            }
            _ => (),
        }
        let calls = Cell::new(0);
        let clock = || {
            let n = calls.get();
            calls.set(n + 1);
            if case == "deadline" && n > 0 {
                999_999
            } else {
                0
            }
        };
        let report = collect(&f, &config(), until(), &clock);
        assert_eq!(report["complete"], false, "{case}: {report}");
        assert!(
            !report["targets"][0]["warnings"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn numeric_errors_are_distinct_and_generic_exit_is_not_first_error() {
    assert_ne!(
        first_error("error: test 123 failed"),
        first_error("error: test 456 failed")
    );
    assert_eq!(
        first_error("##[error]Process completed with exit code 1."),
        None
    );
    assert_eq!(
        first_error("\x1b[31merror[E0123]: bad\x1b[0m").unwrap().1,
        "error[E0123]: bad"
    );
}

#[test]
fn validates_index_and_scope() {
    let signature = "a".repeat(64);
    let entry = json!({"class":"flake","owner":"owner","evidence":["https://github.com/o/r/actions/runs/1"],"issue":null,"fix_status":"staged","first_seen":"2026-10-01T00:00:00Z","explained_at":null,"explanation":null});
    let mut config = json!({"targets":[{"repository":"o/r","workflow":"ci.yml"}],"index":{signature.clone():entry.clone()}});
    assert!(Config::parse(&config.to_string(), until()).is_ok());
    for (key, value) in [
        ("class", json!("invented")),
        ("owner", json!("")),
        ("evidence", json!([])),
        ("fix_status", json!("fixed")),
        ("first_seen", json!("2027-01-01T00:00:00Z")),
        ("explained_at", json!("2026-10-02T00:00:00Z")),
        ("extra", json!(true)),
    ] {
        config["index"][&signature] = entry.clone();
        config["index"][&signature][key] = value;
        assert!(
            Config::parse(&config.to_string(), until()).is_err(),
            "{key}"
        );
    }
    config["index"] = json!({});
    config["targets"][0]["workflow"] = json!("../bad");
    assert!(Config::parse(&config.to_string(), until()).is_err());
}
