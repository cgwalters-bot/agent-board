use assert_cmd::Command;
use serde_json::Value;

fn command() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("agent-board"))
}

fn fixture() -> String {
    format!("{}/../../fixtures/board.json", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn status_update_cli_refusals() {
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
    ] {
        let output = command().args(args).assert().code(1).get_output().clone();
        assert!(String::from_utf8(output.stderr).unwrap().contains(reason));
        assert!(output.stdout.is_empty());
    }
}

/// promote refuses before reading a token or GitHub; with a valid dry run and no token
/// it stops at authentication (exit 4), having made no request. Its config is required
/// and a missing key is named.
#[test]
fn promote_cli_refusals() {
    let directory =
        std::env::temp_dir().join(format!("agent-board-promote-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let shipped = format!("{}/../../policy/promote.toml", env!("CARGO_MANIFEST_DIR"));
    let partial = directory.join("promote.toml");
    let text: String = std::fs::read_to_string(&shipped)
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with("approver ="))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&partial, text).unwrap();
    let partial = partial.to_str().unwrap();
    let missing = directory.join("missing.toml");
    let missing = missing.to_str().unwrap();
    let url = "https://github.com/cgwalters-forge/proj/pull/1";
    for (args, code, reason) in [
        (
            vec!["promote", url, "--config", &shipped],
            1,
            "only runs with --dry-run",
        ),
        (
            vec!["promote", url, "--dry-run"],
            1,
            "required arguments were not provided:\n  --config <FILE>",
        ),
        (
            vec!["promote", url, "--dry-run", "--config", partial],
            1,
            "missing field `approver`",
        ),
        (
            vec!["promote", url, "--dry-run", "--config", missing],
            1,
            "read promote config",
        ),
        (
            vec![
                "promote",
                "https://github.com/acme/proj/pull/1",
                "--dry-run",
                "--config",
                &shipped,
            ],
            1,
            "expected a PR URL in one of the forks",
        ),
        (
            vec![
                "promote",
                url,
                "--dry-run",
                "--config",
                &shipped,
                "--snapshot",
                &fixture(),
            ],
            1,
            "--snapshot and --json are unsupported",
        ),
        (
            vec![
                "promote",
                url,
                "--dry-run",
                "--config",
                &shipped,
                "--hostname",
                "github.example",
            ],
            1,
            "unsupported GitHub host",
        ),
        (
            vec!["promote", url, "--dry-run", "--config", &shipped],
            4,
            "authentication required",
        ),
    ] {
        let output = command()
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .args(args)
            .assert()
            .code(code)
            .get_output()
            .clone();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(reason), "{reason}: {stderr}");
        assert!(output.stdout.is_empty());
    }
    std::fs::remove_dir_all(directory).unwrap();
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

#[test]
fn emitted_proposal() {
    let output = command()
        .args(["reconcile", "plan", "--snapshot", &fixture(), "--emit"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["type"], "update_project");
    assert_eq!(value["fields"]["Status"], "Triage");
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
            .stderr
            .clone();
        assert!(String::from_utf8(output).unwrap().contains("read:project"));
    }
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
    board["fields"][0]["options"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"name":"Needs human"}));
    board["items"][1]["labels"] = serde_json::json!(["question"]);
    let mut closed = board["items"][1].clone();
    closed["identity"]["number"] = 3.into();
    closed["state"] = "closed".into();
    closed["state_reason"] = "COMPLETED".into();
    closed["status"] = "Done".into();
    board["items"].as_array_mut().unwrap().push(closed);
    let update = |number: u64, status: &str| {
        format!(
            "{{\"content_number\":{number},\"content_type\":\"issue\",\"fields\":{{\"Status\":\"{status}\"}},\"project\":\"https://github.com/orgs/example/projects/1\",\"target_repo\":\"example/intake\",\"type\":\"update_project\"}}\n"
        )
    };
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
            format!(
                "{}\n{}",
                r#"{"assignees":["op"],"body":"Blocks: `https://github.com/example/intake/issues/1`\n\nQ: Ok?\n\nReply with your answer in free text. Only comments by @op count.\n","labels":["question"],"target_repo":"example/tracker","title":"Ok?","type":"create_issue"}"#,
                update(1, "Needs human")
            ),
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
            format!(
                "{}\n{}",
                r#"{"body":"Use A.","issue_number":2,"target_repo":"example/intake","type":"close_issue"}"#,
                update(2, "Done")
            ),
            "not applied",
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
        (vec!["--follow", "o/r"], true, 1, "pass --dry-run"),
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
        (vec![], true, 1, "pass --dry-run"),
        (vec!["--dry-run"], false, 4, "authentication"),
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
}
