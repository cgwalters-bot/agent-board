//! Generic example deployment; never installed or used for live inference.
use std::process::Command;

use serde_json::{Value, json};

#[test]
fn resolve_table() {
    let example: Value =
        serde_json::from_str(include_str!("../../../fixtures/pools-example.json")).unwrap();
    let normal = "agent=opencode\nmodel=example/standard\nbudget=500\nskilled=true\n";
    let inherited = "agent=\nmodel=\nbudget=\nskilled=true\n";
    let heavy = "agent=opencode\nmodel=example/heavy\nbudget=500\nskilled=true\n";
    let inherited_heavy = "agent=\nmodel=example/heavy\nbudget=\nskilled=true\n";
    // Every case runs the real CLI and checks both streams, including zero
    // stdout on failure. Patch fields belong to this example pools file only.
    let mut cases = vec![
        (
            "explicit",
            vec!["--pool", "standard"],
            json!({}),
            Ok(normal),
        ),
        ("inherited", vec![], json!({}), Ok(inherited)),
        (
            "unskilled inherited",
            vec!["--fallback-agent", "vibe"],
            json!({}),
            Ok("agent=\nmodel=\nbudget=\nskilled=false\n"),
        ),
        (
            "unskilled explicit",
            vec!["--pool", "compact"],
            json!({}),
            Ok("agent=vibe\nmodel=example/compact\nbudget=100\nskilled=false\n"),
        ),
        (
            "heavy explicit",
            vec![
                "--pool",
                "standard",
                "--effort",
                "heavy",
                "--fallback-agent",
                "ignored",
            ],
            json!({}),
            Ok(heavy),
        ),
        (
            "heavy inherited",
            vec!["--effort", "heavy", "--fallback-model", "example/standard"],
            json!({}),
            Ok(inherited_heavy),
        ),
        (
            "missing inherited model",
            vec!["--effort", "heavy"],
            json!({}),
            Err("unique fallback"),
        ),
        (
            "unmatched inherited model",
            vec!["--effort", "heavy", "--fallback-model", "unknown"],
            json!({}),
            Err("unique fallback"),
        ),
        (
            "unmatched inherited agent",
            vec![
                "--effort",
                "heavy",
                "--fallback-model",
                "example/standard",
                "--fallback-agent",
                "claude",
            ],
            json!({}),
            Err("unique fallback"),
        ),
        (
            "review heavy",
            vec![
                "--pool", "standard", "--effort", "heavy", "--kind", "review",
            ],
            json!({}),
            Err("not supported for reviews"),
        ),
        (
            "review ordinary",
            vec!["--pool", "standard", "--kind", "review"],
            json!({}),
            Ok(normal),
        ),
        (
            "unknown pool",
            vec!["--pool", "unknown"],
            json!({}),
            Err("unknown pool"),
        ),
        (
            "missing heavy",
            vec!["--pool", "compact", "--effort", "heavy"],
            json!({}),
            Err("heavy.model"),
        ),
    ];
    let mut ambiguous = example.clone();
    ambiguous["duplicate"] = example["standard"].clone();
    cases.push((
        "ambiguous inherited",
        vec!["--effort", "heavy", "--fallback-model", "example/standard"],
        ambiguous,
        Err("unique fallback"),
    ));
    for (name, args, expected) in [
        (
            "different explicit budget",
            vec!["--pool", "standard", "--effort", "heavy"],
            "agent=claude\nmodel=example/heavy\nbudget=3000\nskilled=true\n",
        ),
        (
            "inherited budget stays empty",
            vec![
                "--effort",
                "heavy",
                "--fallback-agent",
                "claude",
                "--fallback-model",
                "example/standard",
            ],
            inherited_heavy,
        ),
    ] {
        cases.push((
            name,
            args,
            json!({"standard": {"agent": "claude", "budget": "3000"}}),
            Ok(expected),
        ));
    }
    for effort in [
        "Heavy",
        " heavy",
        "heavy\n",
        "low",
        "default",
        "example/heavy",
    ] {
        cases.push((
            "unknown effort",
            vec!["--pool", "standard", "--effort", effort],
            json!({}),
            Err("effort must"),
        ));
    }
    for variant in [
        Value::Null,
        json!([]),
        json!("model"),
        json!({}),
        json!({"model": ""}),
        json!({"model": null}),
        json!({"model": 42}),
        json!({"model": "bad model"}),
        json!({"model": " x"}),
        json!({"model": "x\n"}),
        json!({"model": "x\r"}),
        json!({"model": "x\nbudget=9999"}),
        json!({"model": "x".repeat(201)}),
    ] {
        let patch = json!({"standard": {"heavy": variant}});
        cases.push((
            "invalid explicit heavy",
            vec!["--pool", "standard", "--effort", "heavy"],
            patch.clone(),
            Err("heavy.model"),
        ));
        cases.push((
            "invalid inherited heavy",
            vec!["--effort", "heavy", "--fallback-model", "example/standard"],
            patch.clone(),
            Err("heavy.model"),
        ));
        cases.push((
            "unused invalid heavy",
            vec!["--pool", "standard"],
            patch,
            Ok(normal),
        ));
    }
    for (field, bad) in [
        ("model", json!(null)),
        ("model", json!(42)),
        ("model", json!("")),
        ("model", json!("bad model")),
        ("model", json!("model\n")),
        ("model", json!("-model")),
        ("model", json!("é")),
        ("model", json!("x".repeat(201))),
        ("budget", json!(null)),
        ("budget", json!(500)),
        ("budget", json!("500\n")),
        ("budget", json!("0")),
        ("budget", json!("01")),
        ("budget", json!("-1")),
        ("budget", json!("1.5")),
        ("budget", json!("")),
    ] {
        cases.push((
            "invalid unselected pool",
            vec![],
            json!({"compact": {field: bad}}),
            Err(field),
        ));
    }
    for agent in ["claude", "fake"] {
        cases.push((
            "skilled fallback",
            vec!["--fallback-agent", agent],
            json!({}),
            Ok(inherited),
        ));
    }
    let dir = std::env::temp_dir().join(format!("agent-board-pools-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, args, patch, expected) in cases {
        let mut pools = example.clone();
        for (pool, fields) in patch.as_object().unwrap() {
            for (key, value) in fields.as_object().unwrap() {
                pools[pool][key] = value.clone();
            }
        }
        let path = dir.join("pools-example.json");
        std::fs::write(&path, serde_json::to_vec(&pools).unwrap()).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-board"));
        command.args(["pools", "resolve", "--pools"]).arg(&path);
        if !args.contains(&"--kind") {
            command.args(["--kind", "implement"]);
        }
        let output = command.args(args).output().unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        match expected {
            Ok(text) => {
                assert!(output.status.success(), "{name}: {stderr}");
                assert_eq!(stdout, text, "{name}");
                assert!(stderr.is_empty(), "{name}: {stderr}");
            }
            Err(message) => {
                assert!(!output.status.success(), "{name}");
                assert!(stdout.is_empty(), "{name}: {stdout}");
                assert!(stderr.contains(message), "{name}: {stderr}");
            }
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}
