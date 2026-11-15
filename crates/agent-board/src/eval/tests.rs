//! Synthetic offline data only: no historical evidence or measured model usage.
//! Ported from cgwalters-forge/agenthome's tests/eval.rs and tests/json.rs.

use super::*;
use serde_json::json;

const MANIFEST: &str = include_str!("../../../../eval/manifest.json");

const RUNS: &str = include_str!("../../../../eval/runs.json");

const SUMMARY: &str = include_str!("../../../../eval/summary.json");

const PREFLIGHT: &str = include_str!("../../../../eval/preflight.json");

const ARM_BUNDLES: &str = include_str!("../../../../eval/arms.json");

struct Fixture {
    manifest: Value,
    runs: Vec<Value>,
}

impl Fixture {
    fn new() -> Self {
        let tasks: Vec<_> = ["fix-a", "implement-b"]
            .map(|id| {
                json!({
                    "id": id, "repository": "fixture/repository",
                    "history": format!("fixture://history/{id}"),
                    "startCommit": "1".repeat(40), "taskText": format!("Synthetic common task {id}"),
                    "checks": {"artifact": format!("fixture://checks/{id}"), "sha256": "2".repeat(64)},
                    "sizingEvidence": format!("fixture://sizing/{id}")
                })
            })
            .into();
        let arms: serde_json::Map<_, _> = ARMS
            .iter()
            .enumerate()
            .map(|(i, arm)| {
                let render = json!({
                    "artifact": format!("fixture://render/{arm}"),
                    "sha256": (i + 5).to_string().repeat(64),
                    "resolvedModels": format!("fixture-resolved-map-{arm}")
                });
                (arm.to_string(), render)
            })
            .collect();
        let manifest = json!({
            "version": 1, "status": "frozen",
            "preflight": {"authorization": "fixture://authorization", "evidence": "fixture://sizing"},
            "tasks": tasks,
            "configuration": {
                "configRevision": "3".repeat(40), "harnessRevision": "4".repeat(40),
                "environment": "fixture-image@immutable", "roleInterface": "fixture://renderer-interface",
                "costBasis": {"artifact": "fixture://prices-usd-v1", "sha256": "8".repeat(64)},
                "arms": arms
            },
            "arms": ARMS, "repetitions": 3,
            "cachePolicies": ["cold", "warm"],
            "limits": {"wallSeconds": 1800, "usd": 5, "turns": 100, "tokens": 200000},
            "decisionRule": "quality-cost-time-v1"
        });
        let digest = manifest_digest(&manifest).unwrap();
        let c = &manifest["configuration"];
        let mut runs = Vec::new();
        for t in manifest["tasks"].as_array().unwrap() {
            for arm in ARMS {
                for policy in ["cold", "warm"] {
                    for repetition in 1..=3 {
                        let id =
                            format!("{}-{arm}-{policy}-{repetition}", t["id"].as_str().unwrap());
                        runs.push(json!({
                            "version": 1, "id": id, "manifestSha256": digest,
                            "runUrl": format!("fixture://runs/{id}"),
                            "artifactUrl": format!("fixture://artifacts/{id}"),
                            "taskId": t["id"], "arm": arm, "cachePolicy": policy,
                            "repetition": repetition, "startCommit": t["startCommit"],
                            "taskText": t["taskText"], "checksSha256": t["checks"]["sha256"],
                            "configRevision": c["configRevision"], "harnessRevision": c["harnessRevision"],
                            "environment": c["environment"], "renderSha256": c["arms"][arm]["sha256"],
                            "resolvedModels": c["arms"][arm]["resolvedModels"], "costBasis": c["costBasis"],
                            "telemetry": {"status": "complete"},
                            "metrics": {
                                "inputTokens": {"value": 100}, "outputTokens": {"value": 20},
                                "cacheReadTokens": {"value": if policy == "warm" { 50 } else { 0 }},
                                "cacheWriteTokens": {"value": 10},
                                "costUsd": {"value": if arm == "single-strong" { 1.0 } else { 0.5 }},
                                "wallSeconds": {"value": 100}
                            },
                            "stageSeconds": {"unavailable": "fixture runner does not expose stages"},
                            "outcome": "passed", "failures": [], "testEvidence": format!("fixture://tests/{id}")
                        }));
                    }
                }
            }
        }
        Self { manifest, runs }
    }

    fn evaluate(&self) -> Result<Value> {
        let runs = Value::Array(self.runs.clone());
        Ok(serde_json::to_value(summarize(&self.manifest, &runs)?)?)
    }

    fn redigest(&mut self) {
        let digest = manifest_digest(&self.manifest).unwrap();
        for run in &mut self.runs {
            run["manifestSha256"] = json!(digest);
        }
    }

    fn find(&mut self, arm: &str, policy: Option<&str>) -> &mut Value {
        self.runs
            .iter_mut()
            .find(|r| r["arm"] == arm && policy.is_none_or(|p| r["cachePolicy"] == p))
            .unwrap()
    }
}

fn verdicts(result: &Value) -> Vec<&str> {
    result["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["verdict"].as_str().unwrap())
        .collect()
}

fn group<'a>(result: &'a Value, arm: &str, policy: &Value) -> &'a Value {
    result["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["arm"] == arm && &g["cachePolicy"] == policy)
        .unwrap()
}

#[test]
fn checked_in_sample_stays_pending_without_fabricated_evidence() {
    let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
    let summary = serde_json::to_value(summarize(&manifest, &json!([])).unwrap()).unwrap();
    assert_eq!(summary["status"], "pending");
    let runs = Value::Array(Fixture::new().runs);
    let error = summarize(&manifest, &runs).unwrap_err();
    assert!(error.to_string().contains("before freezing"));
}

/// The archived summary is byte for byte what agenthome's summarizer printed
/// for the same pending inputs, trailing newline included.
#[test]
fn archived_preflight_summary_matches_pending_inputs_without_a_measured_verdict() {
    let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
    let runs: Value = serde_json::from_str(RUNS).unwrap();
    let preflight: Value = serde_json::from_str(PREFLIGHT).unwrap();
    assert_eq!(runs, json!([]));
    let mut out = Vec::new();
    let summary = summarize(&manifest, &runs).unwrap();
    writeln!(out, "{}", pretty_json(&summary).unwrap()).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), SUMMARY);
    assert_eq!(preflight["status"], "blocked");
    assert_eq!(preflight["capacity"]["status"], "unverified");
    assert_eq!(
        preflight["evaluationCellsLaunched"],
        runs.as_array().unwrap().len()
    );
    assert_eq!(preflight["paidProbesLaunched"], 0);
    assert_eq!(
        preflight["tasksFrozen"],
        manifest["tasks"].as_array().unwrap().len()
    );
    assert_eq!(preflight["measuredVerdict"], Value::Null);
}

/// The arm bundles are references into agenthome, not copies: no prompt or
/// model ID, and every arm draws on the same base configuration files.
#[test]
fn arm_bundles_reference_agenthome_role_files_without_copying_them() {
    let bundles: Value = serde_json::from_str(ARM_BUNDLES).unwrap();
    assert_eq!(bundles["source"]["repository"], "cgwalters-forge/agenthome");
    let arms = bundles["arms"].as_object().unwrap();
    assert!(arms.keys().eq(ARMS));
    for arm in ARMS {
        let bundle = arms[arm].as_object().unwrap();
        assert!(text(bundle.get("assembly")), "{arm}");
        assert!(!bundle.contains_key("prompt") && !bundle.contains_key("model"));
    }
    assert_eq!(arms["single-strong"]["tier"], "sol");
    assert_eq!(arms["single-cheap"]["tier"], "luna");
    assert_eq!(arms["tiered"]["roles"], "roles/opencode/eval-tiered.json");
    // Model IDs and role tiers stay in agenthome's files.
    assert!(!ARM_BUNDLES.contains("praxis/") && !ARM_BUNDLES.contains("role-"));
}

/// Digests computed by agenthome's JavaScript `manifestDigest`, so recorded
/// runs stay valid.
#[test]
fn manifest_digest_matches_javascript_and_ignores_key_order() {
    let manifest = Fixture::new().manifest;
    assert_eq!(
        manifest_digest(&manifest).unwrap(),
        "c0acd25d70b88e46164d89b0e7fb69d21ed869a514fb8fa43cfb563e757e7a47"
    );
    let reordered: serde_json::Map<_, _> = manifest
        .as_object()
        .unwrap()
        .iter()
        .rev()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        manifest_digest(&Value::Object(reordered)).unwrap(),
        manifest_digest(&manifest).unwrap()
    );
    // Number spelling and JavaScript's array-index keys first.
    let input: Value =
        serde_json::from_str(r#"{"b": 1.0, "a": [2, {"10": 1, "9": 2, "x": 3}]}"#).unwrap();
    assert_eq!(
        manifest_digest(&input).unwrap(),
        "65211c536e345fb524eaa2859ed4ce7a0ed177fd6a23bfcc9c2db9d50c1199b4"
    );
}

#[test]
fn numbers_use_javascript_spelling() {
    for (input, expected) in [
        ("2.0", "2"),
        ("-0.0", "0"),
        ("0.1", "0.1"),
        ("-1.5", "-1.5"),
        ("1e-7", "1e-7"),
        ("1.25e-7", "1.25e-7"),
        ("1e-6", "0.000001"),
        ("123.456", "123.456"),
        ("1e20", "100000000000000000000"),
        ("1e21", "1e+21"),
        ("1.5e300", "1.5e+300"),
        ("5e-324", "5e-324"),
    ] {
        let value: Value = serde_json::from_str(input).unwrap();
        assert_eq!(js_number(value.as_f64().unwrap()), expected, "{input}");
        assert_eq!(
            pretty_json(&Canonical(&value)).unwrap(),
            expected,
            "{input}"
        );
    }
}

#[test]
fn json_keys_follow_javascript_order() {
    let mut keys = [
        "b",
        "10",
        "a",
        "9",
        "01",
        "4294967295",
        "\u{ff61}",
        "\u{1f600}",
    ];
    keys.sort_by(|left, right| javascript_key_order(left, right));
    // 2^32 - 1 is not an array index; UTF-16 puts a surrogate pair before U+FF61.
    assert_eq!(
        keys,
        [
            "9",
            "10",
            "01",
            "4294967295",
            "a",
            "b",
            "\u{1f600}",
            "\u{ff61}"
        ]
    );
}

#[test]
fn complete_matrix_aggregates_each_stratum_and_applies_the_rule() {
    let result = Fixture::new().evaluate().unwrap();
    assert_eq!(result["status"], "complete");
    assert_eq!(result["groups"].as_array().unwrap().len(), 6);
    assert_eq!(result["groups"][0]["metrics"]["costUsd"]["value"], 6.0);
    assert_eq!(
        result["groups"][0]["metrics"]["cacheReadTokens"]["value"],
        0.0
    );
    assert_eq!(
        result["groups"][3]["metrics"]["cacheReadTokens"]["value"],
        300.0
    );
    assert_eq!(verdicts(&result), ["eligible", "eligible"]);
}

#[test]
fn missing_and_truncated_telemetry_never_becomes_zero_or_a_partial_total() {
    for status in ["missing", "truncated"] {
        let mut f = Fixture::new();
        let r = f.find("tiered", None);
        r["telemetry"] = json!({"status": status, "reason": "capture interrupted"});
        for name in ["costUsd", "inputTokens", "cacheWriteTokens"] {
            r["metrics"][name] = json!({"value": null, "unavailable": "capture interrupted"});
        }
        let policy = r["cachePolicy"].clone();
        let result = f.evaluate().unwrap();
        let g = group(&result, "tiered", &policy);
        assert_eq!(
            g["metrics"]["costUsd"],
            json!({"value": null, "unavailable": "1 run(s) lack costUsd"})
        );
        assert_eq!(g["metrics"]["cacheWriteTokens"]["value"], Value::Null);
        assert_eq!(g["metrics"]["wallSeconds"]["value"], 600.0);
        assert_eq!(verdicts(&result), ["eligible", "inconclusive"]);
    }
}

/// An overflowing total is null, as JavaScript writes Infinity, and never a
/// comparable cost.
#[test]
fn overflowing_totals_are_null_not_infinite() {
    let mut f = Fixture::new();
    for r in f.runs.iter_mut().filter(|r| r["arm"] == "tiered") {
        r["metrics"]["costUsd"]["value"] = json!(f64::MAX);
    }
    let result = f.evaluate().unwrap();
    assert_eq!(
        result["groups"][2]["metrics"]["costUsd"],
        json!({"value": null})
    );
    assert_eq!(verdicts(&result), ["eligible", "inconclusive"]);
    let runs = Value::Array(f.runs.clone());
    let printed = pretty_json(&summarize(&f.manifest, &runs).unwrap()).unwrap();
    assert!(printed.contains("\"costUsd\": {\n          \"value\": null\n        }"));
}

#[test]
fn three_task_matrix_and_exposed_stage_timings_are_supported() {
    let mut f = Fixture::new();
    let mut task = f.manifest["tasks"][0].clone();
    task["id"] = json!("fix-c");
    task["taskText"] = json!("Synthetic third task");
    f.manifest["tasks"].as_array_mut().unwrap().push(task);
    let extra: Vec<_> = f
        .runs
        .iter()
        .filter(|r| r["taskId"] == "fix-a")
        .map(|r| {
            let mut r = r.clone();
            r["id"] = json!(format!("{}-third", r["id"].as_str().unwrap()));
            r["taskId"] = json!("fix-c");
            r["taskText"] = json!("Synthetic third task");
            r["stageSeconds"] = json!({"explore": {"value": 10}, "execute": {"value": 90}});
            r
        })
        .collect();
    f.runs.extend(extra);
    f.redigest();
    assert_eq!(f.runs.len(), 54);
    let result = f.evaluate().unwrap();
    assert_eq!(result["status"], "complete");
    assert_eq!(result["groups"][0]["runs"], 9);
    assert_eq!(verdicts(&result), ["eligible", "eligible"]);
    f.manifest["tasks"].as_array_mut().unwrap().pop();
    f.runs.retain(|r| r["taskId"] != "fix-c");
    let error = f.evaluate().unwrap_err();
    assert!(error.to_string().contains("mixed comparison manifests"));
}

#[test]
fn failures_count_against_quality_and_retain_spending() {
    for outcome in ["failed", "timeout", "harness-error"] {
        let mut f = Fixture::new();
        f.runs[0]["outcome"] = json!(outcome);
        f.runs[0]["failures"] = json!(["independent check not passed"]);
        let result = f.evaluate().unwrap();
        assert_eq!(result["groups"][0]["passed"], 5);
        assert_eq!(result["groups"][0]["metrics"]["costUsd"]["value"], 6.0);
        assert_eq!(
            result["groups"][0]["failures"][0],
            json!({"id": f.runs[0]["id"], "outcome": outcome, "failures": ["independent check not passed"]})
        );
        assert_eq!(verdicts(&result), ["reject", "reject"]);
    }
}

#[test]
fn cost_and_time_thresholds_apply_to_both_strata_including_the_boundary() {
    let mut f = Fixture::new();
    for r in f.runs.iter_mut().filter(|r| r["arm"] == "tiered") {
        r["metrics"]["costUsd"]["value"] = json!(0.8);
        r["metrics"]["wallSeconds"]["value"] = json!(110);
    }
    assert_eq!(verdicts(&f.evaluate().unwrap())[1], "eligible");
    f.find("tiered", Some("warm"))["metrics"]["wallSeconds"]["value"] = json!(111);
    assert_eq!(verdicts(&f.evaluate().unwrap())[1], "reject");
    let mut expensive = Fixture::new();
    expensive.find("tiered", None)["metrics"]["costUsd"]["value"] = json!(4);
    assert_eq!(verdicts(&expensive.evaluate().unwrap())[1], "reject");
}

#[test]
fn decimal_boundaries_tolerate_only_floating_point_roundoff() {
    let mut f = Fixture::new();
    for r in &mut f.runs {
        let strong = r["arm"] == "single-strong";
        r["metrics"]["costUsd"]["value"] = json!(if strong { 0.05 } else { 0.04 });
        r["metrics"]["wallSeconds"]["value"] = json!(if strong { 0.1 } else { 0.11 });
    }
    assert_eq!(verdicts(&f.evaluate().unwrap()), ["eligible", "eligible"]);
    f.find("tiered", None)["metrics"]["costUsd"]["value"] = json!(0.04 + 1e-10);
    assert_eq!(verdicts(&f.evaluate().unwrap())[1], "reject");
}

#[test]
fn incomplete_matrices_and_zero_denominators_cannot_imply_adoption() {
    let mut f = Fixture::new();
    f.runs.pop();
    assert_eq!(
        verdicts(&f.evaluate().unwrap()),
        ["inconclusive", "inconclusive"]
    );
    let empty = serde_json::to_value(summarize(&f.manifest, &json!([])).unwrap()).unwrap();
    assert_eq!(empty["status"], "incomplete");
    assert_eq!(
        empty["groups"][0]["metrics"]["costUsd"],
        json!({"value": null, "unavailable": "No runs recorded"})
    );
    let mut zero = Fixture::new();
    for r in &mut zero.runs {
        r["metrics"]["costUsd"]["value"] = json!(0);
    }
    assert_eq!(
        verdicts(&zero.evaluate().unwrap()),
        ["inconclusive", "inconclusive"]
    );
}

#[test]
fn sample_limits_and_policy_changes_require_new_records() {
    for edit in [
        |m: &mut Value| m["limits"]["usd"] = json!(0.1),
        |m: &mut Value| m["tasks"].as_array_mut().unwrap().reverse(),
        |m: &mut Value| m["preflight"]["evidence"] = json!("fixture://new-preflight"),
        |m: &mut Value| m["configuration"]["costBasis"]["sha256"] = json!("9".repeat(64)),
    ] {
        let mut f = Fixture::new();
        edit(&mut f.manifest);
        let error = f.evaluate().unwrap_err();
        assert!(error.to_string().contains("mixed comparison manifests"));
    }
}

#[test]
fn unavailable_cost_basis_stays_inconclusive_and_mixed_basis_is_rejected() {
    let mut f = Fixture::new();
    let basis = json!({"unavailable": "no comparable prices"});
    f.manifest["configuration"]["costBasis"] = basis.clone();
    f.redigest();
    for r in &mut f.runs {
        r["costBasis"] = basis.clone();
        r["metrics"]["costUsd"] = json!({"value": null, "unavailable": "no comparable prices"});
    }
    assert_eq!(
        verdicts(&f.evaluate().unwrap()),
        ["inconclusive", "inconclusive"]
    );
    f.runs[0]["metrics"]["costUsd"] = json!({"value": 0});
    assert!(
        f.evaluate()
            .unwrap_err()
            .to_string()
            .contains("unavailable cost basis")
    );
    let mut mixed = Fixture::new();
    mixed.runs[0]["costBasis"]["sha256"] = json!("9".repeat(64));
    assert!(
        mixed
            .evaluate()
            .unwrap_err()
            .to_string()
            .contains("mixed cost basis")
    );
    // Any truthy marker beside a pinned basis still forbids recorded costs.
    for marker in [json!(" "), json!(true), json!(1)] {
        let mut f = Fixture::new();
        f.manifest["configuration"]["costBasis"]["unavailable"] = marker.clone();
        f.redigest();
        let basis = f.manifest["configuration"]["costBasis"].clone();
        for r in &mut f.runs {
            r["costBasis"] = basis.clone();
        }
        let error = f.evaluate().unwrap_err().to_string();
        assert!(
            error.contains("unavailable cost basis"),
            "{marker}: {error}"
        );
    }
}

#[test]
fn mixed_task_or_config_revisions_are_rejected_rather_than_pooled() {
    for field in [
        "startCommit",
        "taskText",
        "checksSha256",
        "configRevision",
        "harnessRevision",
        "environment",
        "renderSha256",
        "resolvedModels",
        "costBasis",
    ] {
        let mut f = Fixture::new();
        f.runs[0][field] = json!("different");
        let error = f.evaluate().unwrap_err();
        assert!(error.to_string().contains("mixed"), "{field}: {error}");
    }
}

#[test]
fn invalid_records_unavailable_metrics_and_duplicate_cells_are_rejected() {
    let edits: [fn(&mut Fixture); 17] = [
        |f| f.runs.push(f.runs[0].clone()),
        |f| f.runs[0]["metrics"]["costUsd"] = json!({"value": null}),
        |f| {
            f.runs[0]["metrics"]
                .as_object_mut()
                .unwrap()
                .remove("cacheReadTokens");
        },
        |f| f.runs[0]["metrics"]["wallSeconds"]["value"] = json!(-1),
        |f| f.runs[0]["metrics"]["costUsd"]["value"] = json!("1"),
        |f| f.runs[0]["stageSeconds"] = json!({}),
        |f| f.runs[0]["telemetry"] = json!({"status": "truncated"}),
        |f| f.runs[0]["outcome"] = json!("failed"),
        |f| f.runs[0]["artifactUrl"] = json!(""),
        |f| f.runs[0]["repetition"] = json!(4),
        |f| f.runs[0]["repetition"] = json!(1.5),
        |f| f.runs[0]["arm"] = json!("unknown"),
        |f| f.runs = vec![f.runs[0].clone(); 55],
        |f| f.runs[0]["telemetry"] = json!({}),
        |f| f.runs[0]["stageSeconds"] = json!({"explore": {"value": -1}}),
        |f| {
            f.runs[0]["outcome"] = json!("failed");
            f.runs[0]["failures"] = json!([" "]);
        },
        |f| f.runs[1]["id"] = f.runs[0]["id"].clone(),
    ];
    for (i, edit) in edits.iter().enumerate() {
        let mut f = Fixture::new();
        edit(&mut f);
        assert!(f.evaluate().is_err(), "edit {i}");
    }
    // A repeated cell is a duplicate even when its repetition is spelled 1.0.
    let mut f = Fixture::new();
    let mut copy = f.runs[0].clone();
    copy["id"] = json!("copy");
    copy["repetition"] = serde_json::from_str("1.0").unwrap();
    f.runs[1] = copy;
    assert!(
        f.evaluate()
            .unwrap_err()
            .to_string()
            .contains("duplicate run or matrix cell")
    );
}

#[test]
fn freezing_requires_evidence_bounded_tasks_and_immutable_dependencies() {
    let edits: [fn(&mut Value); 12] = [
        |m| m["preflight"] = Value::Null,
        |m| m["tasks"][1]["id"] = m["tasks"][0]["id"].clone(),
        |m| m["configuration"]["costBasis"]["unavailable"] = json!("no prices"),
        |m| {
            m["tasks"].as_array_mut().unwrap().pop();
        },
        |m| {
            let tasks = m["tasks"].as_array().unwrap().clone();
            m["tasks"].as_array_mut().unwrap().extend(tasks);
        },
        |m| m["tasks"][0]["startCommit"] = json!("main"),
        |m| m["tasks"][0]["checks"]["sha256"] = json!("latest"),
        |m| m["configuration"]["arms"]["tiered"]["sha256"] = json!(""),
        |m| m["configuration"]["harnessRevision"] = json!("main"),
        |m| m["repetitions"] = json!(100),
        |m| m["limits"]["usd"] = json!(6),
        |m| m["arms"] = json!(["tiered", "single-strong", "single-cheap"]),
    ];
    for (i, edit) in edits.iter().enumerate() {
        let mut manifest = Fixture::new().manifest;
        edit(&mut manifest);
        assert!(validate_manifest(&manifest).is_err(), "edit {i}");
    }
    // A pending manifest claims nothing frozen.
    let frozen = Fixture::new().manifest;
    for key in ["tasks", "preflight", "configuration"] {
        let mut manifest: Value = serde_json::from_str(MANIFEST).unwrap();
        manifest[key] = frozen[key].clone();
        let error = validate_manifest(&manifest).unwrap_err().to_string();
        assert!(error.contains("pending sample"), "{key}: {error}");
    }
}

#[test]
fn complete_summary_prints_in_javascript_field_order() {
    let f = Fixture::new();
    let runs = Value::Array(f.runs.clone());
    let printed = pretty_json(&summarize(&f.manifest, &runs).unwrap()).unwrap();
    let position = |needle: &str| printed.find(needle).unwrap();
    let order = [
        "\"status\": \"complete\"",
        "\"manifestSha256\": \"",
        "\"groups\": [",
        "\"cachePolicy\": \"cold\"",
        "\"arm\": \"single-strong\"",
        "\"runs\": 6",
        "\"expected\": 6",
        "\"passed\": 6",
        "\"failures\": []",
        "\"inputTokens\": {",
        "\"wallSeconds\": {",
        "\"decisions\": [",
        "\"verdict\": \"eligible\"",
    ];
    for pair in order.windows(2) {
        assert!(position(pair[0]) < position(pair[1]), "{pair:?}");
    }
    // Totals print as JavaScript spells them: 6, not 6.0.
    assert!(printed.contains("\"costUsd\": {\n          \"value\": 6\n        }"));
    assert!(!printed.contains("unavailable"));
}
