use super::*;
use std::cell::RefCell;

struct Fixture {
    values: BTreeMap<String, Value>,
    log: Option<String>,
    logs: BTreeMap<String, String>,
    calls: RefCell<Vec<String>>,
}

impl Api for Fixture {
    fn get(&self, path: &str) -> Result<Value> {
        self.calls.borrow_mut().push(path.into());
        self.values
            .get(path)
            .cloned()
            .with_context(|| format!("unavailable fixture {path}"))
    }
    fn download(&self, path: &str, limit: u64) -> Result<Vec<u8>> {
        assert!((1..=LOG_BYTES).contains(&limit));
        self.calls.borrow_mut().push(path.into());
        self.logs
            .get(path)
            .or(self.log.as_ref())
            .map(|s| s.as_bytes()[..s.len().min(limit as usize)].to_vec())
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
    Fixture { values, logs: BTreeMap::new(), calls: RefCell::new(vec![]), log: Some("2026-10-10T01:00:00Z ---- test_42 stdout ----\n2026-10-10T01:00:01Z error: expected 42 got 43\n2026-10-10T01:00:02Z ##[error]Process completed with exit code 1.".into()) }
}

fn multi(count: usize, expensive: Vec<Value>) -> (Fixture, Config) {
    let mut f = fixture(vec![]);
    f.values.clear();
    let mut cfg = config();
    cfg.targets.clear();
    for i in 0..count {
        let repository = format!("o/r{i:02}");
        cfg.targets.push(Target {
            repository: repository.clone(),
            workflow: "ci.yml".into(),
        });
        let runs = if i == 0 {
            expensive.clone()
        } else {
            vec![run(
                9000 + i as u64,
                until() - DAY,
                "push",
                "main",
                "success",
                2,
            )]
        };
        let mut target = fixture(runs.clone());
        for run in &runs {
            let id = run["id"].as_u64().unwrap();
            target.values.insert(
                format!("/repos/o/r/actions/runs/{id}/attempts/1"),
                self::run(id, until() - DAY, "push", "main", "failure", 1),
            );
        }
        f.values
            .extend(target.values.into_iter().map(|(path, value)| {
                (
                    path.replacen("/repos/o/r", &format!("/repos/{repository}"), 1),
                    value,
                )
            }));
    }
    (f, cfg)
}

fn population(target: &Value) -> &Value {
    &target["populations"]["default_push"]
}

#[test]
fn all_busy_targets_stop_at_the_unchanged_global_request_cap() {
    let (mut f, cfg) = multi(
        20,
        vec![run(1, until() - DAY, "push", "main", "failure", 100)],
    );
    let first: Vec<_> = f
        .values
        .iter()
        .filter(|(p, _)| p.starts_with("/repos/o/r00"))
        .map(|(p, v)| (p.clone(), v.clone()))
        .collect();
    for i in 1..20 {
        for (path, value) in &first {
            f.values
                .insert(path.replacen("r00", &format!("r{i:02}"), 1), value.clone());
        }
    }
    let report = collect(&f, &cfg, until(), &|| 0);
    assert_eq!(f.calls.borrow().len(), REQUESTS as usize);
    for target in report["targets"].as_array().unwrap() {
        assert_eq!(target["listing_complete"], true);
        assert_eq!(target["complete"], false);
        assert_eq!(population(target)["expected_attempts"], 100);
        assert_eq!(population(target)["attempts"], 2);
        assert_eq!(population(target)["first_attempt_failed"], 1);
    }
}

#[test]
fn expensive_first_target_cannot_starve_nineteen_others_or_depend_on_config_order() {
    // Regression shape from tracker#1325: one busy target, nineteen later ones.
    let (f, mut cfg) = multi(
        20,
        (1..=100)
            .map(|id| run(id, until() - DAY, "push", "main", "failure", 100))
            .collect(),
    );
    let first = collect(&f, &cfg, until(), &|| 0);
    assert_eq!(first["complete"], false);
    assert_eq!(population(&first["targets"][0])["runs"], 100);
    assert_eq!(population(&first["targets"][0])["expected_attempts"], 10000);
    for target in first["targets"].as_array().unwrap().iter().skip(1) {
        assert_eq!(target["complete"], true, "{target}");
        assert_eq!(population(target)["attempts"], 2);
        assert_eq!(population(target)["first_attempt_failed"], 1);
        assert_eq!(population(target)["first_attempt_denominator"], 1);
        assert_eq!(population(target)["denominator"], 2);
        assert_eq!(population(target)["pass_rate"], 0.5);
    }
    let calls = f.calls.borrow().clone();
    assert!(calls.len() <= REQUESTS as usize);
    for target in &cfg.targets {
        let prefix = format!("/repos/{}", target.repository);
        assert!(calls.iter().filter(|p| p.starts_with(&prefix)).count() <= REQUESTS as usize / 20);
    }
    let first_deep = calls.iter().position(|p| p.contains("/attempts/")).unwrap();
    assert_eq!(
        first_deep, 40,
        "all metadata and listings precede enrichment"
    );
    cfg.targets.reverse();
    f.calls.borrow_mut().clear();
    let mut reversed = collect(&f, &cfg, until(), &|| 0);
    reversed["targets"].as_array_mut().unwrap().reverse();
    assert_eq!(first, reversed);
    assert_eq!(calls, *f.calls.borrow());
}

#[test]
fn breadth_first_pages_retain_partial_census_at_ten_page_limit() {
    let (mut f, cfg) = multi(2, vec![]);
    for page in 1..=PAGES {
        let runs: Vec<_> = (1..=100)
            .map(|n| {
                run(
                    ((page - 1) * 100 + n) as u64,
                    until() - DAY,
                    "push",
                    "main",
                    "success",
                    2,
                )
            })
            .collect();
        f.values.insert(format!("/repos/o/r00/actions/workflows/ci.yml/runs?created=2026-10-02..2026-11-01&per_page=100&page={page}"), json!({"total_count":1001,"workflow_runs":runs}));
    }
    let report = collect(&f, &cfg, until(), &|| 0);
    let target = &report["targets"][0];
    assert_eq!(target["listing_complete"], false);
    assert_eq!(target["complete"], false);
    let counts = population(target);
    assert_eq!(counts["runs"], 1000);
    assert_eq!(counts["expected_attempts"], 2000);
    assert_eq!(counts["attempts"], 1000);
    assert_eq!(counts["denominator"], 1000);
    assert_eq!(counts["first_attempt_denominator"], 0);
    assert_eq!(counts["first_attempt_pass_rate"], Value::Null);
    assert_eq!(report["targets"][1]["complete"], true);
    let calls = f.calls.borrow();
    assert!(calls[2].contains("r00/actions/workflows"));
    assert!(calls[3].contains("r01/actions/workflows"));
    assert!(calls[4].ends_with("page=2"));
    assert!(!calls.iter().any(|p| p.ends_with("page=11")));
}

#[test]
fn partial_listing_survives_later_page_errors() {
    for case in ["missing", "changed", "duplicate", "empty"] {
        let (mut f, cfg) = multi(2, vec![run(1, until() - DAY, "push", "main", "success", 1)]);
        let path = "/repos/o/r00/actions/workflows/ci.yml/runs?created=2026-10-02..2026-11-01&per_page=100&page=";
        f.values.get_mut(&format!("{path}1")).unwrap()["total_count"] = json!(2);
        match case {
            "changed" => {
                f.values.insert(
                    format!("{path}2"),
                    json!({"total_count":3,"workflow_runs":[]}),
                );
            }
            "duplicate" => {
                f.values.insert(format!("{path}2"), json!({"total_count":2,"workflow_runs":[run(1, until() - DAY, "push", "main", "success", 1)]}));
            }
            "empty" => {
                f.values.insert(
                    format!("{path}2"),
                    json!({"total_count":2,"workflow_runs":[]}),
                );
            }
            _ => (),
        }
        let report = collect(&f, &cfg, until(), &|| 0);
        assert_eq!(report["targets"][0]["listing_complete"], false, "{case}");
        assert_eq!(population(&report["targets"][0])["runs"], 1);
        assert_eq!(population(&report["targets"][0])["attempts"], 1);
        assert_eq!(report["targets"][1]["complete"], true);
    }
}

#[test]
fn time_bytes_and_inaccessible_logs_leave_honest_multi_target_counts() {
    for case in [
        "target_time",
        "deadline",
        "target_bytes",
        "aggregate_bytes",
        "logs",
    ] {
        let targets = if case == "aggregate_bytes" { 20 } else { 2 };
        let (mut f, cfg) = multi(
            targets,
            (1..=100)
                .map(|id| run(id, until() - DAY, "push", "main", "failure", 1))
                .collect(),
        );
        if case == "aggregate_bytes" {
            let first: Vec<_> = f
                .values
                .iter()
                .filter(|(p, _)| p.starts_with("/repos/o/r00"))
                .map(|(p, v)| (p.clone(), v.clone()))
                .collect();
            for i in 1..targets {
                for (path, value) in &first {
                    f.values
                        .insert(path.replacen("r00", &format!("r{i:02}"), 1), value.clone());
                }
            }
            // JSON bodies have no caller-supplied download limit. A refused
            // oversized decoded response must still charge the aggregate cap.
            for (_, value) in f
                .values
                .iter_mut()
                .filter(|(p, _)| p.contains("/runs/1/attempts/1/jobs?"))
            {
                value["padding"] = json!("x".repeat(2 * LOG_BYTES as usize));
            }
        }
        if case == "logs" {
            f.log = None;
        }
        if case.ends_with("bytes") {
            f.log = Some(format!("error: {}", "x".repeat(LOG_BYTES as usize - 9)));
        }
        let clock = || {
            let calls = f.calls.borrow();
            if case == "deadline" && calls.len() >= 3 {
                return 120_000;
            }
            if case == "target_time" {
                return calls
                    .iter()
                    .filter(|p| p.contains("/r00/actions/jobs/"))
                    .count() as i64
                    * 30_000;
            }
            0
        };
        let report = collect(&f, &cfg, until(), &clock);
        assert_eq!(report["complete"], false, "{case}: {report}");
        assert_eq!(report["targets"][0]["listing_complete"], true);
        assert_eq!(population(&report["targets"][0])["runs"], 100);
        assert_eq!(population(&report["targets"][0])["attempts"], 100);
        assert_eq!(population(&report["targets"][0])["failed"], 100);
        assert_eq!(
            population(&report["targets"][0])["first_attempt_denominator"],
            100
        );
        if case == "deadline" {
            assert_eq!(report["targets"][1]["listing_complete"], false);
            assert_eq!(population(&report["targets"][1])["attempts"], 0);
            assert_eq!(population(&report["targets"][1])["pass_rate"], Value::Null);
        } else if case == "aggregate_bytes" {
            assert!(report["decoded_bytes"].as_u64().unwrap() > TOTAL_BYTES as u64);
            assert!(
                report["decoded_bytes"].as_u64().unwrap() < (TOTAL_BYTES as u64 + 3 * LOG_BYTES)
            );
            for target in report["targets"].as_array().unwrap() {
                assert_eq!(target["listing_complete"], true);
                assert_eq!(target["complete"], false);
                assert_eq!(population(target)["attempts"], 100);
            }
        } else {
            assert_eq!(report["targets"][1]["listing_complete"], true);
            assert_eq!(population(&report["targets"][1])["attempts"], 2);
            assert_eq!(report["targets"][1]["complete"], case != "logs");
        }
        assert!(f.calls.borrow().len() <= REQUESTS as usize);
    }
}

#[test]
fn heavy_early_logs_cannot_spend_later_targets_byte_shares() {
    let (mut f, cfg) = multi(20, vec![]);
    for i in 0..16 {
        let target = fixture(vec![
            run(1, until() - DAY, "push", "main", "failure", 1),
            run(2, until() - DAY, "push", "main", "failure", 1),
        ]);
        f.values.extend(
            target
                .values
                .into_iter()
                .map(|(p, v)| (p.replacen("/repos/o/r", &format!("/repos/o/r{i:02}"), 1), v)),
        );
        for id in [101, 201] {
            f.logs.insert(
                format!("/repos/o/r{i:02}/actions/jobs/{id}/logs"),
                format!("error: {}", "x".repeat(LOG_BYTES as usize - 9)),
            );
        }
    }
    let report = collect(&f, &cfg, until(), &|| 0);
    assert!(report["decoded_bytes"].as_u64().unwrap() < TOTAL_BYTES as u64);
    for (i, target) in report["targets"].as_array().unwrap().iter().enumerate() {
        assert_eq!(target["listing_complete"], true);
        assert_eq!(target["complete"], i >= 16, "{target}");
        if i < 16 {
            assert_eq!(population(target)["failed"], 2);
        } else {
            assert_eq!(population(target)["attempts"], 2);
            assert_eq!(population(target)["first_attempt_failed"], 1);
        }
    }
    // Each early target contributes only its first full log, not the second
    // allocation-truncated error (even though that prefix looks meaningful).
    for group in report["groups"].as_array().unwrap() {
        assert_eq!(group["observation"]["occurrences"], 1);
    }
    assert_eq!(report["groups"].as_array().unwrap().len(), 20);
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
