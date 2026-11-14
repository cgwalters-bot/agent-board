use assert_cmd::Command;
use serde_json::Value;

fn command() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("agent-board"))
}

fn fixture() -> String {
    format!("{}/../../fixtures/board.json", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn migration_apply_requires_explicit_bounds_and_live_mode() {
    let dir =
        std::env::temp_dir().join(format!("agent-board-migration-args-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (args, diagnostic) in [
        (vec!["--apply"], "--max"),
        (vec!["--apply", "--max", "1"], "--state"),
        (vec!["--apply", "--state", "journal.json"], "--max"),
        (vec!["--max", "0"], "--max must be positive"),
        (
            vec!["--apply", "--max", "0", "--state", "journal.json"],
            "--max must be positive",
        ),
        (
            vec![
                "--apply",
                "--max",
                "1",
                "--state",
                "journal.json",
                "--replay",
                "reads.json",
            ],
            "cannot be used with",
        ),
        (
            vec![
                "--apply",
                "--max",
                "1",
                "--state",
                "journal.json",
                "--record",
                "reads.json",
            ],
            "cannot be used with",
        ),
        (
            vec!["--record", "reads.json", "--replay", "reads.json"],
            "cannot be used with",
        ),
    ] {
        let output = command()
            .current_dir(&dir)
            .args([
                "migrate",
                "midstream-issues",
                "--config",
                "missing-config.json",
            ])
            .args(&args)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .assert()
            .code(1)
            .get_output()
            .clone();
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(diagnostic),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "{args:?}");
    }
    std::fs::remove_dir(dir).unwrap();
}

#[test]
fn migration_cli_preserves_existing_lock_and_releases_its_own_on_error() {
    let dir =
        std::env::temp_dir().join(format!("agent-board-migration-lock-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = dir.join("journal.json");
    let lock = state.with_extension("migration-lock");
    let journal = b"{\"receipts\":{}}\n";
    let existing_lock = b"inspect the prior run before removing this lock\n";
    std::fs::write(&state, journal).unwrap();
    std::fs::write(&lock, existing_lock).unwrap();
    // No credentials or readable configuration: these checks must happen locally.
    // A preview does not acquire/remove the apply lock; an apply cannot steal it.
    for (apply, diagnostic) in [
        (false, "read missing-config.json"),
        (true, "migration journal is locked"),
    ] {
        let mut cmd = command();
        cmd.current_dir(&dir)
            .args([
                "migrate",
                "midstream-issues",
                "--config",
                "missing-config.json",
                "--state",
                "journal.json",
                "--max",
                "1",
            ])
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN");
        if apply {
            cmd.arg("--apply");
        }
        let output = cmd.assert().code(1).get_output().clone();
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(diagnostic),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(&lock).unwrap(), existing_lock);
        assert_eq!(std::fs::read(&state).unwrap(), journal);
    }
    std::fs::remove_file(&lock).unwrap();
    let output = command()
        .current_dir(&dir)
        .args([
            "migrate",
            "midstream-issues",
            "--config",
            "missing-config.json",
            "--apply",
            "--state",
            "journal.json",
            "--max",
            "1",
        ])
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("read missing-config.json"));
    assert!(
        !lock.exists(),
        "ordinary errors must release the acquired lock"
    );
    assert_eq!(std::fs::read(&state).unwrap(), journal);
    std::fs::remove_dir_all(dir).unwrap();
}

/// What a preview of the recorded pass spends: its stages, then every
/// endpoint class and the ten causes with the most requests.
const REPLAY_SPENT: &str = r#"requests 57 (epics 17, schedule 37, status 3), endpoints {"graphql repository.issue":31,"actions workflows runs":12,"graphql repositoryOwner.projectV2":6,"graphql repository.defaultBranchRef":2,"actions runs jobs":1,"graphql node.fields":1,"graphql node.items":1,"graphql repository.issues":1,"graphql repository.pullRequests":1,"issues comments":1}, causes {"epics":11,"schedule":10,"cgwalters-forge/tracker#527":5,"status":3,"cgwalters-forge/agentic-job#188":2,"cgwalters-forge/tracker#468":2,"epic cgwalters-forge/tracker#596":2,"epic cgwalters-forge/tracker#766":2,"epic cgwalters-forge/tracker#890":2,"cgwalters-forge/agent-board#40":1,"... 17 more":17}, points ~43"#;

/// The recorded pass's candidates stage 4 did not start, by why.
const REPLAY_NOT_READY: &str = r#"not ready {"epic-not-live":22,"dispatched":2,"overlap":1}"#;

#[test]
fn review_setup_rejects_invalid_config_before_token_lookup() {
    let dir = std::env::temp_dir().join(format!("agent-board-review-setup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    for (field, value, diagnostic) in [
        (
            "workRepositories",
            serde_json::json!([]),
            "needs workRepositories",
        ),
        (
            "workRepositories",
            serde_json::json!(["owner/repo?escape"]),
            "invalid",
        ),
        (
            "workRepositories",
            serde_json::json!((0..51).map(|n| format!("owner/r{n}")).collect::<Vec<_>>()),
            "more than 50",
        ),
    ] {
        let mut config: Value =
            serde_json::from_str(include_str!("../../../fixtures/pass/config.json")).unwrap();
        config["pr"]["reviewWorkflow"] = "review.yml".into();
        config[field] = value;
        std::fs::write(&path, config.to_string()).unwrap();
        let result = command()
            .args(["test", "review-setup", "--config"])
            .arg(&path)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .assert()
            .failure()
            .get_output()
            .clone();
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(diagnostic),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn maintain_digest_check_is_offline_for_file_and_stdin() {
    let valid = include_str!("../../../fixtures/pass/maintain-digest.md");
    for (body, code, diagnostic) in [
        (valid.to_owned(), 0, "Valid maintain digest"),
        (valid.replace("\"up/proj#7\"", "7"), 1, "approve[0].pr"),
        (
            "not a digest".into(),
            1,
            "missing agent-board-maintain-digest",
        ),
    ] {
        let result = command()
            .args(["check", "maintain-digest"])
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .write_stdin(body)
            .assert()
            .code(code)
            .get_output()
            .clone();
        assert!(result.stdout.is_empty());
        assert!(String::from_utf8_lossy(&result.stderr).contains(diagnostic));
    }
    command()
        .args(["check", "maintain-digest", "--body"])
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/pass/maintain-digest.md"
        ))
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .assert()
        .success();
}

/// The session batch check reads a comment body offline, prints one typed
/// action a line and names a refused action; the bounds come from flags.
#[test]
fn session_batch_check_is_offline_and_bounded() {
    let valid = include_str!("../../../fixtures/pass/session-batch.md");
    let bounds = [
        "--allow-repo",
        "cgwalters-forge/agent-board",
        "--allow-repo",
        "cgwalters-forge/tracker",
        "--priority-option",
        "P1",
        "--ask-domain",
        "budget",
    ];
    for (name, body, extra, code, diagnostic) in [
        (
            "valid",
            valid.to_owned(),
            &bounds[..],
            0,
            "Valid session batch for cgwalters-forge/agent-board with 6 actions",
        ),
        (
            "a Priority that is no option",
            valid.replace("\"P1\"", "\"P9\""),
            &bounds[..],
            1,
            "action 3: Priority \"P9\"",
        ),
        (
            "no batch",
            "not a batch".into(),
            &bounds[..],
            1,
            "missing agent-board-session",
        ),
        (
            "no bounds",
            valid.to_owned(),
            &[][..],
            1,
            "cgwalters-forge/agent-board is not a configured repository",
        ),
        (
            "a malformed bound",
            valid.to_owned(),
            &["--allow-repo", "cgwalters-forge"][..],
            1,
            "invalid configured repository",
        ),
        (
            "an oversize body",
            "x".repeat(1024 * 1024 + 1),
            &bounds[..],
            1,
            "candidate exceeds 1048576 bytes",
        ),
    ] {
        let result = command()
            .args(["check", "session-batch"])
            .args(extra)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .write_stdin(body)
            .assert()
            .code(code)
            .get_output()
            .clone();
        let stdout = String::from_utf8_lossy(&result.stdout);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(stderr.contains(diagnostic), "{name}: {stderr}");
        if code == 0 {
            let types: Vec<String> = stdout
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap()["type"].to_string())
                .collect();
            assert_eq!(types.len(), 6, "{name}: {stdout}");
            assert_eq!(types[0], "\"close_issue\"", "{name}");
        } else {
            assert!(stdout.is_empty(), "{name}: {stdout}");
        }
    }
    command()
        .args(["check", "session-batch", "--body"])
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/pass/session-batch.md"
        ))
        .args(bounds)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .assert()
        .success();
}

#[test]
fn pass_next_rejects_invalid_train_before_token_lookup() {
    for (extra, diagnostic) in [
        (
            vec![
                "--train-workflow",
                "../train.yml",
                "--train-stale-minutes",
                "15",
            ],
            "invalid workflow",
        ),
        (
            vec![
                "--train-workflow",
                "train.yml",
                "--train-stale-minutes",
                "15",
                "--train-input",
                "release=true",
                "--train-input",
                "release=false",
            ],
            "input release of train.yml given twice",
        ),
    ] {
        let output = command()
            .args([
                "pass",
                "next",
                "--workflow",
                "pass.yml",
                "--branch",
                "main",
                "--apply-title",
                "pass (apply)",
                "--wait-minutes",
                "10",
            ])
            .args(extra)
            .env("GITHUB_REPOSITORY", "example/board")
            .env("GITHUB_RUN_ID", "100")
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .assert()
            .code(1)
            .get_output()
            .clone();
        let err = String::from_utf8(output.stderr).unwrap();
        assert!(err.contains(diagnostic), "{err}");
    }
}

#[test]
fn deploy_preflight_report_is_offline_and_preview_only() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let output = command()
        .current_dir(root)
        .args([
            "test",
            "deploy",
            "--config",
            "fixtures/deploy-config.json",
            "--fixture",
            "fixtures/deploy-reads.json",
        ])
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .assert()
        .success()
        .get_output()
        .clone();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema"], "agent-board.deploy-preflight.v1");
    assert_eq!(report["mode"], "preview");
    assert_eq!(report["writes"], 0);
    assert_eq!(report["checks"].as_array().unwrap().len(), 8);
    assert!(output.stderr.is_empty());

    let dir = std::env::temp_dir().join(format!("agent-board-deploy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    for contents in ["{", "{}", r#"{"owner":"production"}"#] {
        std::fs::write(&path, contents).unwrap();
        let output = command()
            .args(["test", "deploy", "--config"])
            .arg(&path)
            // A missing fixture proves invalid config is refused before any reads.
            .args(["--fixture", "does-not-exist.json"])
            .assert()
            .code(1)
            .get_output()
            .clone();
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["writes"], 0);
        assert_eq!(report["checks"][0]["state"], "blocked");
    }
    // The smoke path is live only, and without credentials its preflight
    // cannot be ready, so it writes nothing.
    command()
        .current_dir(root)
        .args([
            "test",
            "deploy",
            "--config",
            "fixtures/deploy-config.json",
            "--fixture",
            "fixtures/deploy-reads.json",
            "--apply",
        ])
        .assert()
        .code(1);
    let output = command()
        .current_dir(root)
        .args([
            "test",
            "deploy",
            "--config",
            "fixtures/deploy-config.json",
            "--apply",
        ])
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .assert()
        .code(1)
        .get_output()
        .clone();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema"], "agent-board.deploy-smoke.v1");
    assert_eq!(report["writes"], 0);
    assert_eq!(report["smoke"], Value::Null);
    assert_eq!(report["checks"][1]["state"], "unverified");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn deploy_preflight_fixture_failures_produce_blocked_reports() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let dir =
        std::env::temp_dir().join(format!("agent-board-deploy-fixture-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("reads.json");
    for contents in [
        None,
        Some("["),
        Some("{}"),
        Some("[]"),
        Some("[{}, {}, {}]"),
        Some("[{}, {}, {}, {}, {}]"),
    ] {
        if let Some(contents) = contents {
            std::fs::write(&path, contents).unwrap();
        }
        let output = command()
            .current_dir(root)
            .args([
                "test",
                "deploy",
                "--config",
                "fixtures/deploy-config.json",
                "--fixture",
            ])
            .arg(&path)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .assert()
            .code(1)
            .get_output()
            .clone();
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["schema"], "agent-board.deploy-preflight.v1");
        assert_eq!(report["writes"], 0);
        assert_eq!(report["checks"].as_array().unwrap().len(), 1);
        assert_eq!(report["checks"][0]["group"], "fixture");
        assert_eq!(report["checks"][0]["state"], "blocked");
        assert!(
            !String::from_utf8(output.stdout)
                .unwrap()
                .contains(path.to_str().unwrap())
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn deploy_preflight_rejects_unsupported_global_options() {
    for flags in [
        vec!["--json", "checks"],
        vec!["--json", "checks", "--jq", ".checks"],
        vec!["--board-config", "does-not-exist.json"],
        vec!["--template", "{{.checks}}"],
    ] {
        let output = command()
            .args(["test", "deploy", "--config", "does-not-exist.json"])
            .args(&flags)
            .assert()
            .code(1)
            .get_output()
            .clone();
        assert!(output.stdout.is_empty(), "{flags:?}");
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("unsupported"),
            "{flags:?}"
        );
    }
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
            .env_remove("DISPATCH_TOKEN")
            .assert()
            .success()
            .get_output()
            .clone();
        assert!(!output.stdout.is_empty(), "{line}");
        if line.starts_with("agent-board pass ") {
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(text.starts_with("pass: preview\n"), "{text}");
            assert_eq!(text.lines().filter(|l| l.starts_with("start ")).count(), 1);
            assert!(text.contains("held by --max-dispatch: start "), "{text}");
            assert!(text.ends_with(&format!("status: 0 change(s) of 0 item(s) read\nsummary: dispatched 1/1, writes 3/3, held 5, {REPLAY_SPENT}, starts {{}}, comments created 0, edited 0, {REPLAY_NOT_READY}, planned starts {{\"triage\":1}}\n")), "{text}");
            assert!(output.stderr.is_empty(), "{line}");
        }
        count += 1;
    }
    assert_eq!(count, 4);

    // Keep the separate, work-off live starter valid without contacting a forge.
    let live = setup
        .split_once("6. **Preview the first live pass")
        .unwrap()
        .1;
    let json = live
        .split_once("```json\n")
        .unwrap()
        .1
        .split_once("```")
        .unwrap()
        .0;
    let config = board_forge::pass::Config::parse(json).unwrap();
    assert_eq!(config.wip, 0);
    assert!(config.follow.is_empty());
    assert!(config.epics.is_none());
    assert!(config.triage.is_none());
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
        // Without --since, the repository diff, from the caller's checkout.
        (vec!["changes"], "read .github/pass.json"),
        (
            vec!["changes", "--until", "2026-10-21T00:00:00Z"],
            "--since",
        ),
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

/// The snapshot board-forge's GitLab reader makes of the issue listing and
/// note fixtures (its unit test keeps the two in step).
#[test]
fn gitlab_snapshot_table() {
    let fixture = format!(
        "{}/../../fixtures/gitlab/board.json",
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
            // The JSON url is the synthesized one, as for Forgejo; the
            // exact per-forge link is the item's evidence URL.
            "https://gitlab.example/sample/board/issues/3\n",
        ),
        (
            vec![
                "item",
                "view",
                "sample/board#1",
                "--hostname",
                "gitlab.example",
                "--json",
                "fields",
                "--jq",
                ".fields.Run",
            ],
            "https://gitlab.example/sample/runner/-/pipelines/41\n",
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
    let gitlab = dir.join("gitlab.json");
    std::fs::write(
        &gitlab,
        r#"{"board":{"forge":"gitlab","url":"https://gitlab.example/","tokenEnv":"AGENT_BOARD_TEST_GITLAB_TOKEN","repositories":["sample/board"],"membership":"open"}}"#,
    )
    .unwrap();
    let unknown = dir.join("unknown.json");
    std::fs::write(&unknown, r#"{"board":{"forge":"gitea"}}"#).unwrap();
    let (forgejo, invalid, github, gitlab, unknown) = (
        forgejo.to_str().unwrap(),
        invalid.to_str().unwrap(),
        github.to_str().unwrap(),
        gitlab.to_str().unwrap(),
        unknown.to_str().unwrap(),
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
        // The GitLab read board refuses the same way, and refuses every
        // write command and intake outright.
        (
            vec!["snapshot", "create", "--board-config", gitlab],
            4,
            "AGENT_BOARD_TEST_GITLAB_TOKEN",
        ),
        (
            vec!["reconcile", "plan", "--board-config", gitlab],
            4,
            "AGENT_BOARD_TEST_GITLAB_TOKEN",
        ),
        (
            vec!["item", "add", "sample/board#1", "--board-config", gitlab],
            1,
            "only read by",
        ),
        (
            vec!["intake", "--board-config", gitlab, "--json"],
            1,
            "only read by",
        ),
        // An unknown forge is a decode failure, not GitHub.
        (
            vec!["item", "list", "--board-config", unknown],
            1,
            "unknown variant",
        ),
        (
            vec!["reconcile", "plan", "--board-config", forgejo],
            4,
            "AGENT_BOARD_TEST_FORGEJO_TOKEN",
        ),
        (
            vec!["intake", "--board-config", forgejo, "--json"],
            1,
            "board.intake is not configured",
        ),
        (
            vec!["intake", "--json"],
            1,
            "intake requires --board-config",
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
            .env_remove("AGENT_BOARD_TEST_GITLAB_TOKEN")
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
fn supervise_previous_requires_explicit_identity_and_never_claims_failed_reads() {
    let args = [
        "supervise",
        "previous",
        "--repository",
        "example/control",
        "--workflow",
        ".github/workflows/health.yml",
        "--artifact",
        "health-report",
        "--branch",
        "default",
        "--event",
        "schedule",
        "--event",
        "issue_comment",
        "--destination",
        "previous-health",
    ];
    let failed = command()
        .args(args)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .assert()
        .failure()
        .get_output()
        .clone();
    assert!(failed.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("GH_TOKEN"));
    for flag in [
        "--repository",
        "--workflow",
        "--artifact",
        "--branch",
        "--event",
        "--destination",
    ] {
        let mut missing = args.to_vec();
        // Remove all occurrences, including repeated --event values.
        while let Some(i) = missing.iter().position(|v| *v == flag) {
            missing.drain(i..i + 2);
        }
        command().args(missing).assert().failure().stdout("");
    }
    let mut invalid = args;
    invalid[3] = "../foreign";
    let failed = command()
        .args(invalid)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .assert()
        .failure()
        .get_output()
        .clone();
    assert!(failed.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("invalid repository"));
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

/// A dry run of a daily session (agent-board#830) on the recorded board:
/// with `sessions` naming agent-board#40 as the anchor, the preview plans
/// the session start first, triages that issue no more, and writes
/// nothing.
#[test]
fn pass_replay_previews_a_daily_session() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let mut config: serde_json::Value =
        serde_json::from_str(include_str!("../../../fixtures/pass/config.json")).unwrap();
    config["triagePools"] = serde_json::json!(["claude"]);
    config["sessions"] = serde_json::json!({
        "hourUtc": 0, "idleDays": 1, "issues": ["cgwalters-forge/agent-board#40"],
        "compareAfter": "2026-10-22T00:00:00Z"
    });
    let path = std::env::temp_dir().join(format!(
        "agent-board-session-config-{}.json",
        std::process::id()
    ));
    std::fs::write(&path, config.to_string()).unwrap();
    // Synthetic dispatch history: evidence of the log contract, not live savings.
    let mut recording: Value =
        serde_json::from_str(include_str!("../../../fixtures/pass/board-2026-10-23.json")).unwrap();
    recording["requests"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "request": {"get": "https://api.github.com/repos/cgwalters-forge/agent-board"},
            "answer": {"status": 200, "body": {"has_issues": true}}
        }));
    // The historical recording predates activity fields on the first anchor
    // page. Supply an explicit, synthetic quiet repository for this replay.
    let mut activity = recording["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| {
            entry["request"]["variables"]["name"] == "agent-board"
                && entry["request"]["variables"]["number"] == 40
                && entry["answer"]["data"]["repository"]["issue"]["comments"].is_object()
        })
        .unwrap()
        .clone();
    activity["request"]["graphql"] = "6d40afa20e813ee9".into();
    activity["answer"]["data"]["repository"]["sessionIssues"] = serde_json::json!({"nodes": []});
    activity["answer"]["data"]["repository"]["sessionPulls"] = serde_json::json!({"nodes": []});
    activity["answer"]["data"]["repository"]["sessionDefault"] = Value::Null;
    recording["requests"].as_array_mut().unwrap().push(activity);
    for (day, kind) in [(21, "triage"), (22, "session")] {
        let url = format!(
            "https://api.github.com/repos/cgwalters-forge/tracker/actions/workflows/dispatch.yml/runs?event=workflow_dispatch&created=2026-10-{day}T00%3A00%3A00Z..2026-10-{day}T23%3A59%3A59Z&per_page=100&page=1"
        );
        recording["requests"].as_array_mut().unwrap().push(serde_json::json!({
            "request": {"get": url}, "answer": {"status": 200, "link": null,
                "body": {"total_count": 1, "workflow_runs": [{"id": day,
                    "created_at": format!("2026-10-{day}T00:00:00Z"),
                    "display_title": format!("{kind} cgwalters-forge/agent-board#40 (claude)")}]}}
        }));
    }
    let history_path = path.with_extension("replay.json");
    std::fs::write(&history_path, recording.to_string()).unwrap();
    let output = command()
        .args(["pass", "--config"])
        .arg(&path)
        .arg("--replay")
        .arg(&history_path)
        .current_dir(root)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .assert()
        .success()
        .get_output()
        .clone();
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(&history_path).unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    let starts: Vec<&str> = text.lines().filter(|l| l.starts_with("start ")).collect();
    assert_eq!(
        starts.first(),
        Some(&"start session cgwalters-forge/agent-board#40 pool=claude"),
        "{text}"
    );
    assert_eq!(starts.len(), 6, "{text}");
    assert!(text.contains("session comparison cgwalters-forge/agent-board: before=[2026-10-21T00:00:00Z,2026-10-22T00:00:00Z) after=[2026-10-22T00:00:00Z,2026-10-23T00:00:00Z) before: triage=1 session=0; after: triage=0 session=1; listed dispatch creations, not inference savings"), "{text}");
    assert!(
        !text.contains("triage cgwalters-forge/agent-board#40"),
        "{text}"
    );
    assert!(
        text.ends_with(", planned starts {\"session\":1,\"triage\":5}\n"),
        "{text}"
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
        .env("PATH", "")
        .assert()
        .success()
        .get_output()
        .clone();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("pass: preview\n"), "{text}");
    // The recording's stuck run (agent-board#376) is old enough to have its
    // jobs read; none are recorded, so it keeps its slot. tracker#596's
    // sub-issues are all closed: it is asked about, three writes, not planned.
    assert_eq!(text.lines().filter(|l| l.starts_with("start ")).count(), 6);
    assert!(
        text.ends_with(&format!("\nsummary: dispatched 6, writes 3, held 0, {REPLAY_SPENT}, starts {{}}, comments created 0, edited 0, {REPLAY_NOT_READY}, planned starts {{\"triage\":6}}\n")),
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

#[test]
fn pass_checkout_defaults_match_explicit_flags() {
    let root = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    let dir = std::env::temp_dir().join(format!("agent-board-defaults-{}", std::process::id()));
    let github = dir.join(".github");
    std::fs::create_dir_all(&github).unwrap();
    let path = github.join("pass.json");
    let bounds = github.join("fix.toml");
    std::fs::write(
        &bounds,
        "repos = [\"cgwalters-forge/tracker\"]\n[outputs.push_to_pull_request_branch]\nbranches = [\"dispatch/**\"]\n",
    )
    .unwrap();
    let base: Value =
        serde_json::from_str(include_str!("../../../fixtures/pass/config.json")).unwrap();
    let mut config = base.clone();
    config["fixBounds"] = serde_json::json!(["fix.toml"]);
    config["maxDispatch"] = 1.into();
    config["maxWrites"] = 3.into();
    config["maxStatusChanges"] = 5.into();
    std::fs::write(&path, config.to_string()).unwrap();
    let fixture = root.join("fixtures/pass/board-2026-10-23.json");
    let preview = |cwd: &std::path::Path, extra: &[&str]| {
        command()
            .args(["pass", "--replay"])
            .arg(&fixture)
            .args(extra)
            .current_dir(cwd)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env("PATH", "")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let defaults = preview(&dir, &[]);
    let explicit = preview(
        root,
        &[
            "--config",
            "fixtures/pass/config.json",
            "--fix-bounds",
            bounds.to_str().unwrap(),
            "--max-dispatch",
            "1",
            "--max-writes",
            "3",
            "--max-status-changes",
            "5",
        ],
    );
    assert_eq!(defaults, explicit);
    // Explicit config paths use the config's directory, not the working directory.
    assert_eq!(
        preview(root, &["--config", path.to_str().unwrap()]),
        defaults
    );
    // Each CLI cap, including zero, replaces the config value independently.
    for flag in ["--max-dispatch", "--max-writes", "--max-status-changes"] {
        let overridden = preview(&dir, &[flag, "0"]);
        let mut expected = config.clone();
        expected[match flag {
            "--max-dispatch" => "maxDispatch",
            "--max-writes" => "maxWrites",
            _ => "maxStatusChanges",
        }] = 0.into();
        std::fs::write(&path, expected.to_string()).unwrap();
        assert_eq!(overridden, preview(&dir, &[]));
        if flag != "--max-status-changes" {
            assert_ne!(overridden, defaults, "{flag}");
        }
        std::fs::write(&path, config.to_string()).unwrap();
    }
    // Every configured bounds file is checked even by --check-config. CLI
    // bounds replace the whole list, rather than widening it by union.
    config["fixBounds"] = serde_json::json!(["fix.toml", "missing.toml"]);
    std::fs::write(&path, config.to_string()).unwrap();
    let refused = command()
        .args(["pass", "--check-config"])
        .current_dir(&dir)
        .assert()
        .code(1);
    assert!(String::from_utf8_lossy(&refused.get_output().stderr).contains("missing.toml"));
    command()
        .args(["pass", "--check-config", "--fix-bounds"])
        .arg(&bounds)
        .current_dir(&dir)
        .assert()
        .success();
    // Config caps satisfy the existing local apply gate, not the token gate.
    config["fixBounds"] = serde_json::json!([]);
    std::fs::write(&path, config.to_string()).unwrap();
    let refused = command()
        .args(["pass", "--apply"])
        .current_dir(&dir)
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env("PATH", "")
        .assert()
        .code(4);
    assert!(String::from_utf8_lossy(&refused.get_output().stderr).contains("gh auth login"));
    // Discovery never walks upward from an unrelated current directory.
    let refused = command()
        .args(["pass", "--check-config"])
        .current_dir(&github)
        .assert()
        .code(1);
    assert!(
        String::from_utf8_lossy(&refused.get_output().stderr).contains("read .github/pass.json")
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn pass_without_environment_tokens_or_gh_reports_authentication_required() {
    let result = command()
        .args(["pass", "--config", "fixtures/pass/config.json"])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env("PATH", "")
        .assert()
        .code(4);
    let error = String::from_utf8_lossy(&result.get_output().stderr);
    assert!(error.contains("gh auth login for github.com"), "{error}");
}

#[test]
fn pass_replay_resolves_organization_sources_without_credentials() {
    let output = command()
        .args([
            "pass",
            "--config",
            "fixtures/pass/org-config.json",
            "--replay",
            "fixtures/pass/org-repositories.json",
        ])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("DISPATCH_TOKEN")
        .assert()
        .success()
        .get_output()
        .clone();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("repositories: org cgwalters-forge resolved ["),
        "{text}"
    );
    let resolved = text
        .lines()
        .find(|l| l.starts_with("repositories: /workRepositories"))
        .unwrap();
    assert!(resolved.contains("cgwalters-forge/agent-board"));
    assert!(!resolved.contains("cgwalters-forge/bootc"));
    assert!(text.contains("requests 1"), "{text}");
    assert!(!text.lines().any(|l| l.starts_with("start ")));
}

#[test]
fn organization_sources_preserve_promotion_replay_and_reconcile_config_loading() {
    let dir = std::env::temp_dir().join(format!("agent-board-org-config-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let replay_path = dir.join("replay.json");
    let mut config: Value =
        serde_json::from_str(include_str!("../../../fixtures/promote/config.json")).unwrap();
    config["budget"] =
        serde_json::json!({"harnessRepositories":[{"org":"cgwalters-forge","forks":false}]});
    let mut replay: Value = serde_json::from_str(include_str!(
        "../../../fixtures/promote/public-2026-11-04.json"
    ))
    .unwrap();
    let org: Value =
        serde_json::from_str(include_str!("../../../fixtures/pass/org-repositories.json")).unwrap();
    replay["requests"]
        .as_array_mut()
        .unwrap()
        .extend(org["requests"].as_array().unwrap().iter().cloned());
    std::fs::write(&path, config.to_string()).unwrap();
    std::fs::write(&replay_path, replay.to_string()).unwrap();
    let output = command()
        .args(["promote", "--config"])
        .arg(&path)
        .arg("--replay")
        .arg(&replay_path)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("DISPATCH_TOKEN")
        .assert()
        .success()
        .get_output()
        .clone();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("repositories: org cgwalters-forge resolved ["),
        "{text}"
    );
    assert!(
        text.contains("hold promote cgwalters-forge/composefs-rs#"),
        "{text}"
    );

    // A disabled promotion still performs no reads or token lookup.
    config.as_object_mut().unwrap().remove("promote");
    std::fs::write(&path, config.to_string()).unwrap();
    command()
        .args(["promote", "--config"])
        .arg(&path)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .assert()
        .success()
        .stdout("promote: off\n");

    // Reconcile must run source validation even with status reconciliation off,
    // rather than silently accepting an unexpanded organization object.
    std::fs::write(
        &path,
        serde_json::json!({"statusReconcile": {
            "enabled": false, "runRepositories":[{"org":"team","forks":true}]
        }})
        .to_string(),
    )
    .unwrap();
    let output = command()
        .args([
            "reconcile",
            "plan",
            "--snapshot",
            "fixtures/board.json",
            "--config",
        ])
        .arg(&path)
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .assert()
        .failure()
        .get_output()
        .clone();
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("forks: true is supported only in follow, workRepositories and pr.noMerge")
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// `pass --check-config` checks an explicit configuration as a pass would, with no
/// token or forge read, and refuses to be mixed with a pass's own options.
#[test]
fn pass_check_config_table() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let dir = std::env::temp_dir().join(format!("agent-board-check-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let base: Value =
        serde_json::from_str(include_str!("../../../fixtures/pass/config.json")).unwrap();
    let sweep = serde_json::json!({"weekday": "friday", "hourUtc": 9});
    let with = |triage: Value| {
        let mut config = base.clone();
        config["triage"] = triage;
        config["sweep"] = sweep.clone();
        config
    };
    for (name, config, extra, code, expected) in [
        ("base", base.clone(), &[][..], 0, "pass: configuration ok"),
        // tracker#1403's change (agent-board#755).
        (
            "all",
            with(Value::Bool(true)),
            &[],
            0,
            "pass: configuration ok",
        ),
        (
            "off",
            with(Value::Bool(false)),
            &[],
            1,
            "sweep needs triage",
        ),
        (
            "apply",
            base.clone(),
            &["--apply"],
            1,
            "cannot be used with",
        ),
        (
            "replay",
            base.clone(),
            &["--replay", "fixtures/pass/board-2026-10-23.json"],
            1,
            "cannot be used with",
        ),
    ] {
        let path = dir.join(format!("{name}.json"));
        std::fs::write(&path, config.to_string()).unwrap();
        let output = command()
            .args(["pass", "--check-config", "--config"])
            .arg(&path)
            .args(extra)
            .current_dir(root)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .assert()
            .code(code)
            .get_output()
            .clone();
        let said = match code {
            0 => output.stdout,
            _ => output.stderr,
        };
        let said = String::from_utf8(said).unwrap();
        assert!(said.contains(expected), "{name}: {said}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `--fix-bounds` parses twice on pass and poll, and every path it names is
/// read: two good files get past reading them, and a missing second fails.
#[test]
fn fix_bounds_repeats() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let dir = std::env::temp_dir().join(format!("agent-board-bounds-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = |name: &str, repos: &str, branches: &str| {
        let path = dir.join(name);
        let text = format!(
            "repos = [{repos:?}]\n[outputs.push_to_pull_request_branch]\nbranches = [{branches:?}]\n"
        );
        std::fs::write(&path, text).unwrap();
        path.to_str().unwrap().to_owned()
    };
    let fix = file("fix.toml", "cgwalters-forge/tracker", "dispatch/**");
    let midstream = file("midstream-fix.toml", "cgwalters-forge/bootc", "bot/**");
    let missing = dir.join("missing.toml").to_str().unwrap().to_owned();
    let pass = [
        "pass",
        "--config",
        "fixtures/pass/config.json",
        "--replay",
        "fixtures/pass/board-2026-10-23.json",
    ];
    let poll = [
        "poll",
        "--operator",
        "op",
        "--bot",
        "bot",
        "--dispatch-repo",
        "example/dispatch",
    ];
    let read_missing = format!("read {missing}");
    // The poll goes only as far as reading its files: past them, with
    // neither --dry-run nor --apply, it refuses.
    let unasked = "poll takes its steps only when asked";
    for (args, second, code, error) in [
        (pass.as_slice(), &midstream, 0, ""),
        (pass.as_slice(), &missing, 1, read_missing.as_str()),
        (poll.as_slice(), &midstream, 1, unasked),
        (poll.as_slice(), &missing, 1, read_missing.as_str()),
    ] {
        let output = command()
            .args(args)
            .args(["--fix-bounds", &fix, "--fix-bounds", second])
            .current_dir(root)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .assert()
            .code(code)
            .get_output()
            .clone();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(error), "{args:?} {second}: {stderr}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Opted-in promotion discovery replays token-free; an empty candidate list
/// changes no other decisions. Invalid configuration fails before any read.
#[test]
fn pass_replay_with_promote_discovery_is_read_only() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let dir = std::env::temp_dir().join(format!("agent-board-promote-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let base: Value =
        serde_json::from_str(include_str!("../../../fixtures/pass/config.json")).unwrap();
    let mut recording: Value =
        serde_json::from_str(include_str!("../../../fixtures/pass/board-2026-10-23.json")).unwrap();
    // Scenario edit: an empty fork discovery, using the PromoteAwaiting query
    // key and the public response shape documented in fixtures/promote/README.md.
    recording["requests"].as_array_mut().unwrap().push(serde_json::json!({
        "request":{"graphql":"ac69d4026a5e9797","variables":{"owner":"cgwalters-forge","name":"bootc"}},
        "answer":{"data":{"repository":{"pullRequests":{"pageInfo":{"hasNextPage":false},"nodes":[]}}}}
    }));
    let recording_path = dir.join("replay.json");
    std::fs::write(&recording_path, recording.to_string()).unwrap();
    let replay = |config: &Value, name: &str| {
        let path = dir.join(format!("{name}.json"));
        std::fs::write(&path, config.to_string()).unwrap();
        command()
            .args(["pass", "--config"])
            .arg(&path)
            .arg("--replay")
            .arg(&recording_path)
            .current_dir(root)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .assert()
    };
    let without = replay(&base, "without").success().get_output().clone();
    let mut with = base.clone();
    with["promote"] = serde_json::json!({
        "forks": {"cgwalters-forge/bootc": {"upstream": "bootc-dev/bootc", "base": "main"}},
        "signoff": {"name": "Op Erator", "email": "op@example.com"},
        "policyRepository": "cgwalters-forge/tracker",
        "policyPath": "upstream-policy",
    });
    let output = replay(&with, "with").success().get_output().clone();
    let without = String::from_utf8(without.stdout).unwrap();
    assert!(without.contains("\npromote: off\n"), "{without}");
    // One extra discovery read, no candidates or promotion writes. The summary
    // accounts for the extra request and GraphQL point; other decisions match.
    let decisions = |s: &str| {
        s.lines()
            .filter(|l| !l.starts_with("summary:") && *l != "promote: off")
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        decisions(&String::from_utf8(output.stdout).unwrap()),
        decisions(&without)
    );
    // The case, its edit of the configuration, and the error.
    type Case = (&'static str, fn(&mut Value), &'static str);
    let cases: [Case; 5] = [
        (
            "unknown-key",
            |v| v["promote"]["draft"] = true.into(),
            "unknown field `draft`",
        ),
        (
            "null",
            |v| v["promote"]["policyPath"] = Value::Null,
            "config.promote.policyPath is null; leave the key out instead",
        ),
        (
            "fork-not-owner-repo",
            |v| {
                v["promote"]["forks"] =
                    serde_json::json!({"bootc": {"upstream": "bootc-dev/bootc", "base": "main"}})
            },
            "promote forks: invalid repository \"bootc\": expected OWNER/REPO",
        ),
        (
            "fork-in-follow",
            |v| v["follow"] = serde_json::json!(["cgwalters-forge/bootc"]),
            "promote fork cgwalters-forge/bootc is also in follow",
        ),
        (
            "forks-without-signoff",
            |v| {
                v["promote"].as_object_mut().unwrap().remove("signoff");
            },
            "promote signoff is required with forks",
        ),
    ];
    for (case, edit, error) in cases {
        let mut config = with.clone();
        edit(&mut config);
        let output = replay(&config, case).code(1).get_output().clone();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(error), "{case}: {stderr}");
        assert!(output.stdout.is_empty(), "{case}: {:?}", output.stdout);
        let path = dir.join(format!("{case}.json"));
        let output = command()
            .args(["promote", "--config"])
            .arg(path)
            .args(["--replay", "fixtures/promote/public-2026-11-04.json"])
            .current_dir(root)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .assert()
            .code(1)
            .get_output()
            .clone();
        assert!(
            String::from_utf8(output.stderr).unwrap().contains(error),
            "{case}"
        );
        assert!(output.stdout.is_empty());
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn promote_would_promote_is_an_offline_scenario_not_public_authorization() {
    use board_forge::{
        github::Transport,
        pass::{Recorder, Replay},
    };
    struct NoPlan;
    impl Transport for NoPlan {
        fn graphql(&self, _: &str, _: Value) -> anyhow::Result<Value> {
            Ok(
                serde_json::json!({"repository":{"issue":{"comments":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}),
            )
        }
    }
    let mut replay = Replay::from_json(
        &serde_json::from_str(include_str!(
            "../../../fixtures/promote/public-2026-11-04.json"
        ))
        .unwrap(),
    )
    .unwrap();
    let extra = Replay::from_json(
        &serde_json::from_str(include_str!(
            "../../../fixtures/promote/gates-public-2026-11-04.json"
        ))
        .unwrap(),
    )
    .unwrap();
    replay.requests.extend(extra.requests);
    let policy = Replay::from_json(
        &serde_json::from_str(include_str!(
            "../../../fixtures/promote/policy-public-2026-11-04.json"
        ))
        .unwrap(),
    )
    .unwrap();
    replay.requests.extend(policy.requests);
    // Hypothetical source issue with no upstream association; the old public
    // recording predates this source-body read.
    replay.requests.push(serde_json::json!({"request":{"get":"https://api.github.com/repos/cgwalters-forge/composefs-rs/issues/31"},
        "answer":{"status":200,"body":{"number":31,"repository_url":"https://api.github.com/repos/cgwalters-forge/composefs-rs","body":"A fork issue."}}}));
    for e in &mut replay.requests {
        let url = e["request"]["get"].as_str().unwrap_or_default().to_owned();
        if url.contains("pulls?state=open") {
            let mut p = e["answer"]["body"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["number"] == 34)
                .unwrap()
                .clone();
            p["draft"] = false.into();
            p["labels"] = serde_json::json!([]);
            p["title"] = "Implement slice".into();
            p["body"] = "Part of cgwalters-forge/composefs-rs#31".into();
            e["answer"]["body"] = serde_json::json!([p]);
        } else if url.contains("/issues/34/comments?") {
            e["answer"]["body"] = serde_json::json!([{"id":1,"user":{"login":"cgwalters"},"body":"/promote","created_at":"2026-10-23T00:00:00Z","updated_at":"2026-10-23T00:00:00Z"}]);
        } else if url.contains("/compare/") {
            e["answer"]["body"]["behind_by"] = 0.into();
            e["answer"]["body"]["commits"][0]["author"]["login"] = "cgwalters-bot".into();
            e["answer"]["body"]["commits"][0]["commit"] =
                serde_json::json!({"message":"Implement slice"});
        }
        if e["request"]["variables"]["number"] == 34 {
            e["answer"]["data"]["repository"]["pullRequest"]["comments"]["nodes"] = serde_json::json!([{"url":"https://github.com/cgwalters-forge/composefs-rs/pull/34#issuecomment-1","author":{"login":"cgwalters"},"createdAt":"2026-10-23T00:00:00Z","lastEditedAt":null,"editor":null}]);
        }
    }
    let recorder = Recorder::new(&NoPlan);
    let issue =
        board_forge::parse_github_item("cgwalters-forge/composefs-rs#31", None, "github.com")
            .unwrap();
    board_forge::plan::issue_comments(&recorder, &issue).unwrap();
    replay.requests.extend(
        Replay::from_json(&serde_json::from_str(&recorder.fixture(replay.now).unwrap()).unwrap())
            .unwrap()
            .requests,
    );
    let dir = std::env::temp_dir().join(format!(
        "agent-board-promote-scenario-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("scenario.json");
    std::fs::write(&path, serde_json::to_vec(&serde_json::json!({"schema":"agent-board/pass-replay/v1","now":replay.now.format(&time::format_description::well_known::Rfc3339).unwrap(),"requests":replay.requests})).unwrap()).unwrap();
    let output = command()
        .args([
            "promote",
            "--config",
            "fixtures/promote/config.json",
            "--replay",
        ])
        .arg(&path)
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .assert()
        .success()
        .get_output()
        .clone();
    assert!(String::from_utf8(output.stdout).unwrap().contains("would promote cgwalters-forge/composefs-rs#34 -> composefs/composefs-rs (sign-off on 1 of 1 commits) (policy bot-ok); preview only; publication revalidates approval and all gates"));
    // Apply cannot reuse credential-free replay as write authority.
    command()
        .args([
            "promote",
            "--config",
            "fixtures/promote/config.json",
            "--apply",
            "--max-writes",
            "3",
            "--replay",
        ])
        .arg(&path)
        .assert()
        .failure();
    command()
        .args([
            "promote",
            "--config",
            "fixtures/promote/config.json",
            "--apply",
        ])
        .assert()
        .failure();
    // Hypothetical human-text scenarios, not recorded operator authorization.
    use base64::{Engine, engine::general_purpose::STANDARD};
    for e in &mut replay.requests {
        let url = e["request"]["get"].as_str().unwrap_or_default().to_owned();
        if url.contains("/contents/upstream-policy/") {
            let file = &mut e["answer"]["body"];
            let text = String::from_utf8(
                STANDARD
                    .decode(file["content"].as_str().unwrap().replace('\n', ""))
                    .unwrap(),
            )
            .unwrap()
            .replace("verdict: bot-ok", "verdict: human-text");
            file["size"] = text.len().into();
            file["content"] = STANDARD.encode(text).into();
        } else if url.contains("/activity?") {
            e["answer"]["body"][0]["actor"] = serde_json::json!({"login":"cgwalters"});
        } else if url.contains("/issues/34/comments?") {
            e["answer"]["body"][0]["body"] = "/promote --human-text".into();
        }
    }
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../fixtures/promote/provenance-public-2026-11-04.json"
    ))
    .unwrap();
    let mut answer = fixture["answer"].clone();
    let p = &mut answer["repository"]["pullRequest"];
    p["number"] = 34.into();
    p["url"] = "https://github.com/cgwalters-forge/composefs-rs/pull/34".into();
    p["headRefOid"] = "866b6ce9050782c50eb0fec1c08b37cebec93dc6".into();
    p["title"] = "Implement slice".into();
    p["body"] = "Part of cgwalters-forge/composefs-rs#31".into();
    p["editor"] = serde_json::json!({"login":"cgwalters"});
    p["lastEditedAt"] = "2026-10-22T12:00:00Z".into();
    p["userContentEdits"] = serde_json::json!({"totalCount":1,"nodes":[{"editedAt":"2026-10-22T12:00:00Z","deletedAt":null,"editor":{"login":"cgwalters"}}],"pageInfo":{"hasNextPage":false,"endCursor":null}});
    p["timelineItems"] = serde_json::json!({"totalCount":14,"nodes":[{"id":"rename-1","createdAt":"2026-10-22T12:00:00Z","previousTitle":"Bot title","currentTitle":"Implement slice","actor":{"login":"cgwalters"}}],"pageInfo":{"hasNextPage":false,"endCursor":null}});
    struct Evidence(Value);
    impl Transport for Evidence {
        fn graphql(&self, _: &str, _: Value) -> anyhow::Result<Value> {
            Ok(self.0.clone())
        }
    }
    let evidence = Evidence(answer);
    let recorder = Recorder::new(&evidence);
    recorder
        .graphql(
            fixture["query"].as_str().unwrap(),
            serde_json::json!({"owner":"cgwalters-forge","name":"composefs-rs","number":34}),
        )
        .unwrap();
    replay.requests.extend(
        Replay::from_json(&serde_json::from_str(&recorder.fixture(replay.now).unwrap()).unwrap())
            .unwrap()
            .requests,
    );
    for hold in [false, true] {
        if hold {
            let e = replay
                .requests
                .iter_mut()
                .find(|e| {
                    e["answer"]["data"]["repository"]["pullRequest"]["userContentEdits"].is_object()
                })
                .unwrap();
            e["answer"]["data"]["repository"]["pullRequest"]["editor"] = Value::Null;
        }
        std::fs::write(&path, serde_json::to_vec(&serde_json::json!({"schema":"agent-board/pass-replay/v1","now":replay.now.format(&time::format_description::well_known::Rfc3339).unwrap(),"requests":replay.requests})).unwrap()).unwrap();
        let output = command()
            .args([
                "promote",
                "--config",
                "fixtures/promote/config.json",
                "--replay",
            ])
            .arg(&path)
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env_remove("DISPATCH_TOKEN")
            .assert()
            .success()
            .get_output()
            .clone();
        let expected = if hold {
            "hold promote cgwalters-forge/composefs-rs#34 head=866b6ce9050782c50eb0fec1c08b37cebec93dc6: needs-text: last body editor is not the operator or is missing".to_owned()
        } else {
            "would promote cgwalters-forge/composefs-rs#34 -> composefs/composefs-rs (sign-off on 1 of 1 commits) (policy human-text; provenance verified); preview only; publication revalidates approval and all gates; stack=none (no plan); dco=required; head=866b6ce9050782c50eb0fec1c08b37cebec93dc6 command=https://github.com/cgwalters-forge/composefs-rs/pull/34#issuecomment-1 at=2026-10-23T00:00:00Z human-text=true push=2026-10-22T03:25:48Z; staging verification required for bot-fork head".to_owned()
        };
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{expected}\n{}\n", board_forge::promote::REMAINING)
        );
        assert!(output.stderr.is_empty());
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn promote_offline_preview_and_cli_refusals() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let config = "fixtures/promote/config.json";
    let fixture = "fixtures/promote/public-2026-11-04.json";
    let output = command()
        .args(["promote", "--config", config, "--replay", fixture])
        .current_dir(root)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("DISPATCH_TOKEN")
        .assert()
        .success()
        .get_output()
        .clone();
    let recorded: Value = serde_json::from_str(include_str!(
        "../../../fixtures/promote/public-2026-11-04.json"
    ))
    .unwrap();
    let mut pulls = recorded["requests"][1]["answer"]["body"]
        .as_array()
        .unwrap()
        .clone();
    pulls.sort_by_key(|p| p["number"].as_u64());
    let mut expected = String::new();
    for pull in pulls {
        expected.push_str(&format!(
            "hold promote cgwalters-forge/composefs-rs#{} head={} command=unverified: draft\n",
            pull["number"],
            pull["head"]["sha"].as_str().unwrap()
        ));
    }
    expected.push_str(board_forge::promote::REMAINING);
    expected.push('\n');
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    assert!(output.stderr.is_empty());
    for (extra, code, reason) in [
        (vec!["--apply"], 1, "--max-writes"),
        (
            vec!["--apply", "--max-writes", "3"],
            1,
            "promote.publish must be true",
        ),
        (
            vec!["--record", "unused.json", "--replay", fixture],
            1,
            "cannot be used with",
        ),
        (vec!["--owner", "example"], 1, "--config only"),
        (
            vec!["--replay", "fixtures/board.json"],
            1,
            "not a pass replay",
        ),
        (vec![], 4, "GH_TOKEN"),
    ] {
        let output = command()
            .args(["promote", "--config", config])
            .args(&extra)
            .current_dir(root)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .assert()
            .code(code)
            .get_output()
            .clone();
        assert!(
            String::from_utf8(output.stderr).unwrap().contains(reason),
            "{extra:?}"
        );
        assert!(output.stdout.is_empty());
    }
    command()
        .args(["promote", "--config", "fixtures/pass/config.json"])
        .current_dir(root)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .assert()
        .success()
        .stdout("promote: off\n");
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

#[test]
fn changes_replay_lists_the_recorded_diff_without_credentials() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let state = std::env::temp_dir().join(format!("agent-board-changes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&state).unwrap();
    // The recording's watermarks: every repository at 14:05 UTC.
    let replay = |fixture: &str| {
        let mut command = command();
        command
            .args([
                "changes",
                "--config",
                "fixtures/changes/config.json",
                "--replay",
            ])
            .arg(fixture)
            .arg("--state")
            .arg(&state)
            .current_dir(root)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env("PATH", "");
        command
    };
    let seed = include_str!("../../../fixtures/changes/state.json");
    std::fs::write(state.join("changes.json"), seed).unwrap();
    for args in [
        vec!["--json", "items"],
        vec!["--json", "--jq", "["],
        vec!["--json", "--jq", "unknown_function"],
        vec!["--json", "--jq", ".repositories | error(\"render failed\")"],
    ] {
        replay("fixtures/changes/agentic-job-2026-11-11.json")
            .arg("--commit")
            .args(args)
            .assert()
            .failure();
        assert_eq!(
            std::fs::read_to_string(state.join("changes.json")).unwrap(),
            seed
        );
    }
    let output = replay("fixtures/changes/agentic-job-2026-11-11.json")
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .clone();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let items = value["repositories"][0]["items"].as_array().unwrap();
    for number in [3402, 3411] {
        let reference = format!("cgwalters-forge/agentic-job#{number}");
        let item = items.iter().find(|i| i["ref"] == reference).unwrap();
        assert!(
            item["changes"]
                .as_array()
                .unwrap()
                .contains(&Value::from("closed"))
        );
    }
    let output = replay("fixtures/changes/agentic-job-2026-11-11.json")
        .arg("--commit")
        .assert()
        .success()
        .get_output()
        .clone();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.starts_with("changes: 43 items in 3 of 24 repositories; 28 requests, 0 revalidated\n"),
        "{text}"
    );
    assert_eq!(text.lines().count(), 4, "{text}");
    // After the commit, the second recording lists nothing; --json gives
    // the diff, selectable by field.
    let output = replay("fixtures/changes/agentic-job-2026-11-11-again.json")
        .args(["--json", "schema,repositories"])
        .assert()
        .success()
        .get_output()
        .clone();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema"], "agent-board/changes/v1");
    assert_eq!(value["repositories"].as_array().unwrap().len(), 24);
    assert!(
        value["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["items"].as_array().unwrap().is_empty())
    );
    for (args, error) in [
        (
            vec!["--since", "2026-11-11T00:00:00Z", "--commit"],
            "cannot be used with",
        ),
        (
            vec!["--owner", "example"],
            "changes reads its repositories from --config",
        ),
        (vec!["--json", "items"], "unknown JSON field: items"),
    ] {
        let output = replay("fixtures/changes/agentic-job-2026-11-11-again.json")
            .args(&args)
            .assert()
            .failure()
            .get_output()
            .clone();
        let err = String::from_utf8(output.stderr).unwrap();
        assert!(err.contains(error), "{args:?}: {err}");
    }
    // A fixture's watermarks never land in the cache.
    let output = command()
        .args([
            "changes",
            "--config",
            "fixtures/changes/config.json",
            "--replay",
            "fixtures/changes/agentic-job-2026-11-11-again.json",
        ])
        .current_dir(root)
        .assert()
        .failure()
        .get_output()
        .clone();
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("--state <DIR>")
    );
    std::fs::remove_dir_all(&state).unwrap();
}
