//! CLI isolation: a Forgejo preview has no GitHub client or credential fallback.
use assert_cmd::Command;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

const TOKEN_VAR: &str = "AGENT_BOARD_TEST_PASS_FORGEJO";

fn command(path: &std::path::Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("agent-board"));
    command
        .args(["pass", "--config"])
        .arg(path)
        .env_remove(TOKEN_VAR)
        .env_remove("GITHUB_ACTIONS")
        .env_remove("FORGEJO_ACTIONS")
        .env("GH_TOKEN", "not-a-forgejo-credential")
        .env("GITHUB_TOKEN", "not-a-forgejo-credential")
        .env("DISPATCH_TOKEN", "not-a-forgejo-credential");
    command
}

fn config(url: &str) -> Value {
    json!({"operator": "operator", "bot": "bot", "dispatchRepo": "example/control",
        "dispatchWorkflow": "agent.yml", "board": {"forge": "forgejo", "url": url,
        "tokenEnv": TOKEN_VAR, "repositories": ["example/work"], "membership": "open", "recorders": [1, 2]}})
}

fn directory(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agent-board-forgejo-pass-{name}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn refusals_precede_credentials_and_auxiliary_files() {
    let dir = directory("refused");
    let path = dir.join("config.json");
    let base = config("http://127.0.0.1:1/forge/");
    std::fs::write(&path, base.to_string()).unwrap();
    for (flags, refused) in [
        (vec!["--fix-bounds", "does-not-exist"], "--fix-bounds"),
        (
            vec![
                "--record",
                "does-not-exist/record.json",
                "--replay",
                "does-not-exist",
            ],
            "--record with --replay",
        ),
    ] {
        let output = command(&path)
            .args(&flags)
            .assert()
            .failure()
            .get_output()
            .clone();
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(
            error.contains(&format!("unsupported_capability: {refused}:")),
            "{error}"
        );
        assert!(!error.contains("authentication required"), "{error}");
    }
    let output = command(&path).assert().failure().get_output().clone();
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains(TOKEN_VAR), "{error}");
    assert!(!error.contains("GH_TOKEN"), "{error}");
    for option in ["follow", "bodyPlans"] {
        let mut value = base.clone();
        value[option] = false.into();
        std::fs::write(&path, value.to_string()).unwrap();
        let output = command(&path).assert().failure().get_output().clone();
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(
            error.contains(&format!("unsupported_capability: {option}:")),
            "{error}"
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn apply_requires_local_caps_or_matching_actions_instance_before_credentials() {
    let dir = directory("apply-admission");
    let path = dir.join("config.json");
    std::fs::write(&path, config("http://127.0.0.1:1/forge/").to_string()).unwrap();
    for flags in [
        vec![],
        vec!["--max-dispatch", "1"],
        vec!["--max-writes", "4"],
    ] {
        let output = command(&path)
            .arg("--apply")
            .args(flags)
            .assert()
            .failure()
            .get_output()
            .clone();
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(
            error.contains("needs both --max-dispatch and --max-writes"),
            "{error}"
        );
    }
    let output = command(&path)
        .args(["--apply", "--max-dispatch", "1", "--max-writes", "4"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains(TOKEN_VAR)
    );
    for prefix in ["FORGEJO", "GITHUB"] {
        for server in [
            "http://foreign.example/forge",
            "http://127.0.0.1:1/other",
            "http://127.0.0.1:1/forge",
        ] {
            let output = command(&path)
                .arg("--apply")
                .env(format!("{prefix}_ACTIONS"), "true")
                .env(format!("{prefix}_SERVER_URL"), server)
                .env(format!("{prefix}_REPOSITORY"), "team.example/control.repo")
                .env(format!("{prefix}_RUN_ID"), "42")
                .assert()
                .failure()
                .get_output()
                .clone();
            let error = String::from_utf8(output.stderr).unwrap();
            assert!(
                error.contains(
                    if server.ends_with("/forge") && !server.contains("foreign") {
                        TOKEN_VAR
                    } else {
                        "server URL differs"
                    }
                ),
                "{error}"
            );
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn invalid_names_precede_token_lookup() {
    let dir = directory("names");
    let path = dir.join("config.json");
    for name in [
        "..",
        "../visitor",
        "a/b",
        "a\\b",
        "a?b",
        "a#b",
        "a\n",
        "%2e%2e",
        "//evil.example",
        "https://evil.example",
    ] {
        for field in ["operator", "bot", "dispatchRepo", "workRepositories"] {
            let mut value = config("http://127.0.0.1:1/forge/");
            value[field] = match field {
                "dispatchRepo" => json!(format!("{name}/repo")),
                "workRepositories" => json!([format!("{name}/repo")]),
                _ => json!(name),
            };
            std::fs::write(&path, value.to_string()).unwrap();
            let output = command(&path).assert().failure().get_output().clone();
            let error = String::from_utf8(output.stderr).unwrap();
            assert!(error.contains("invalid"), "{field}={name:?}: {error}");
            assert!(!error.contains("authentication required"), "{error}");
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn only_configured_instance_subpath_and_credential_free_broker() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        for (path, body, authenticated) in [
            (
                "/forge/api/v1/user",
                json!({"login": "board.bot", "id": 1, "html_url": "https://github.com/visitor"}),
                true,
            ),
            (
                "/forge/api/v1/users/alice.smith",
                json!({"login": "alice.smith", "id": 2}),
                true,
            ),
            (
                "/broker/usage",
                json!({"schema": "praxis-broker-usage/v1"}),
                false,
            ),
        ] {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing {path}");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 8192);
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            assert!(
                request.starts_with(&format!("get {path} http/1.1\r\n")),
                "{request}"
            );
            assert_eq!(
                request.contains("authorization: token fixture-only"),
                authenticated,
                "{request}"
            );
            if !authenticated {
                assert!(!request.contains("authorization:"), "{request}");
            }
            assert!(!request.contains("not-a-forgejo-credential"));
            let body = body.to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    });
    let dir = directory("isolation");
    let path = dir.join("config.json");
    let mut value = config(&format!("{origin}/forge"));
    value["operator"] = "alice.smith".into();
    value["bot"] = "board.bot".into();
    value["dispatchRepo"] = "team.example/control.repo".into();
    value["workRepositories"] = json!(["team.example/work.repo"]);
    value["usage"] = json!({"url": format!("{origin}/broker/usage")});
    value["pools"] = json!({"claude": {}});
    std::fs::write(&path, value.to_string()).unwrap();
    let recording = dir.join("recording.json");
    let output = command(&path)
        .env(TOKEN_VAR, "fixture-only")
        .arg("--record")
        .arg(&recording)
        .assert()
        .success()
        .get_output()
        .clone();
    let output = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.contains("identity: bot board.bot (id 1), operator alice.smith (id 2)"),
        "{output}"
    );
    assert!(
        output.contains("usage: claude: no reading (held)"),
        "{output}"
    );
    assert_eq!(
        output
            .lines()
            .filter(|l| l.contains("not built on Forgejo yet"))
            .count(),
        4
    );
    assert!(
        output.contains("schedule: off (wip 0 or no workRepositories)"),
        "{output}"
    );
    assert!(output.contains("dispatched 0, writes 0"), "{output}");
    server.join().unwrap();
    let fixture = std::fs::read_to_string(&recording).unwrap();
    assert!(!fixture.contains("fixture-only"), "{fixture}");
    assert!(
        !fixture.to_ascii_lowercase().contains("authorization"),
        "{fixture}"
    );
    // The server is gone and no token is set: the replay reads only the
    // fixture, and decides the same.
    let replayed = command(&path)
        .arg("--replay")
        .arg(&recording)
        .assert()
        .success()
        .get_output()
        .clone();
    assert_eq!(String::from_utf8(replayed.stdout).unwrap(), output);
    // Another instance's board, or a GitHub fixture or board, is refused.
    let other = dir.join("other.json");
    value["board"]["url"] = "https://forge.example/forge".into();
    std::fs::write(&other, value.to_string()).unwrap();
    let github = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/pass");
    for (config, fixture, refused) in [
        (other.clone(), recording.clone(), "recorded on"),
        (
            path.clone(),
            format!("{github}/board-2026-10-23.json").into(),
            "not a Forgejo pass replay",
        ),
        (
            format!("{github}/config.json").into(),
            recording.clone(),
            "a \"forgejo\" pass replay",
        ),
    ] {
        let output = command(&config)
            .arg("--replay")
            .arg(&fixture)
            .assert()
            .failure()
            .get_output()
            .clone();
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains(refused), "{error}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}
