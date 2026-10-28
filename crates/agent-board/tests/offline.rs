use assert_cmd::Command;
use serde_json::Value;

fn command() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("agent-board"))
}

fn fixture() -> String {
    format!("{}/../../fixtures/board.json", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn setup_credential_free_rehearsal() {
    let setup = include_str!("../../../docs/setup.md");
    let section = setup
        .split_once("## Try it offline\n")
        .unwrap()
        .1
        .split_once("## Connect one lane\n")
        .unwrap()
        .0;
    let commands = section.split_once("```sh\n").unwrap().1;
    let commands = commands.split_once("```").unwrap().0;
    let mut count = 0;
    for line in commands.lines() {
        let mut args = line.split_whitespace();
        assert_eq!(args.next(), Some("agent-board"));
        let output = command()
            .args(args)
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .assert()
            .success()
            .get_output()
            .clone();
        assert!(!output.stdout.is_empty(), "{line}");
        count += 1;
    }
    assert_eq!(count, 3);
}

#[test]
fn decisions_cli_configuration_table() {
    let dir = std::env::temp_dir().join(format!("agent-board-decisions-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("agent-board.json");
    for (contents, message, code) in [
        (r#"{}"#, "decisions are not configured", 1),
        (
            r#"{"decision":{"repository":"sample/coordination","issue":3}}"#,
            "unknown agent-board config key decision",
            1,
        ),
        (
            r#"{"decisions":{"repository":"sample/coordination","issue":3,"extra":true}}"#,
            "unknown field",
            1,
        ),
        (
            r#"{"decisions":{"repository":"bad","issue":3}}"#,
            "repository",
            1,
        ),
        (
            r#"{"decisions":{"repository":"sample/coordination","issue":0}}"#,
            "positive GraphQL Int",
            1,
        ),
        (
            r#"{"decisions":{"repository":"sample/coordination","issue":3}}"#,
            "GH_TOKEN",
            4,
        ),
    ] {
        std::fs::write(&config, contents).unwrap();
        for args in [
            vec!["ask", "read"],
            vec!["ask", "resolve", "D12", "Keep the text."],
            vec![
                "ask",
                "add",
                "Proceed?",
                "--context",
                "Trial passed.",
                "--option",
                "Yes",
                "--option",
                "No",
                "--recommend",
                "ready",
                "--item",
                "sample/work#7",
                "--priority",
                "P0",
            ],
        ] {
            let output = command()
                .args(args)
                .arg("--config")
                .arg(&config)
                .env_remove("GH_TOKEN")
                .env_remove("GITHUB_TOKEN")
                .assert()
                .code(code)
                .get_output()
                .clone();
            assert!(
                String::from_utf8(output.stderr).unwrap().contains(message),
                "{contents}"
            );
            assert!(output.stdout.is_empty());
        }
        assert!(!config.with_extension("decisions-state.md").exists());
    }
    std::fs::remove_dir_all(dir).unwrap();
    let output = command()
        .args([
            "ask",
            "resolve",
            "Demo/repo#12",
            "answer",
            "--snapshot",
            &fixture(),
        ])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(
        !error.contains("decisions use an issue transport"),
        "{error}"
    );
    assert!(
        error.contains("not on the board in this snapshot"),
        "{error}"
    );
}

#[test]
fn reconcile_config_is_opt_in() {
    let dir = std::env::temp_dir().join(format!(
        "agent-board-reconcile-config-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("agent-board.json");
    let baseline = command()
        .args(["reconcile", "plan", "--snapshot", &fixture(), "--emit"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&baseline).unwrap();
    assert_eq!(value["type"], "update_project");
    assert_eq!(value["fields"]["Status"], "Triage");
    for contents in [r#"{}"#, r#"{"statusReconcile":{"enabled":false}}"#] {
        std::fs::write(&config, contents).unwrap();
        let output = command()
            .args([
                "reconcile",
                "plan",
                "--snapshot",
                &fixture(),
                "--emit",
                "--config",
                config.to_str().unwrap(),
            ])
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .assert()
            .success()
            .get_output()
            .clone();
        assert_eq!(output.stdout, baseline);
    }
    std::fs::write(&config, r#"{"statusReconcile":{"enabled":"yes"}}"#).unwrap();
    let output = command()
        .args([
            "reconcile",
            "plan",
            "--snapshot",
            &fixture(),
            "--config",
            config.to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("invalid statusReconcile enabled")
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn epic_apply_cli_is_opt_in_and_requires_trusted_config() {
    for args in [
        vec!["epic", "apply", "example/work#1"],
        vec![
            "epic",
            "apply",
            "example/work#1",
            "--config",
            "unused",
            "--apply",
            "--dry-run",
        ],
        vec![
            "epic",
            "plan",
            "example/work#1",
            "--operator",
            "operator",
            "--ready-cap",
            "1",
        ],
    ] {
        command().args(args).assert().code(1);
    }
    let help = command()
        .args(["epic", "apply", "--help"])
        .assert()
        .success()
        .get_output()
        .clone();
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(text.contains("the default"));
    assert!(text.contains("--apply"));
}

/// Refused before any request to GitHub.
#[test]
fn apps_create_cli_refusals() {
    let base = ["apps", "create", "--org", "example", "--role"];
    for (args, message) in [
        (vec!["nope"], "not in the roles config"),
        (
            vec!["status", "--config", "missing.json"],
            "read missing.json",
        ),
        (vec!["status", "--json", "x"], "only its own options"),
        (
            vec!["status", "--key-stdout", "--hostname", "ghe.example.com"],
            "supports only github.com",
        ),
        (vec!["status"], "--key-file FILE or --key-stdout"),
        (
            vec!["status", "--key-stored", "--key-stdout"],
            "cannot be used with",
        ),
        (
            vec!["status", "--key-file", "k", "--key-stdout"],
            "cannot be used with",
        ),
    ] {
        let output = command()
            .args(base)
            .args(&args)
            .assert()
            .code(1)
            .get_output()
            .clone();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(message), "{args:?}: {stderr}");
    }
    let output = command()
        .args(["apps", "create", "--org", "bad.org", "--role", "status"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(String::from_utf8_lossy(&output.stderr).contains("not an organization login"));
}

#[test]
fn live_project_cli_refusals() {
    for (args, reason) in [
        (vec!["project", "status-update"], "--status"),
        (
            vec!["project", "status-update", "--status", "unknown"],
            "invalid value",
        ),
        (
            vec![
                "project",
                "status-update",
                "--status",
                "on-track",
                "--snapshot",
                &fixture(),
            ],
            "requires a live project",
        ),
        (
            vec![
                "project",
                "status-update",
                "--status",
                "at-risk",
                "--json",
                "project",
            ],
            "does not support --json",
        ),
        (vec!["changes"], "--since"),
        (
            vec![
                "changes",
                "--since",
                "2026-10-20T00:00:00Z",
                "--format",
                "html",
            ],
            "invalid value",
        ),
        (
            vec![
                "changes",
                "--since",
                "2026-10-20T00:00:00Z",
                "--snapshot",
                &fixture(),
            ],
            "requires a live project",
        ),
        (
            vec!["changes", "--since", "2026-10-20T00:00:00Z", "--json", "x"],
            "--format json",
        ),
    ] {
        let output = command().args(args).assert().code(1).get_output().clone();
        assert!(String::from_utf8(output.stderr).unwrap().contains(reason));
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn proposal_check_cli_table() {
    let snapshot = format!(
        "{}/../../fixtures/assigned-board.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let valid = serde_json::json!({"type":"update_project", "project":"https://github.com/orgs/example/projects/1", "target_repo":"example/intake", "content_type":"issue", "content_number":1, "fields":{"Status":"Done"}});
    for (identity, number, field, reason) in [
        ("rust-worker", 1, "Status", None),
        ("rust-worker", 2, "Status", Some("not assigned")),
        ("rust-worker", 3, "Status", Some("not assigned")),
        ("rust-worker", 1, "Agent", Some("never editable")),
        ("rust-worker", 1, "agent", Some("never editable")),
        ("rust-worker", 1, "AGENT", Some("never editable")),
        ("rust-worker", 1, "aGeNt", Some("never editable")),
        ("rust-worker", 3, "Agent", Some("never editable")),
        ("rust-worker", 1, "Priority", Some("field not permitted")),
        ("unknown", 1, "Status", Some("unknown agent identity")),
    ] {
        let mut output = valid.clone();
        output["content_number"] = serde_json::json!(number);
        output["fields"] = serde_json::json!({field:"rust-worker"});
        let result = command()
            .args([
                "check",
                "proposals",
                "--as",
                identity,
                "--snapshot",
                &snapshot,
            ])
            .write_stdin(format!("{output}\n"))
            .assert();
        if let Some(reason) = reason {
            let output = result.code(1).get_output().clone();
            assert!(String::from_utf8(output.stderr).unwrap().contains(reason));
        } else {
            result.success().stdout("");
        }
    }
    for (input, reason) in [
        (format!("{valid}\nnot json\n"), "decode proposal line 2"),
        (
            format!("{valid}\n{{\"type\":\"create_issue\"}}\n"),
            "proposal line 2 refused",
        ),
    ] {
        let output = command()
            .args([
                "check",
                "proposals",
                "--as",
                "rust-worker",
                "--snapshot",
                &snapshot,
            ])
            .write_stdin(input)
            .assert()
            .code(1)
            .get_output()
            .clone();
        assert!(String::from_utf8(output.stderr).unwrap().contains(reason));
    }
    command()
        .args(["check", "proposals", "--as", "rust-worker"])
        .write_stdin("")
        .assert()
        .code(1);
    let output = command()
        .args(["snapshot", "create", "--snapshot", &snapshot])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let normalized: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(normalized["items"][0]["assignment"], "rust-worker");
    assert_eq!(normalized["items"][1]["assignment"], "board-coordinator");
    assert!(normalized["items"][2]["assignment"].is_null());
}

#[test]
fn custom_policy_snapshot_and_check_cli() {
    // Honor TMPDIR, allowing sandbox runners to keep scratch files under home.
    let directory = std::env::temp_dir().join(format!("agent-board-policy-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let policy = directory.join("agents.toml");
    let snapshot = directory.join("board.json");
    let proposals = directory.join("outputs.jsonl");
    let original = include_str!("../../../fixtures/assigned-board.json")
        .replace("rust-worker", "custom-worker");
    std::fs::write(&snapshot, original).unwrap();
    std::fs::write(
        &policy,
        board_core::DEFAULT_AGENT_POLICY.replace("rust-worker", "custom-worker"),
    )
    .unwrap();
    let args = [
        "--policy",
        policy.to_str().unwrap(),
        "--snapshot",
        snapshot.to_str().unwrap(),
    ];
    let output = command()
        .args(["snapshot", "create"])
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let normalized: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(normalized["items"][0]["assignment"], "custom-worker");
    std::fs::write(&snapshot, output).unwrap();
    std::fs::write(
        &proposals,
        "{\"type\":\"close_issue\",\"target_repo\":\"example/intake\",\"issue_number\":1}\n",
    )
    .unwrap();
    command()
        .args([
            "check",
            "proposals",
            "--as",
            "custom-worker",
            "--proposals",
            proposals.to_str().unwrap(),
        ])
        .args(args)
        .assert()
        .success();
    std::fs::write(
        &policy,
        board_core::DEFAULT_AGENT_POLICY
            .replace("rust-worker", "custom-worker")
            .replace("enabled = true", "enabled = false"),
    )
    .unwrap();
    let output = command()
        .args([
            "check",
            "proposals",
            "--as",
            "custom-worker",
            "--proposals",
            proposals.to_str().unwrap(),
        ])
        .args(args)
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("disabled agent identity")
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn dispatch_preview_cli_refuses_ineligible_items_and_profiles() {
    for extra in [vec![], vec!["--kind", "review"], vec!["--json", "argv"]] {
        command()
            .args([
                "request",
                "dispatch",
                "--item",
                "example/intake#1",
                "--caller",
                "example/runners",
                "--snapshot",
                &fixture(),
            ])
            .args(extra)
            .assert()
            .code(1);
    }
    command()
        .args(["request", "dispatch", "--help"])
        .assert()
        .success();
    command()
        .args(["run", "record", "--help"])
        .assert()
        .success();
}

#[test]
fn offline_table() {
    for (args, expected) in [
        (
            vec![
                "item",
                "view",
                "example/intake#1",
                "--json",
                "turn",
                "--jq",
                ".turn",
            ],
            "Coordinator\n",
        ),
        (
            vec![
                "item",
                "view",
                "https://github.com/example/intake/issues/1",
                "--json",
                "number",
                "--jq",
                ".number",
            ],
            "1\n",
        ),
        (
            vec![
                "item",
                "view",
                "1",
                "-R",
                "example/intake",
                "--json",
                "number",
                "--jq",
                ".number",
            ],
            "1\n",
        ),
        (
            vec![
                "item",
                "list",
                "--status",
                "Triage",
                "--json",
                "number",
                "--jq",
                ".[0].number",
            ],
            "2\n",
        ),
        (
            vec![
                "reconcile",
                "plan",
                "--json",
                "actions",
                "--jq",
                ".actions[].reason",
            ],
            "missing_status\n",
        ),
    ] {
        let output = command()
            .args(args)
            .args(["--snapshot", &fixture()])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        assert_eq!(String::from_utf8(output).unwrap(), expected);
    }
}

/// The snapshot board-forge's Forgejo reader makes of the issue listing and
/// comment fixtures (its unit test keeps the two in step).
#[test]
fn forgejo_snapshot_table() {
    let fixture = format!(
        "{}/../../fixtures/forgejo/board.json",
        env!("CARGO_MANIFEST_DIR")
    );
    for (args, expected) in [
        (
            vec!["item", "list"],
            "ITEM\tSTATUS\tPRIORITY\tTURN\tTITLE\n\
             sample/board#1\t-\tP1\tCoordinator\tRead the board from labels\n\
             sample/board#2\t-\t-\tOperator\tTwo priorities set by hand\n\
             sample/board#3\t-\t-\tWorker\tClosed without a priority\n",
        ),
        (
            vec![
                "item",
                "list",
                "--priority",
                "P1",
                "--json",
                "number",
                "--jq",
                ".[].number",
            ],
            "1\n",
        ),
        (
            vec![
                "item", "list", "--state", "closed", "--json", "url", "--jq", ".[].url",
            ],
            "https://forge.example/sample/board/issues/3\n",
        ),
        (
            vec![
                "item",
                "view",
                "sample/board#1",
                "--hostname",
                "forge.example",
                "--json",
                "fields",
                "--jq",
                ".fields.Run",
            ],
            "https://forge.example/sample/runner/actions/runs/41\n",
        ),
        (
            vec![
                "reconcile",
                "plan",
                "--json",
                "actions",
                "--jq",
                ".actions | length",
            ],
            "0\n",
        ),
        (
            vec![
                "reconcile",
                "plan",
                "--json",
                "diagnostics",
                "--jq",
                ".diagnostics | to_entries[] | \"\\(.key) \\(.value)\"",
            ],
            "sample/board#2 missing_data: incomplete fields: 2 priority/ labels\n\
             sample/board#3 missing_data: incomplete fields: no priority/ label\n",
        ),
    ] {
        let output = command()
            .args(&args)
            .args(["--snapshot", &fixture])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        assert_eq!(String::from_utf8(output).unwrap(), expected, "{args:?}");
    }
}

#[test]
fn board_config_cli_table() {
    let dir = std::env::temp_dir().join(format!("agent-board-board-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let forgejo = dir.join("forgejo.json");
    std::fs::write(
        &forgejo,
        r#"{"board":{"forge":"forgejo","url":"https://forge.example/","tokenEnv":"AGENT_BOARD_TEST_FORGEJO_TOKEN","repositories":["sample/board"],"membership":"open"}}"#,
    )
    .unwrap();
    let invalid = dir.join("invalid.json");
    std::fs::write(
        &invalid,
        r#"{"board":{"forge":"forgejo","url":"https://forge.example/?a=b","repositories":["sample/board"],"membership":"open"}}"#,
    )
    .unwrap();
    let github = dir.join("github.json");
    std::fs::write(&github, r#"{"board":{"forge":"github"}}"#).unwrap();
    let (forgejo, invalid, github) = (
        forgejo.to_str().unwrap(),
        invalid.to_str().unwrap(),
        github.to_str().unwrap(),
    );
    // Plain HTTP warns once on stderr, and only for a host outside the cluster.
    for (url, warnings) in [
        ("http://forge.example:3000/", 1),
        ("http://forgejo-http.forgejo.svc:3000/", 0),
    ] {
        let plain = dir.join("plain.json");
        std::fs::write(
            &plain,
            format!(
                r#"{{"board":{{"forge":"forgejo","url":"{url}","tokenEnv":"AGENT_BOARD_TEST_FORGEJO_TOKEN","repositories":["sample/board"],"membership":"open"}}}}"#
            ),
        )
        .unwrap();
        let output = command()
            .env_remove("AGENT_BOARD_TEST_FORGEJO_TOKEN")
            .args(["item", "list", "--board-config", plain.to_str().unwrap()])
            .assert()
            .code(4)
            .get_output()
            .clone();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(
            stderr.matches("token is sent unencrypted").count(),
            warnings,
            "{url}: {stderr}"
        );
        assert!(output.stdout.is_empty(), "{url}");
    }
    for (args, code, message) in [
        // No token in the configured variable: gh's authentication exit code.
        (
            vec!["snapshot", "create", "--board-config", forgejo],
            4,
            "AGENT_BOARD_TEST_FORGEJO_TOKEN",
        ),
        (
            vec!["reconcile", "plan", "--board-config", forgejo],
            4,
            "AGENT_BOARD_TEST_FORGEJO_TOKEN",
        ),
        (
            vec!["item", "list", "--board-config", forgejo, "--owner", "x"],
            1,
            "not --owner or --project",
        ),
        (
            vec!["item", "add", "sample/board#1", "--board-config", forgejo],
            1,
            "only read by",
        ),
        (
            vec!["item", "list", "--board-config", invalid],
            1,
            "canonical HTTP(S) URL",
        ),
        // GitHub stays the default: the board config adds nothing to it.
        (
            vec![
                "item",
                "list",
                "--board-config",
                github,
                "--owner",
                "example",
                "--project",
                "1",
            ],
            4,
            "GH_TOKEN",
        ),
    ] {
        let output = command()
            .env_remove("AGENT_BOARD_TEST_FORGEJO_TOKEN")
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .args(&args)
            .assert()
            .code(code)
            .get_output()
            .stderr
            .clone();
        let stderr = String::from_utf8(output).unwrap();
        assert!(stderr.contains(message), "{args:?}: {stderr}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn project_init_plan_and_dispatch_hint_table() {
    // Honor TMPDIR, allowing sandbox runners to keep scratch files under home.
    let directory = std::env::temp_dir().join(format!("agent-board-init-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let snapshot = directory.join("board.json");
    let write = |fields: &Value, complete: bool| {
        let mut board: Value =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        board["fields"] = fields.clone();
        board["items"][0]["status"] = "Todo".into();
        board["coverage"]["complete"] = complete.into();
        std::fs::write(&snapshot, board.to_string()).unwrap();
    };
    let default_board = serde_json::json!([{"name":"Status","dataType":"SINGLE_SELECT","options":[{"name":"Todo"},{"name":"In Progress"},{"name":"Done"}]}]);
    let mut text_turn = default_board.clone();
    text_turn
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"name":"Turn","dataType":"TEXT"}));
    let mut ready = default_board.clone();
    ready.as_array_mut().unwrap().extend([
        serde_json::json!({"name":"Turn","dataType":"SINGLE_SELECT","options":[{"name":"Coordinator"},{"name":"Worker"},{"name":"Operator"}]}),
        serde_json::json!({"name":"Run","dataType":"TEXT"}),
        serde_json::json!({"name":"Result","dataType":"TEXT"}),
    ]);
    for (fields, complete, code, stdout, stderr) in [
        (
            &default_board,
            true,
            0,
            "Status\tSINGLE_SELECT\tpresent\t\nTurn\tSINGLE_SELECT\tmissing\tCoordinator, Worker, Operator, External, None\nRun\tTEXT\tmissing\t\nResult\tTEXT\tmissing\t\n",
            "gh project field-create 1 --owner example --name Turn --data-type SINGLE_SELECT --single-select-options Coordinator,Worker,Operator,External,None\ngh project field-create 1 --owner example --name Run --data-type TEXT\ngh project field-create 1 --owner example --name Result --data-type TEXT",
        ),
        (&default_board, false, 1, "", "incomplete snapshot"),
        (
            &ready,
            true,
            0,
            "Status\tSINGLE_SELECT\tpresent\t\nTurn\tSINGLE_SELECT\tpresent\t\nRun\tTEXT\tpresent\t\nResult\tTEXT\tpresent\t\n",
            "",
        ),
        (
            &text_turn,
            true,
            1,
            "",
            "exactly one Turn field of type SINGLE_SELECT",
        ),
        (
            &serde_json::json!([{"name":"Status","dataType":"SINGLE_SELECT","options":[{"name":"Triage"}]}]),
            true,
            1,
            "",
            "Status option 'Todo'",
        ),
        (
            &serde_json::json!([]),
            true,
            1,
            "",
            "restore the built-in Status",
        ),
    ] {
        write(fields, complete);
        let output = command()
            .args(["project", "init", "--snapshot"])
            .arg(&snapshot)
            .assert()
            .code(code)
            .get_output()
            .clone();
        assert_eq!(String::from_utf8(output.stdout).unwrap(), stdout);
        let diagnostic = String::from_utf8(output.stderr).unwrap();
        assert!(diagnostic.contains(stderr));
        if stderr.is_empty() || code != 0 {
            assert!(!diagnostic.contains("gh project field-create"));
        }
    }
    // A default board lacks only fields init can create; dispatch names the command.
    write(&default_board, true);
    let output = command()
        .args([
            "request",
            "dispatch",
            "--item",
            "example/intake#1",
            "--caller",
            "example/runners",
            "--snapshot",
        ])
        .arg(&snapshot)
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(String::from_utf8(output.stderr).unwrap().contains(
        "board lacks the Turn, Run, Result field(s); create them with: agent-board project init --owner example --project 1"
    ));
    // A snapshot can come from elsewhere: a hostile owner never reaches the printed command.
    for owner in ["$(id)", "a;id", "x&&id"] {
        let mut board: Value = serde_json::from_slice(&std::fs::read(&snapshot).unwrap()).unwrap();
        board["project"] = format!("https://github.com/orgs/{owner}/projects/1").into();
        let hostile = directory.join("hostile.json");
        std::fs::write(&hostile, board.to_string()).unwrap();
        let output = command()
            .args([
                "request",
                "dispatch",
                "--item",
                "example/intake#1",
                "--caller",
                "example/runners",
                "--snapshot",
            ])
            .arg(&hostile)
            .assert()
            .code(1)
            .get_output()
            .clone();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("invalid GitHub project URL"), "{stderr}");
        assert!(!stderr.contains("project init"), "{stderr}");
    }
    std::fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn project_init_live_refusals_precede_token() {
    // Each is refused before a token is read: no token is set, yet none is asked for.
    for (args, stderr) in [
        (
            vec!["--owner", "example", "--project", "1", "--json", "fields"],
            "project init prints a field table",
        ),
        (
            vec![
                "--owner",
                "example",
                "--project",
                "1",
                "--hostname",
                "github.example",
            ],
            "unsupported GitHub host",
        ),
        (vec!["--project", "1"], "live commands require --owner"),
        (
            vec!["--owner", "example"],
            "live commands require --project",
        ),
    ] {
        let output = command()
            .args(["project", "init"])
            .args(&args)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .assert()
            .failure()
            .get_output()
            .clone();
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains(stderr), "{args:?}: {error}");
        assert!(
            !error.contains("authentication required"),
            "{args:?}: {error}"
        );
    }
}

#[test]
fn snapshot_and_project_views() {
    for args in [["snapshot", "create"], ["project", "view"]] {
        let output = command()
            .args(args)
            .args(["--snapshot", &fixture()])
            .args(["--json", "schema,project,clock,coverage,items,fields"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let snapshot: board_core::Snapshot = serde_json::from_slice(&output).unwrap();
        snapshot.validate().unwrap();
        assert_eq!(snapshot.items.len(), 2);
    }
}

#[test]
fn first_run_help_and_summary() {
    for (args, present, absent) in [
        (
            vec!["--help"],
            "Preview deterministic board repairs",
            "  output ",
        ),
        (vec!["--help"], "Ask the operator a question", "  output "),
        (
            vec!["item", "--help"],
            "including closed issues",
            "  create ",
        ),
        (
            vec!["item", "list", "--help"],
            "[default: all]",
            "--template",
        ),
        (vec!["item", "view", "--help"], "OWNER/REPO#N", "--template"),
        (
            vec!["snapshot", "create", "--help"],
            "JSON to stdout",
            "--template",
        ),
        // run record --apply writes and epic apply --apply exists; help
        // must not claim otherwise.
        (
            vec!["--help"],
            "or write them with --apply",
            "proposals only",
        ),
        (
            vec!["epic", "plan", "--help"],
            "use epic apply --apply to make them",
            "apply is not implemented",
        ),
    ] {
        let output = command()
            .args(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains(present), "{text}");
        assert!(!text.contains(absent), "{text}");
    }
    let output = command()
        .args(["project", "view", "--snapshot", &fixture()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    for expected in [
        "Project: https://github.com/orgs/example/projects/1",
        "Items: 2",
        "Triage\t1",
        "Turn\tmissing",
    ] {
        assert!(text.contains(expected), "{text}");
    }
    assert!(!text.contains("board-snapshot/v1"));
    let output = command()
        .args(["item", "create"])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("gh issue create")
    );
}

#[test]
fn item_add_and_edit_preview_table() {
    let proposal = |number: u64, fields: &str| {
        format!(
            "{{\"content_number\":{number},\"content_type\":\"issue\",\"fields\":{fields},\"project\":\"https://github.com/orgs/example/projects/1\",\"target_repo\":\"example/intake\",\"type\":\"update_project\"}}\n"
        )
    };
    let mut board: Value =
        serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
    board["fields"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"name":"Priority","dataType":"SINGLE_SELECT","options":[{"name":"P0"},{"name":"P1"}]}));
    board["items"][1]["content_kind"] = "pull_request".into();
    // A snapshot only previews: nothing here has a token, and none is asked for.
    for (args, code, stdout, stderr) in [
        (
            vec![
                "item",
                "add",
                "https://github.com/example/intake/issues/9",
                "--priority",
                "P1",
            ],
            0,
            proposal(9, r#"{"Priority":"P1"}"#),
            "not applied",
        ),
        (
            vec!["item", "add", "example/intake#9"],
            0,
            proposal(9, "{}"),
            "not applied",
        ),
        (
            vec![
                "item",
                "edit",
                "1",
                "-R",
                "example/intake",
                "--field",
                "Status=Done",
                "--field",
                "Priority=P0",
            ],
            0,
            proposal(1, r#"{"Priority":"P0","Status":"Done"}"#),
            "not applied",
        ),
        (
            vec!["item", "add", "example/intake#9", "--priority", "P9"],
            1,
            String::new(),
            "Priority has no unambiguous option \"P9\"; options: P0, P1",
        ),
        (
            vec!["item", "edit", "example/intake#9", "--field", "Status=Done"],
            1,
            String::new(),
            "not on the board",
        ),
        (
            vec!["item", "edit", "example/intake#2", "--field", "Status=Done"],
            1,
            String::new(),
            "only issues can be added or edited, not pull_request",
        ),
        (
            vec!["item", "add", "example/intake#2"],
            1,
            String::new(),
            "only issues can be added or edited, not pull_request",
        ),
        (
            vec!["item", "add", "https://github.com/example/intake/pull/9"],
            1,
            String::new(),
            "only issues can be added or edited, not pull requests",
        ),
        (
            vec!["item", "add", "https://github.com//example/intake/pull/9"],
            1,
            String::new(),
            "only issues can be added or edited, not pull requests",
        ),
        (
            vec![
                "item",
                "add",
                "https://github.com/example/intake/issues/../pull/9",
            ],
            1,
            String::new(),
            "only issues can be added or edited, not pull requests",
        ),
        (
            vec![
                "item",
                "edit",
                "https://github.com//example/intake/./pull/1",
                "--field",
                "Status=Done",
            ],
            1,
            String::new(),
            "only issues can be added or edited, not pull requests",
        ),
        (
            vec![
                "item",
                "add",
                "9",
                "--repo",
                "example/intake",
                "--hostname",
                "github.example",
            ],
            1,
            String::new(),
            "unsupported item host github.example",
        ),
        (
            vec!["item", "edit", "example/intake#1", "--field", "Turn=Worker"],
            1,
            String::new(),
            "board has no Turn field",
        ),
        (
            vec!["item", "edit", "example/intake#1"],
            1,
            String::new(),
            "--field <NAME=VALUE>",
        ),
        (
            vec![
                "item",
                "edit",
                "example/intake#1",
                "--field",
                "Status=Done",
                "--apply",
            ],
            1,
            String::new(),
            "cannot use --snapshot",
        ),
        (
            vec!["item", "add", "example/intake#9", "--json", "fields"],
            1,
            String::new(),
            "--json is unsupported",
        ),
    ] {
        let output = command()
            .args(&args)
            .args(["--snapshot", "/dev/stdin"])
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .write_stdin(board.to_string())
            .assert()
            .code(code)
            .get_output()
            .clone();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            stdout,
            "{args:?}"
        );
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains(stderr), "{args:?}: {error}");
    }
    // Live writes are refused before a token is read, and a token is then required.
    for (args, code, stderr) in [
        (
            vec![
                "--owner",
                "example",
                "--project",
                "1",
                "--hostname",
                "github.example",
            ],
            1,
            "unsupported item host",
        ),
        (vec!["--project", "1"], 1, "live commands require --owner"),
        (
            vec!["--owner", "example", "--project", "1"],
            4,
            "authentication required",
        ),
    ] {
        let output = command()
            .args(["item", "add", "example/intake#9", "--apply"])
            .args(&args)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .assert()
            .code(code)
            .get_output()
            .clone();
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains(stderr), "{args:?}: {error}");
    }
}

#[test]
fn list_includes_closed_items_by_default() {
    let path = format!(
        "{}/../../fixtures/terminal-drift.json",
        env!("CARGO_MANIFEST_DIR")
    );
    for (filter, expected) in [
        (None, 2),
        (Some("open"), 1),
        (Some("closed"), 1),
        (Some("all"), 2),
    ] {
        let mut cmd = command();
        cmd.args(["item", "list", "--snapshot", &path, "--json", "number"]);
        if let Some(state) = filter {
            cmd.args(["--state", state]);
        }
        let output = cmd.assert().success().get_output().stdout.clone();
        let items: Vec<Value> = serde_json::from_slice(&output).unwrap();
        assert_eq!(items.len(), expected);
    }
}

#[test]
fn project_schema_summary_table() {
    for (field, expected) in [
        (
            serde_json::json!({"name":"Status","options":[{"name":"Todo"},{"name":"Done"}]}),
            "Todo, Done",
        ),
        (
            serde_json::json!({"name":"Status","options":[]}),
            "no single-select options",
        ),
        (
            serde_json::json!({"name":"Status","dataType":"TEXT"}),
            "no single-select options",
        ),
    ] {
        let mut snapshot: Value =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        snapshot["fields"] = serde_json::json!([field]);
        snapshot["items"] = serde_json::json!([]);
        snapshot["coverage"]["complete"] = false.into();
        let output = command()
            .args(["project", "view", "--snapshot", "/dev/stdin"])
            .write_stdin(serde_json::to_vec(&snapshot).unwrap())
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let text = String::from_utf8(output).unwrap();
        for expected in [
            format!("Status\t{expected}"),
            "Items: 0".into(),
            "Coverage: incomplete".into(),
        ] {
            assert!(text.contains(&expected), "{text}");
        }
    }
}

#[test]
fn live_reads_require_authentication() {
    for args in [
        vec!["snapshot", "create"],
        vec!["project", "view"],
        vec!["item", "list"],
        vec!["item", "view", "example/intake#1"],
        vec!["reconcile", "plan"],
    ] {
        let output = command()
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .args(args)
            .args(["--owner", "example", "--project", "1"])
            .assert()
            .code(4)
            .get_output()
            .clone();
        // A failed read prints no partial listing, only the error.
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("read:project")
        );
    }
}

#[test]
fn status_cli_refusals() {
    let dir = std::env::temp_dir().join(format!("agent-board-status-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("agent-board.json");
    let partial = dir.join("partial.json");
    std::fs::write(
        &config,
        r#"{"operator": "owner", "bot": "worker", "dispatchRepo": "example/control",
            "dispatchWorkflow": "dispatch.yml", "follow": ["example/code"]}"#,
    )
    .unwrap();
    std::fs::write(
        &partial,
        r#"{"operator": "owner", "bot": "worker", "dispatchRepo": "example/control", "follow": []}"#,
    )
    .unwrap();
    let (config, partial) = (config.to_str().unwrap(), partial.to_str().unwrap());
    for (args, code, message) in [
        (vec!["--config", config], 1, "status requires --owner"),
        (
            vec!["--config", config, "--snapshot", &fixture()],
            1,
            "status reads live",
        ),
        (
            vec!["--owner", "example", "--config", partial],
            1,
            "status needs dispatchWorkflow",
        ),
        (
            vec!["--owner", "example", "--config", config, "--job-limit", "0"],
            1,
            "--job-limit is not 1 to",
        ),
        (
            vec![
                "--owner",
                "example",
                "--config",
                config,
                "--json",
                "runners,items",
            ],
            1,
            "unknown JSON field: items",
        ),
        (
            vec!["--owner", "example", "--config", config, "--json", "board"],
            1,
            "--json board needs --project",
        ),
        (
            vec!["--owner", "example", "--config", config, "--json", "pools"],
            1,
            "--json pools needs the config's pace",
        ),
        // Every check above precedes the token; without one nothing is read.
        (
            vec![
                "--owner", "example", "--config", config, "--json", "runners",
            ],
            4,
            "",
        ),
    ] {
        let output = command()
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .arg("status")
            .args(&args)
            .assert()
            .code(code)
            .get_output()
            .clone();
        assert!(output.stdout.is_empty(), "{args:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(message), "{args:?}: {stderr}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn terminal_drift_preview_and_emit() {
    let fixture = format!(
        "{}/../../fixtures/terminal-drift.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let output = command()
        .args(["reconcile", "plan", "--snapshot", &fixture])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let plan: Value = serde_json::from_slice(&output).unwrap();
    let mut reasons: Vec<_> = plan["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|action| action["reason"].as_str().unwrap())
        .collect();
    reasons.sort();
    assert_eq!(reasons, ["closed_issue_not_done"]);
    assert!(
        plan["diagnostics"]["example/intake#1"]
            .as_str()
            .unwrap()
            .contains("needs_review")
    );
    let output = command()
        .args(["reconcile", "plan", "--snapshot", &fixture, "--emit"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let mut proposals: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    proposals.sort_by_key(|value| value["type"].as_str().unwrap().to_owned());
    assert_eq!(
        proposals,
        [
            serde_json::json!({"type":"update_project", "project":"https://github.com/orgs/example/projects/1", "content_type":"issue", "content_number":2, "target_repo":"example/intake", "fields":{"Status":"Done"}}),
        ]
    );
}

#[test]
fn errors_and_help() {
    for args in [
        vec!["project", "view"],
        vec!["item", "list"],
        vec!["item", "list", "--unknown"],
        vec!["item", "view", "1", "--snapshot", &fixture()],
        vec![
            "item",
            "view",
            "https://forge.example/example/intake/issues/1",
            "--snapshot",
            &fixture(),
        ],
    ] {
        command().args(args).assert().code(1);
    }
    for noun in [
        "project",
        "item",
        "ask",
        "run",
        "request",
        "snapshot",
        "reconcile",
        "output",
        "test",
    ] {
        command().args([noun, "--help"]).assert().success();
    }
    Command::new(assert_cmd::cargo::cargo_bin!("gh-agent-board"))
        .args(["item", "list", "--snapshot", &fixture()])
        .assert()
        .success();
}

#[test]
fn machine_errors_table() {
    for args in [
        vec!["item", "list", "--status", "Done", "--json", "nonexistent"],
        vec![
            "item",
            "list",
            "--json",
            "number",
            "--jq",
            "unknown_function",
        ],
        vec!["item", "list", "--json", "number", "--jq", "["],
        vec!["reconcile", "plan", "--emit", "--json", "actions"],
        vec!["item", "list", "--template", "{{.title}}"],
    ] {
        command()
            .args(args)
            .args(["--snapshot", &fixture()])
            .assert()
            .code(1);
    }
    for (mode, code) in [
        (vec!["--project", "2"], 4),
        (vec!["--create-project"], 4),
        (vec![], 1),
        (vec!["--project", "0"], 1),
        (vec!["--project", "2", "--create-project"], 1),
    ] {
        command()
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .args([
                "test",
                "project",
                "--owner",
                "cgwalters-forge-stage",
                "--repo",
                "cgwalters-forge-stage/board-test",
            ])
            .args(mode)
            .assert()
            .code(code);
    }
    command()
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .args([
            "test",
            "project",
            "--owner",
            "cgwalters-forge-stage",
            "--repo",
            "cgwalters-forge-stage/board-test",
            "--sweep",
        ])
        .assert()
        .code(1);
}

#[test]
fn ask_preview_and_list_table() {
    let mut board: Value =
        serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
    board["items"][1]["labels"] = serde_json::json!(["question"]);
    let mut closed = board["items"][1].clone();
    closed["identity"]["number"] = 3.into();
    closed["state"] = "closed".into();
    closed["state_reason"] = "COMPLETED".into();
    closed["status"] = "Done".into();
    board["items"].as_array_mut().unwrap().push(closed);
    // A snapshot only previews: nothing here has a token, and none is asked for.
    for (args, code, stdout, stderr) in [
        (
            vec!["ask", "list"],
            0,
            "ITEM\tSTATUS\tPRIORITY\tTURN\tTITLE\nexample/intake#2\tTriage\tP1\tCoordinator\tAlready triaged\n".to_owned(),
            "",
        ),
        (
            vec!["ask", "list", "--json", "number,labels", "--jq", ".[].number"],
            0,
            "2\n".to_owned(),
            "",
        ),
        (
            vec!["ask", "create", "Ok?", "--item", "example/intake#1", "--assignee", "op", "-R", "example/tracker"],
            0,
            concat!(
                r#"{"body":"An operator question is being opened for this item; any earlier triage verdict no longer authorizes closure.","item_number":1,"target_repo":"example/intake","type":"add_comment"}"#,
                "\n",
                r#"{"assignees":["op"],"body":"Blocks: `https://github.com/example/intake/issues/1`\n\nQ: Ok?\n\nReply with your answer in free text. Only comments by @op count.\n","labels":["question","needs-human"],"target_repo":"example/tracker","title":"Ok?","type":"create_issue"}"#,
                "\n",
                r#"{"item_number":1,"labels":["needs-human"],"target_repo":"example/intake","type":"add_labels"}"#,
                "\n"
            )
            .to_owned(),
            "not applied",
        ),
        (
            vec!["ask", "create", "Ok?", "--item", "example/intake#9", "--assignee", "op"],
            1,
            String::new(),
            "example/intake#9 is not on the board",
        ),
        (
            vec!["ask", "create", "Ok?", "--item", "example/intake#1", "--assignee", "op", "--apply"],
            1,
            String::new(),
            "cannot use --snapshot",
        ),
        (
            vec!["ask", "resolve", "example/intake#2", "Use A."],
            0,
            concat!(
                r#"{"body":"Use A.","issue_number":2,"target_repo":"example/intake","type":"close_issue"}"#,
                "\n"
            )
            .to_owned(),
            "read from the live forge",
        ),
        (
            vec!["ask", "resolve", "example/intake#1", "Use A."],
            1,
            String::new(),
            "not labelled question",
        ),
        (
            vec!["ask", "resolve", "example/intake#3", "Use A."],
            1,
            String::new(),
            "already closed",
        ),
        (
            vec!["ask", "resolve", "https://github.com/example/intake/pull/2", "A"],
            1,
            String::new(),
            "not pull requests",
        ),
        (
            vec!["ask", "resolve", "example/intake#2", "A", "--json", "url"],
            1,
            String::new(),
            "--json is unsupported",
        ),
    ] {
        let output = command()
            .args(&args)
            .args(["--snapshot", "/dev/stdin"])
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .write_stdin(board.to_string())
            .assert()
            .code(code)
            .get_output()
            .clone();
        assert_eq!(String::from_utf8(output.stdout).unwrap(), stdout, "{args:?}");
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains(stderr), "{args:?}: {error}");
    }
    // Live asks are refused before a token is read, and a token is then required.
    for (args, code, stderr) in [
        (
            vec![
                "ask",
                "create",
                "Q?",
                "--item",
                "example/intake#1",
                "--assignee",
                "a;b",
            ],
            1,
            "invalid GitHub login",
        ),
        (
            vec!["ask", "resolve", "example/intake#2", " "],
            1,
            "the answer is empty",
        ),
        (
            vec!["ask", "resolve", "example/intake#2", "A"],
            4,
            "authentication required",
        ),
    ] {
        let output = command()
            .args(&args)
            .args(["--owner", "example", "--project", "1", "--apply"])
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .assert()
            .code(code)
            .get_output()
            .clone();
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains(stderr), "{args:?}: {error}");
    }
}

#[test]
fn poll_guard_table() {
    let dir = std::env::temp_dir().join(format!("agent-board-poll-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("agent-board.json");
    std::fs::write(&config, r#"{"operator":"operator","bot":"worker","dispatchRepo":"o/tracker","follow":["o/repo"],"reviewWorkflow":"review.yml","dispatchWorkflow":"dispatch.yml","fixPools":["large"],"mergeAuthors":["updater[bot]"],"toilFixPool":"small","kindPools":{"triage":"small"}}"#).unwrap();
    // Config-only invocation reaches the authentication guard without a request.
    for mode in ["--dry-run", "--apply"] {
        let output = command()
            .args([
                "poll",
                mode,
                "--config",
                config.to_str().unwrap(),
                "--follow",
                "untrusted/input",
            ])
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .assert()
            .code(4)
            .get_output()
            .clone();
        assert!(String::from_utf8_lossy(&output.stderr).contains("authentication"));
    }
    std::fs::write(
        &config,
        r#"{"operator":"operator","bot":"worker","dispatchRepo":"o/tracker","follow":"wrong"}"#,
    )
    .unwrap();
    let output = command()
        .args(["poll", "--dry-run", "--config", config.to_str().unwrap()])
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(String::from_utf8_lossy(&output.stderr).contains("config follow"));
    let corrupt = dir.join("state.json");
    std::fs::write(&corrupt, r#"{"schema":"other"}"#).unwrap();
    let corrupt = corrupt.to_str().unwrap();
    let base = [
        "poll",
        "--operator",
        "cgwalters",
        "--bot",
        "cgwalters-bot",
        "--dispatch-repo",
        "cgwalters-forge/tracker",
    ];
    // Each fails before any request: no case here reaches the network.
    for (args, token, code, reason) in [
        (
            vec!["--follow", "o/r"],
            true,
            1,
            "pass --dry-run or --apply",
        ),
        (
            vec!["--apply", "--follow", "o/r"],
            true,
            1,
            "--dispatch-workflow <FILE>",
        ),
        (
            vec![
                "--apply",
                "--dry-run",
                "--dispatch-workflow",
                "dispatch.yml",
                "--follow",
                "o/r",
            ],
            true,
            1,
            "cannot be used with",
        ),
        (
            vec![
                "--apply",
                "--dispatch-workflow",
                "../x.yml",
                "--follow",
                "o/r",
            ],
            true,
            1,
            "invalid workflow",
        ),
        (
            vec![
                "--dry-run",
                "--review-workflow",
                ".github/workflows/review.yml",
                "--follow",
                "o/r",
            ],
            true,
            1,
            "invalid workflow",
        ),
        (
            vec!["--dry-run", "--follow", "o/r"],
            false,
            4,
            "authentication",
        ),
        (vec!["--dry-run"], true, 1, "at least one repository"),
        (
            vec!["--dry-run", "--follow", "o/r/../x"],
            true,
            1,
            "invalid repository",
        ),
        (
            vec!["--dry-run", "--watch", "o/r?per_page=1"],
            true,
            1,
            "invalid repository",
        ),
        (
            vec!["--dry-run", "--follow", "o/r", "--json", "x"],
            true,
            1,
            "--json is unsupported",
        ),
        (
            vec!["--dry-run", "--follow", "o/r", "--state", corrupt],
            true,
            1,
            "may be deleted",
        ),
    ] {
        let mut command = command();
        command.env_remove("GH_TOKEN").env_remove("GITHUB_TOKEN");
        if token {
            command.env("GH_TOKEN", "unused");
        }
        let output = command
            .args(base)
            .args(&args)
            .assert()
            .code(code)
            .stdout("")
            .get_output()
            .clone();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(reason), "{args:?}: {stderr}");
    }
    // A refused pass leaves the state as it was.
    assert_eq!(
        std::fs::read_to_string(corrupt).unwrap(),
        r#"{"schema":"other"}"#
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn dispatch_plan_guard_table() {
    let base = [
        "dispatch",
        "cgwalters-forge/tracker#540",
        "--caller",
        "cgwalters-forge/tracker",
        "--operator",
        "cgwalters",
    ];
    // Each fails before any request: no case here reaches the network.
    for (args, token, code, reason) in [
        (vec![], true, 1, "pass --dry-run or --apply"),
        (vec!["--dry-run"], false, 4, "authentication"),
        (vec!["--apply"], false, 4, "authentication"),
        (vec!["--dry-run", "--apply"], true, 1, "cannot be used with"),
        (
            vec!["--apply", "--workflow", "../dispatch.yml"],
            true,
            1,
            "invalid workflow",
        ),
        (
            vec!["--dry-run", "--ref", "a..b"],
            true,
            1,
            "invalid caller ref",
        ),
        (
            vec!["--dry-run", "--planner", "bad/login"],
            true,
            1,
            "invalid login",
        ),
        (
            vec!["--dry-run", "--json", "x"],
            true,
            1,
            "--json is unsupported",
        ),
        (
            vec!["--dry-run", "--snapshot", "fixtures/board.json"],
            true,
            1,
            "not a snapshot",
        ),
        (
            vec!["--dry-run", "--hostname", "github.example"],
            true,
            1,
            "plans are read only from github.com",
        ),
        (
            vec!["--apply", "--bot", "bad/login"],
            true,
            1,
            "invalid login",
        ),
        // The batch bound is checked before a token is read.
        (
            vec!["--dry-run", "--batch", "6"],
            true,
            1,
            "6 is not in 0..=5",
        ),
    ] {
        let mut command = command();
        command.env_remove("GH_TOKEN").env_remove("GITHUB_TOKEN");
        if token {
            command.env("GH_TOKEN", "unused");
        }
        command.args(base).args(&args);
        // The workflow comes from the caller's configuration, as --caller does.
        if !args.contains(&"--workflow") {
            command.args(["--workflow", "dispatch.yml"]);
        }
        let output = command.assert().code(code).stdout("").get_output().clone();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(reason), "{args:?}: {stderr}");
    }
}

#[test]
fn supervise_offline_table() {
    let dir = std::env::temp_dir().join(format!("agent-board-supervise-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let write = |name: &str, value: Value| {
        let path = dir.join(name);
        std::fs::write(&path, value.to_string()).unwrap();
        path.to_str().unwrap().to_owned()
    };
    let config = write(
        "agent-board.json",
        serde_json::json!({"operator": "operator", "bot": "worker", "dispatchRepo": "o/tracker",
            "dispatchWorkflow": "dispatch.yml", "follow": ["o/repo"], "epics": {"repository": "o/tracker"},
            "schedule": {"repositories": ["o/repo"]}}),
    );
    let mut supervisor = serde_json::json!({"pollWorkflow": "poll.yml", "probeWorkflow": "probe.yml",
        "supervisorWorkflow": "supervisor.yml", "schedulerJob": "board",
        "implementBranchPrefix": "dispatch/implement/agent-run-", "releaseRepository": "o/agentic-job",
        "wrapperRepository": "o/actions", "wrapperPath": ".github/workflows/wrapper.yml"});
    let off = write("off.json", supervisor.clone());
    supervisor["agent"] = serde_json::json!({"pool": "large", "cooldownHours": 1,
        "requests": {"repository": "o/tracker", "issue": 5}, "log": {"repository": "o/operator", "issue": 2},
        "kinds": ["research"], "maxDispatchesPerPass": 1, "maxDispatchesPerDay": 3});
    let on = write("on.json", supervisor.clone());
    supervisor["agent"]["skipUnchangedFailures"] = true.into();
    let skip = write("skip.json", supervisor.clone());
    supervisor["wrapperPath"] = "../elsewhere".into();
    let bad = write("bad.json", supervisor);
    let pools = write(
        "pools.json",
        serde_json::json!({"large": {"agent": "a", "model": "m", "budget": "1"}}),
    );
    let other_pools = write("other-pools.json", serde_json::json!({"small": {}}));
    let ok = write(
        "ok.json",
        serde_json::json!([{"id": "poll", "status": "ok"}, {"id": "release", "status": "warn"}]),
    );
    let failing = write(
        "failing.json",
        serde_json::json!([{"id": "release", "status": "fail"}]),
    );
    let comment = write(
        "comment.json",
        serde_json::json!({"comment": {"user": {"login": "other"}, "body": "x"},
            "issue": {"number": 5, "locked": true}}),
    );
    // Each case is decided before any request: none reaches the network.
    for (args, token, code, stdout, stderr) in [
        (
            vec![
                "trigger",
                "--supervisor-config",
                &on,
                "--pools",
                &pools,
                "--health",
                &ok,
            ],
            true,
            0,
            "act=false\n",
            "no check fails",
        ),
        (
            vec![
                "trigger",
                "--supervisor-config",
                &off,
                "--pools",
                &pools,
                "--health",
                &failing,
            ],
            true,
            0,
            "act=false\n",
            "not configured",
        ),
        (
            vec![
                "trigger",
                "--supervisor-config",
                &on,
                "--pools",
                &other_pools,
                "--health",
                &ok,
            ],
            true,
            1,
            "",
            "not in the pools file",
        ),
        (
            vec!["health", "--supervisor-config", &bad],
            true,
            1,
            "",
            "invalid repository path",
        ),
        (
            vec!["health", "--supervisor-config", &on],
            false,
            4,
            "",
            "authentication",
        ),
        (
            vec![
                "execute",
                "--supervisor-config",
                &off,
                "--pools",
                &pools,
                "--result-url",
                "u",
            ],
            true,
            1,
            "",
            "not configured",
        ),
        (
            vec![
                "repeat",
                "--supervisor-config",
                &skip,
                "--health",
                &failing,
                "--previous",
                &failing,
            ],
            false,
            0,
            "skip=true\n",
            "release failing as in the previous pass",
        ),
        (
            vec!["repeat", "--supervisor-config", &skip, "--health", &failing],
            false,
            0,
            "skip=false\n",
            "",
        ),
        (
            vec![
                "repeat",
                "--supervisor-config",
                &on,
                "--health",
                &failing,
                "--previous",
                &failing,
            ],
            false,
            0,
            "skip=false\n",
            "",
        ),
        (
            vec![
                "compose",
                "--supervisor-config",
                &on,
                "--task",
                "t",
                "--event",
                &ok,
            ],
            true,
            1,
            "",
            "agent.profile is not configured",
        ),
        (
            vec![
                "compose",
                "--supervisor-config",
                &on,
                "--task",
                "t",
                "--event",
                &comment,
            ],
            true,
            1,
            "",
            "not an operator comment",
        ),
        (
            vec!["preview", "--supervisor-config", &on, "--result-url", "u"],
            true,
            1,
            "",
            "not a comment on the supervisor requests issue",
        ),
    ] {
        let mut command = command();
        command
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env("GITHUB_REPOSITORY", "o/tracker")
            .env("GITHUB_RUN_ID", "9")
            .env("DEFAULT_BRANCH", "main");
        if token {
            command.env("GH_TOKEN", "unused");
        }
        let output = command
            .args(["supervise", args[0], "--config", &config])
            .args(&args[1..])
            .assert()
            .code(code)
            .stdout(stdout)
            .get_output()
            .clone();
        let err = String::from_utf8(output.stderr).unwrap();
        assert!(err.contains(stderr), "{args:?}: {err}");
    }
    command()
        .args([
            "supervise",
            "trigger",
            "--config",
            &config,
            "--supervisor-config",
            &on,
            "--pools",
            &pools,
            "--json",
            "x",
        ])
        .assert()
        .code(1);
    // The pacing reads no settings: a lookup that worked but found no
    // previous run is a sweep; one that failed is not.
    let times = dir.join("times.json");
    // A prompt is text, even one that looks like a flag.
    for (event, prompt, previous, stdout) in [
        (
            "schedule",
            "",
            Some("missing.json"),
            "health=true\ndaily=true\n",
        ),
        ("schedule", "", None, "health=true\ndaily=false\n"),
        ("workflow_dispatch", "", None, "health=true\ndaily=false\n"),
        (
            "workflow_dispatch",
            "--help",
            None,
            "health=true\ndaily=true\n",
        ),
    ] {
        let mut command = command();
        command.args([
            "supervise",
            "due",
            "--event",
            event,
            "--prompt",
            prompt,
            "--out",
        ]);
        command.arg(&times);
        if let Some(name) = previous {
            command.arg("--previous").arg(dir.join(name));
        }
        command.assert().success().stdout(stdout);
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&times).unwrap()).unwrap();
        assert_eq!(
            written["sweep"].is_string(),
            stdout.ends_with("daily=true\n"),
            "{written}"
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn cli_reference_is_current() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/cli.md");
    let output = command().arg("help-markdown").assert().success();
    let generated = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let committed = std::fs::read_to_string(path).unwrap_or_default();
    assert!(
        generated == committed,
        "docs/cli.md does not match `agent-board help-markdown`; regenerate it with `just cli-docs`"
    );
}

/// The recorded pass of 23 October previews offline, a line per decision,
/// and refuses what would leave preview or mix sources.
#[test]
fn pass_replay_previews_without_credentials() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let replay = [
        "pass",
        "--config",
        "fixtures/pass/config.json",
        "--replay",
        "fixtures/pass/board-2026-10-23.json",
    ];
    let output = command()
        .args(replay)
        .current_dir(root)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .assert()
        .success()
        .get_output()
        .clone();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("pass: preview\n"), "{text}");
    // The recording's stuck run (agent-board#376) is old enough to have its
    // jobs read; none are recorded, so it keeps its slot.
    assert_eq!(text.lines().filter(|l| l.starts_with("start ")).count(), 6);
    assert!(
        text.ends_with("\nsummary: dispatched 6, writes 0, held 0, requests 51, points ~37\n"),
        "{text}"
    );
    for refused in [
        vec!["--apply"],
        vec!["--owner", "example"],
        vec!["--json", "lines"],
    ] {
        command()
            .args(replay)
            .args(refused)
            .current_dir(root)
            .assert()
            .code(1);
    }
    let mut unknown = replay.to_vec();
    unknown[2] = "fixtures/board.json";
    let output = command().args(unknown).current_dir(root).assert().code(1);
    let error = String::from_utf8(output.get_output().stderr.clone()).unwrap();
    assert!(error.starts_with("error: fixtures/board.json: "), "{error}");
}

/// `prune` checks its configuration, options and cache before any token is
/// read; past them, the next refusal is the missing token.
#[test]
fn prune_cli_refusals() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let dir = std::env::temp_dir().join(format!("agent-board-prune-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let boardless = dir.join("config.json");
    let mut config: serde_json::Value =
        serde_json::from_str(include_str!("../../../fixtures/pass/config.json")).unwrap();
    config.as_object_mut().unwrap().remove("board");
    config["wip"] = 0.into();
    std::fs::write(&boardless, config.to_string()).unwrap();
    let pass = "fixtures/pass/config.json";
    for (args, code, reason) in [
        (vec!["--config", pass, "--json", "x"], 1, "--format json"),
        (vec!["--config", pass, "--owner", "x"], 1, "--config only"),
        (
            vec!["--config", pass, "--stale-days", "0"],
            1,
            "invalid value",
        ),
        (
            vec!["--config", pass, "--format", "html"],
            1,
            "invalid value",
        ),
        (
            vec!["--config", pass, "--cache", "fixtures/board.json"],
            1,
            "unsupported cache schema",
        ),
        (
            vec!["--config", boardless.to_str().unwrap()],
            1,
            "needs the configuration's board",
        ),
        (vec!["--config", pass], 4, "GH_TOKEN"),
    ] {
        let output = command()
            .arg("prune")
            .args(&args)
            .current_dir(root)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .assert()
            .code(code)
            .get_output()
            .clone();
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains(reason), "{args:?}: {error}");
        assert!(output.stdout.is_empty());
    }
    std::fs::remove_dir_all(dir).unwrap();
}

/// `item search` and `item list --off-board` check their options, configuration
/// and cache before any token is read, as prune does.
#[test]
fn item_read_cli_refusals() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let pass = "fixtures/pass/config.json";
    let off_board = ["item", "list", "--off-board", "--config", pass];
    let search = ["item", "search", "widget", "--config", pass];
    for (args, extra, code, reason) in [
        (&off_board[..], &["--json", "x"][..], 1, "--config only"),
        (
            &off_board,
            &["--snapshot", "fixtures/board.json"],
            1,
            "--config only",
        ),
        (&off_board, &["--status", "Todo"], 1, "cannot be used with"),
        (&off_board, &["--state", "open"], 1, "cannot be used with"),
        (&off_board[..3], &[], 1, "needs --config"),
        (&["item", "list"], &["--cache", "x"], 1, "for --off-board"),
        (
            &search,
            &["--cache", "fixtures/board.json"],
            1,
            "unsupported cache schema",
        ),
        (&search, &["-R", "example/app"], 1, "--config only"),
        (&off_board, &[], 4, "GH_TOKEN"),
        (&search, &[], 4, "GH_TOKEN"),
    ] {
        let output = command()
            .args(args)
            .args(extra)
            .current_dir(root)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .assert()
            .code(code)
            .get_output()
            .clone();
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains(reason), "{args:?} {extra:?}: {error}");
        assert!(output.stdout.is_empty());
    }
}

/// Outside GitHub Actions, `pass --apply` needs both caps, and is refused
/// before any token is read without them; with them, it warns that it is
/// not serialized. Inside Actions it still needs its run.
#[test]
fn pass_local_apply_needs_both_caps() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let apply = ["pass", "--config", "fixtures/pass/config.json", "--apply"];
    let local = "--apply outside GitHub Actions needs both --max-dispatch and --max-writes";
    let warning = "warning: applying outside GitHub Actions, not serialized against the scheduled workflow's passes";
    // Extra arguments, whether in Actions, the exit code, the error and
    // stdout's lines. Past the gate, the next refusal is the missing token.
    type Case<'a> = (&'a [&'a str], bool, i32, &'a str, &'a [&'a str]);
    let cases: &[Case] = &[
        (&[], false, 1, local, &[]),
        (&["--max-dispatch", "1"], false, 1, local, &[]),
        (&["--max-writes", "3"], false, 1, local, &[]),
        (
            &["--max-dispatch", "1", "--max-writes", "3"],
            false,
            4,
            "GH_TOKEN",
            &["pass: apply", warning],
        ),
        (
            &["--max-dispatch", "1", "--max-writes", "3"],
            true,
            1,
            "--apply runs in a GitHub Actions run, which sets GITHUB_REPOSITORY",
            &[],
        ),
    ];
    for (args, actions, code, error, stdout) in cases {
        let mut command = command();
        command
            .args(apply)
            .args(*args)
            .current_dir(root)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GITHUB_REPOSITORY")
            .env_remove("GITHUB_RUN_ID")
            .env_remove("GITHUB_ACTIONS");
        if *actions {
            command.env("GITHUB_ACTIONS", "true");
        }
        let output = command.assert().code(*code).get_output().clone();
        let err = String::from_utf8(output.stderr).unwrap();
        assert!(err.contains(error), "{args:?}: {err}");
        let out = String::from_utf8(output.stdout).unwrap();
        assert_eq!(out.lines().collect::<Vec<_>>(), *stdout, "{args:?}");
    }
}
