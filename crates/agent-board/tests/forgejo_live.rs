//! The Forgejo pass against a real, disposable Forgejo (#515, docs/forges.md
//! "Live CI"), where the other tests have fakes: `agent-board pass` preview
//! and capped `--apply`, the board writes, the dispatch adapter and leases.
//!
//! Needs a fresh Forgejo at `AGENT_BOARD_TEST_FORGEJO` (`http://127.0.0.1:3000`)
//! in the container `AGENT_BOARD_TEST_FORGEJO_CONTAINER`, where `docker exec`
//! (or `$CONTAINER exec`) makes its users and tokens; the test starts a runner
//! container on the host network beside it. A second run needs a new
//! container. CI's `forgejo` job starts one and runs:
//!
//! ```text
//! cargo test --locked -p agent-board --test forgejo_live -- --ignored --nocapture
//! ```
//!
//! Nothing here is a secret: every user and token is made at run time on a
//! server that ends with the job. It proves nothing about agents.
use base64::{Engine, engine::general_purpose::STANDARD};
use board_core::{AgentPolicy, DEFAULT_AGENT_POLICY, Item, ItemRef};
use board_forge::dispatch::{Forgejo as Actions, ForgejoLease, Target};
use board_forge::forgejo::{Client, Config, Forgejo};
use board_forge::plan::{DISPATCH_MARKER, JOB_PLAN_SCHEMA, PLAN_FENCE};
use board_forge::{BoardOperation, BoardWrite, Forge};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const SERVER_VAR: &str = "AGENT_BOARD_TEST_FORGEJO";
const CONTAINER_VAR: &str = "AGENT_BOARD_TEST_FORGEJO_CONTAINER";
/// The bot's token, for the CLI and [`Client`]; set by this test alone.
const TOKEN_VAR: &str = "AGENT_BOARD_TEST_FORGEJO_TOKEN";
/// Forgejo Runner v13.2.1, the newest release on 31 October 2026.
const RUNNER_IMAGE: &str = "data.forgejo.org/forgejo/runner@sha256:568cea8c15ddc01fc8f330124892f572af7db37fa28344a2c1914405793bf61d";
/// How long a starting server has to answer, and a run to change status.
const STARTUP: Duration = Duration::from_secs(120);
const RUN_CHANGE: Duration = Duration::from_secs(180);

/// `admin` is a name Forgejo reserves. The admin is also the operator.
const ADMIN: &str = "forge-admin";
const BOT: &str = "bot";
/// docs/forges.md's token scopes, and no admin, organization write or `all`
/// one. Label resolution's `GET /orgs/{owner}` needs `read:organization`.
const SCOPES: &str =
    "read:user,read:issue,write:issue,read:repository,write:repository,read:organization";
const WORK: &str = "forge-admin/work";
const CALLER: &str = "forge-admin/control";
const PUBLICATION: &str = "forge-admin/publication";
const WORKFLOW: &str = "agent.yml";
const UNTRIGGERED: &str = "untriggered.yml";
const LEASE_BRANCH: &str = "agent-board-leases";
/// The pass's WIP, and so its lease slots; slot [`WIP`] is the race's only.
const WIP: u64 = 3;
const LABELS: &[&str] = &[
    "priority/P0",
    "priority/P1",
    "priority/P2",
    "priority/P3",
    "turn/coordinator",
    "turn/worker",
    "status/todo",
    "status/in-progress",
];

/// The inert caller: docs/forges.md's six inputs and `run-name`, and one job
/// that prints them. It waits a little, so `running` can be seen, and fails
/// when its task is `fail`, so a rerun can be.
const CALLER_WORKFLOW: &str = r#"name: agent
run-name: ${{ inputs.kind }} ${{ inputs.repo }}#${{ inputs.item }} (${{ inputs.pool }})
on:
  workflow_dispatch:
    inputs:
      task: {type: string, required: true}
      repo: {type: string, required: true}
      item: {type: string, required: true}
      kind: {type: string, required: true}
      pool: {type: string, required: false}
      base: {type: string, required: false}
jobs:
  inert:
    runs-on: inert
    steps:
      - shell: sh
        env:
          TASK: ${{ inputs.task }}
          REPO: ${{ inputs.repo }}
          ITEM: ${{ inputs.item }}
          KIND: ${{ inputs.kind }}
          POOL: ${{ inputs.pool }}
          BASE: ${{ inputs.base }}
        run: |
          echo "task=$TASK repo=$REPO item=$ITEM kind=$KIND pool=$POOL base=$BASE"
          sleep 10
          test "$TASK" != fail
"#;

/// A workflow without `workflow_dispatch`, which Forgejo still starts.
const UNTRIGGERED_WORKFLOW: &str = r#"name: untriggered
on:
  release:
    types: [published]
jobs:
  inert:
    runs-on: inert
    steps:
      - shell: sh
        run: echo untriggered
"#;

/// The API as the admin or the bot, without the pass's checks.
struct Api {
    server: String,
    http: ureq::Agent,
}

impl Api {
    fn call(&self, token: &str, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let url = format!("{}/api/v1{path}", self.server);
        let authorization = format!("token {token}");
        let response = match (method, body) {
            ("GET", None) => self
                .http
                .get(&url)
                .header("authorization", &authorization)
                .call(),
            ("POST", Some(body)) => self
                .http
                .post(&url)
                .header("authorization", &authorization)
                .send_json(body),
            ("PUT", Some(body)) => self
                .http
                .put(&url)
                .header("authorization", &authorization)
                .send_json(body),
            _ => panic!("{method} {path} with {body:?}"),
        };
        let mut response = response.unwrap_or_else(|err| panic!("{method} {path}: {err}"));
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    fn get(&self, token: &str, path: &str) -> Value {
        let (status, answer) = self.call(token, "GET", path, None);
        assert_eq!(status, 200, "GET {path}: {answer}");
        answer
    }

    fn send(&self, token: &str, method: &str, path: &str, body: Value) -> Value {
        let (status, answer) = self.call(token, method, path, Some(&body));
        assert!(
            (200..300).contains(&status),
            "{method} {path}: {status} {answer}"
        );
        answer
    }

    fn file(&self, token: &str, repository: &str, branch: &str, path: &str, content: &str) {
        self.send(
            token,
            "POST",
            &format!("/repos/{repository}/contents/{path}"),
            json!({"branch": branch, "content": STANDARD.encode(content), "message": path}),
        );
    }

    fn comments(&self, token: &str, number: u64) -> Vec<Value> {
        let comments = self.get(token, &format!("/repos/{WORK}/issues/{number}/comments"));
        comments.as_array().unwrap().clone()
    }

    fn runs(&self, token: &str, workflow: &str) -> Vec<Value> {
        let path = format!("/repos/{CALLER}/actions/runs?workflow_id={workflow}&limit=50");
        let runs = self.get(token, &path);
        runs["workflow_runs"].as_array().unwrap().clone()
    }
}

/// Runs `command` to success and returns its stdout, trimmed.
fn run(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// `docker`, or `$CONTAINER` (podman) as the Justfile's lint recipe has it.
fn engine() -> Command {
    Command::new(std::env::var("CONTAINER").unwrap_or_else(|_| "docker".into()))
}

/// The server's own CLI, as its user, not root: the API makes no first admin.
fn forgejo_cli(container: &str, args: &[&str]) -> String {
    run(engine()
        .args(["exec", "--user", "git", container, "forgejo"])
        .args(args))
}

/// A user made by the server's CLI with a password nobody keeps, and its
/// token with `scopes`.
fn user(container: &str, login: &str, admin: bool, scopes: &str) -> String {
    let email = format!("{login}@example.org");
    let mut args = vec!["admin", "user", "create", "--username", login];
    args.extend(["--email", &email, "--random-password"]);
    args.push("--must-change-password=false");
    if admin {
        args.push("--admin");
    }
    forgejo_cli(container, &args);
    forgejo_cli(
        container,
        &[
            "admin",
            "user",
            "generate-access-token",
            "--username",
            login,
            "--token-name",
            "live-ci",
            "--scopes",
            scopes,
            "--raw",
        ],
    )
}

/// A Forgejo runner for the caller only, removed when dropped.
struct Runner(String);

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = engine().args(["rm", "-f", &self.0]).output();
    }
}

fn start_runner(api: &Api, admin: &str, dir: &Path) -> Runner {
    let registered = api.send(
        admin,
        "POST",
        &format!("/repos/{CALLER}/actions/runners"),
        json!({"name": "live-ci"}),
    );
    // `host` runs the job in the runner's own container, with no Docker.
    let config = format!(
        "runner:\n  capacity: 1\n  fetch_interval: 1s\n  labels: [\"inert:host\"]\ncache:\n  enabled: false\nserver:\n  connections:\n    ci:\n      url: {}/\n      uuid: {}\n      token: {}\n",
        api.server,
        registered["uuid"].as_str().unwrap(),
        registered["token"].as_str().unwrap(),
    );
    let path = dir.join("runner.yml");
    std::fs::write(&path, config).unwrap();
    let id = run(engine()
        .args(["run", "-d", "--network", "host", "-v"])
        .arg(format!("{}:/etc/runner.yml:ro", path.display()))
        .args([
            RUNNER_IMAGE,
            "forgejo-runner",
            "daemon",
            "-c",
            "/etc/runner.yml",
        ]));
    Runner(id)
}

/// A credential-free broker whose reading leaves the `claude` pool room.
fn broker() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/usage", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // The request's headers, to their blank line; the body is ignored.
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 2 {
                line.clear();
            }
            drop(reader);
            let now = OffsetDateTime::now_utc().unix_timestamp();
            let window = |reset: i64| json!({"utilization": 0.01, "reset": now + reset, "status": "allowed", "observed_at": now - 10});
            let body = json!({"schema": "praxis-broker-usage/v1", "started_at": now - 3600,
                "anthropic": {"unified_5h": window(4 * 3600), "unified_7d": window(6 * 86400)}})
            .to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    url
}

/// `agent-board pass` on `config` with `flags`: its success and stdout.
fn pass(config: &Path, token: &str, flags: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-board"))
        .args(["pass", "--config"])
        .arg(config)
        .args(flags)
        .env(TOKEN_VAR, token)
        // Local mode, even inside CI's GitHub Actions job.
        .env_remove("GITHUB_ACTIONS")
        .env_remove("FORGEJO_ACTIONS")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    eprintln!(
        "pass {flags:?}: {}\n{stdout}{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), stdout)
}

fn apply(config: &Path, token: &str) -> String {
    let (ok, stdout) = pass(
        config,
        token,
        &["--apply", "--max-dispatch", "1", "--max-writes", "12"],
    );
    assert!(ok, "{stdout}");
    stdout
}

/// Polls `check` until it answers, for at most `limit`.
fn wait<T>(what: &str, limit: Duration, mut check: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(found) = check() {
            return found;
        }
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// The JSON of the newest receipt on `comments`.
fn receipt(comments: &[Value]) -> Value {
    let body = comments
        .iter()
        .rev()
        .filter_map(|c| c["body"].as_str())
        .find(|b| b.starts_with("<!-- agent-board-receipt:"))
        .unwrap();
    serde_json::from_str(body.split_once('\n').unwrap().1).unwrap()
}

/// An issue of the work repository, opened by the operator, with a job plan
/// for `brief` where there is one.
fn issue(api: &Api, admin: &str, title: &str, brief: Option<&str>) -> u64 {
    let created = api.send(
        admin,
        "POST",
        &format!("/repos/{WORK}/issues"),
        json!({"title": title, "body": "Live CI."}),
    );
    let number = created["number"].as_u64().unwrap();
    if let Some(brief) = brief {
        let plan = json!({"schema": JOB_PLAN_SCHEMA, "repo": WORK, "item": number,
            "kind": "implement", "runner": "hosted", "brief": brief, "pool": "claude"});
        api.send(
            admin,
            "POST",
            &format!("/repos/{WORK}/issues/{number}/comments"),
            json!({"body": format!("```{PLAN_FENCE}\n{plan:#}\n```")}),
        );
    }
    number
}

fn status(api: &Api, token: &str, run: u64) -> String {
    let run = api.get(token, &format!("/repos/{CALLER}/actions/runs/{run}"));
    run["status"].as_str().unwrap().to_owned()
}

#[test]
#[ignore = "needs a disposable Forgejo; CI's forgejo job runs it"]
fn pass_against_live_forgejo() {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
    let server = var(SERVER_VAR).trim_end_matches('/').to_owned();
    let container = var(CONTAINER_VAR);
    let http = ureq::Agent::config_builder()
        .proxy(None)
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let api = Api { server, http };
    wait("the server's health check", STARTUP, || {
        let health = api.http.get(format!("{}/api/healthz", api.server)).call();
        health.is_ok_and(|r| r.status().is_success()).then_some(())
    });
    let dir = std::env::temp_dir().join(format!("agent-board-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // Provisioning, by the admin: the bot, the repositories, labels,
    // workflows, the lease slots.
    let admin = user(&container, ADMIN, true, "all");
    let bot = user(&container, BOT, false, SCOPES);
    let admin_id = api.get(&admin, "/user")["id"].as_u64().unwrap();
    let bot_id = api.get(&bot, "/user")["id"].as_u64().unwrap();
    let (status_code, _) = api.call(&bot, "GET", "/admin/users", None);
    assert_eq!(status_code, 403, "the bot's token reaches the admin API");
    for (name, private) in [("work", false), ("control", true), ("publication", true)] {
        api.send(
            &admin,
            "POST",
            "/user/repos",
            json!({"name": name, "private": private, "auto_init": true, "default_branch": "main"}),
        );
        api.send(
            &admin,
            "PUT",
            &format!("/repos/{ADMIN}/{name}/collaborators/{BOT}"),
            json!({"permission": "write"}),
        );
    }
    assert_eq!(
        api.get(&bot, &format!("/repos/{PUBLICATION}"))["private"],
        true
    );
    for label in LABELS {
        api.send(
            &admin,
            "POST",
            &format!("/repos/{WORK}/labels"),
            json!({"name": label, "color": "#5f6b7a", "exclusive": true}),
        );
    }
    for (name, content) in [
        (WORKFLOW, CALLER_WORKFLOW),
        (UNTRIGGERED, UNTRIGGERED_WORKFLOW),
    ] {
        let path = format!(".forgejo/workflows/{name}");
        api.file(&admin, CALLER, "main", &path, content);
    }
    api.send(
        &admin,
        "POST",
        &format!("/repos/{CALLER}/branches"),
        json!({"new_branch_name": LEASE_BRANCH, "old_branch_name": "main"}),
    );
    for slot in 0..=WIP {
        let path = format!("leases/{slot}.json");
        api.file(&admin, CALLER, LEASE_BRANCH, &path, r#"{"free":"initial"}"#);
    }

    // The library, as the bot, with the pass's own transport.
    let board: Config = serde_json::from_value(json!({"url": format!("{}/", api.server),
        "tokenEnv": TOKEN_VAR, "repositories": [WORK], "membership": "open",
        "recorders": [admin_id, bot_id]}))
    .unwrap();
    // SAFETY: this is the binary's only test, and it has started no thread
    // yet; libtest's own threads do not touch the environment meanwhile.
    unsafe { std::env::set_var(TOKEN_VAR, &bot) };
    let forge = Forgejo {
        transport: Client::from_env(&board).unwrap(),
        config: board.clone(),
    };
    let target = |workflow: &str| Target {
        caller: CALLER.into(),
        workflow: workflow.into(),
        git_ref: "main".into(),
    };
    let caller = target(WORKFLOW);
    let inputs = ["task", "repo", "item", "kind", "pool"].map(String::from);
    let actions = Actions::new(&forge.transport, &board, &caller, inputs.into(), bot_id).unwrap();

    // Label, comment, claim, receipt and state round trips on issue 1.
    let first = issue(&api, &admin, "Round trips", Some("Round trips."));
    let at = ItemRef {
        host: "127.0.0.1".into(),
        repository: WORK.into(),
        number: first,
    };
    let labels = || -> Vec<String> {
        let issue = api.get(&admin, &format!("/repos/{WORK}/issues/{first}"));
        let labels = issue["labels"].as_array().unwrap().iter();
        labels
            .map(|l| l["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let write = |operation: BoardOperation| {
        let observed = Item {
            identity: at.clone(),
            project_item: None,
            assignment: None,
            title: String::new(),
            state: "open".into(),
            state_reason: None,
            status: None,
            priority: None,
            turn: None,
            fields_complete: false,
            field_errors: vec![],
            evidence: vec![],
            content_kind: "issue".into(),
            labels: labels(),
            assignees: vec![],
            author: None,
            timestamps: BTreeMap::new(),
            fields: BTreeMap::new(),
        };
        let policy = AgentPolicy::parse(DEFAULT_AGENT_POLICY).unwrap();
        let write = BoardWrite {
            observed,
            policy,
            operation,
        };
        forge.board_write(&write, true, &mut 1).unwrap();
    };
    for (scope, name) in [
        ("priority/", "priority/P2"),
        ("turn/", "turn/worker"),
        ("priority/", "priority/P1"),
    ] {
        let (scope, name) = (scope.into(), name.into());
        write(BoardOperation::Label { scope, name });
    }
    // The exclusive scope replaced P2, and kept the other scope's label.
    assert_eq!(labels(), ["priority/P1", "turn/worker"]);
    let body = "A record, appended.".to_owned();
    write(BoardOperation::Comment { body: body.clone() });
    let plan = api.comments(&admin, first)[0]["id"].as_u64().unwrap();
    let marker = DISPATCH_MARKER.to_owned();
    write(BoardOperation::Claim {
        marker: marker.clone(),
        plan,
    });
    write(BoardOperation::Receipt {
        marker: marker.clone(),
        plan,
        body: r#"{"event":"held"}"#.into(),
    });
    let claim = forge.plan_claim(&at, &marker, plan).unwrap().unwrap();
    assert_eq!(
        (claim.recorder, claim.receipts),
        (bot_id, vec![r#"{"event":"held"}"#.to_owned()])
    );
    let comments = api.comments(&admin, first);
    assert!(
        comments
            .iter()
            .any(|c| c["body"] == body.as_str() && c["user"]["id"] == bot_id)
    );
    write(BoardOperation::State {
        state: "closed".into(),
    });
    let closed = api.get(&admin, &format!("/repos/{WORK}/issues/{first}"));
    assert_eq!(closed["state"], "closed");
    // docs/forges.md asks whether a fresh comment's two times can differ.
    for n in 0..20 {
        api.send(
            &bot,
            "POST",
            &format!("/repos/{WORK}/issues/{first}/comments"),
            json!({"body": format!("Sample {n}.")}),
        );
    }
    let comments = api.comments(&admin, first);
    let differ = comments
        .iter()
        .filter(|c| c["created_at"] != c["updated_at"])
        .count();
    eprintln!(
        "comment times: {differ} of {} fresh comments have created_at != updated_at",
        comments.len()
    );

    // The pass's configuration, and one whose caller has no dispatch trigger.
    let mut file = json!({"operator": ADMIN, "bot": BOT, "dispatchRepo": CALLER,
        "dispatchWorkflow": UNTRIGGERED, "board": {"forge": "forgejo",
        "url": format!("{}/", api.server), "tokenEnv": TOKEN_VAR, "repositories": [WORK],
        "membership": "open", "recorders": [admin_id, bot_id]}, "wip": WIP,
        "workRepositories": [WORK], "pools": {"claude": {}}, "usage": {"url": broker()},
        "leases": true});
    let untriggered = dir.join("untriggered.json");
    std::fs::write(&untriggered, file.to_string()).unwrap();
    file["dispatchWorkflow"] = WORKFLOW.into();
    file["reruns"] = true.into();
    let config = dir.join("config.json");
    std::fs::write(&config, file.to_string()).unwrap();

    // A start on the untriggered workflow: Forgejo answers 201 and creates a
    // run without the inputs, which is recorded as ambiguous, cancelled and
    // never sent again.
    let second = issue(&api, &admin, "Untriggered", Some("Untriggered."));
    let stdout = apply(&untriggered, &bot);
    assert!(
        stdout.contains(&format!("start {WORK}#{second}: ambiguous:")),
        "{stdout}"
    );
    let held = receipt(&api.comments(&admin, second));
    assert_eq!(held["event"], "held");
    let runs = api.runs(&admin, UNTRIGGERED);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["status"], "cancelled");
    let stdout = apply(&untriggered, &bot);
    assert!(
        stdout.contains(&format!(
            "hold {WORK}#{second}: plan already receipted; never retry"
        )),
        "{stdout}"
    );
    assert!(stdout.contains("dispatched 0/1"), "{stdout}");
    assert_eq!(api.runs(&admin, UNTRIGGERED).len(), 1);

    // Preview, then a capped apply: a 201 start, confirmed by reading the
    // run back, and listed `waiting`, as no runner is registered yet.
    let third = issue(&api, &admin, "Inert work", Some("fail"));
    let (ok, stdout) = pass(&config, &bot, &[]);
    assert!(ok, "{stdout}");
    assert!(
        stdout.contains(&format!("would start {WORK}#{third} pool=claude")),
        "{stdout}"
    );
    assert!(api.runs(&admin, WORKFLOW).is_empty());
    let stdout = apply(&config, &bot);
    let started = receipt(&api.comments(&admin, third));
    assert_eq!(started["event"], "started", "{stdout}");
    let id = started["run"].as_u64().unwrap();
    let url = started["url"].as_str().unwrap();
    assert!(
        stdout.contains(&format!("start {WORK}#{third}: {url}")),
        "{stdout}"
    );
    let read = api.get(&admin, &format!("/repos/{CALLER}/actions/runs/{id}"));
    assert_eq!(read["trigger_user"]["id"], bot_id);
    assert_eq!(read["title"], format!("implement {WORK}#{third} (claude)"));
    assert_eq!(read["status"], "waiting");
    let listed = actions.active_runs().unwrap();
    assert!(
        listed
            .iter()
            .any(|r| r["id"] == id && r["status"] == "waiting")
    );
    let slot = actions.slot(LEASE_BRANCH, 1).unwrap();
    assert_eq!(slot.lease().map(|l| l.run), Some(id));
    // A second pass starts nothing: the run in flight holds the issue.
    let stdout = apply(&config, &bot);
    assert!(stdout.contains("dispatched 0/1"), "{stdout}");
    assert_eq!(api.runs(&admin, WORKFLOW).len(), 1);

    // The runner picks it up: `running`, then a failure, which ends it.
    let _runner = start_runner(&api, &admin, &dir);
    wait("the run to start", RUN_CHANGE, || {
        let running = actions.active_runs().unwrap();
        running
            .iter()
            .any(|r| r["id"] == id && r["status"] == "running")
            .then_some(())
    });
    // The adapter's own request budget is kept for what only it can say.
    wait("the run to fail", RUN_CHANGE, || {
        (status(&api, &admin, id) == "failure").then_some(())
    });
    assert_eq!(actions.status(id).unwrap()["status"], "failure");
    assert!(actions.active_runs().unwrap().is_empty());

    // One rerun: recorded with attempt 1 before it is sent, and its jobs'
    // attempts grow to 2 on the same run id.
    let stdout = apply(&config, &bot);
    assert!(
        stdout.contains(&format!("run rerun {WORK}#{third}: {url}")),
        "{stdout}"
    );
    let rerun = receipt(&api.comments(&admin, third));
    assert_eq!(
        (&rerun["event"], &rerun["run"], &rerun["attempt"]),
        (&json!("rerun"), &json!(id), &json!(1))
    );
    let jobs_path = format!("/repos/{CALLER}/actions/runs/{id}/jobs");
    assert_eq!(api.get(&admin, &jobs_path)[0]["attempt"], 2);
    // The rerun reset the run before answering, so a `failure` from here on
    // is the rerun's.
    assert_ne!(status(&api, &admin, id), "failure");
    wait("the rerun to fail", RUN_CHANGE, || {
        (status(&api, &admin, id) == "failure").then_some(())
    });
    // The next pass reruns nothing more and frees the lease of the ended run.
    let stdout = apply(&config, &bot);
    assert!(
        stdout.contains(&format!("hold {WORK}#{third}: plan already dispatched")),
        "{stdout}"
    );
    assert!(stdout.contains("dispatched 0/1"), "{stdout}");
    assert_eq!(api.runs(&admin, WORKFLOW).len(), 1);
    assert_eq!(api.get(&admin, &jobs_path)[0]["attempt"], 2);
    let slots: Vec<_> = (0..WIP)
        .map(|n| actions.slot(LEASE_BRANCH, n).unwrap())
        .collect();
    assert!(slots.iter().all(|s| s.lease().is_none_or(|l| l.run != id)));

    // Two admits race for one slot, each with its own client: exactly one
    // takes it, and the slot names the winner.
    let barrier = Arc::new(Barrier::new(2));
    let racers: Vec<_> = (0..2)
        .map(|n| {
            let (barrier, board) = (barrier.clone(), board.clone());
            std::thread::spawn(move || {
                let client = Client::from_env(&board).unwrap();
                let caller = target(WORKFLOW);
                let inputs = ["task", "repo", "item", "kind"].map(String::from);
                let actions =
                    Actions::new(&client, &board, &caller, inputs.into(), bot_id).unwrap();
                let observed = actions.slot(LEASE_BRANCH, WIP).unwrap();
                let now = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
                let lease = ForgejoLease {
                    run: 0,
                    claim: format!("race-{n}"),
                    item: format!("{WORK}#{}", n + 1),
                    pool: "claude".into(),
                    taken_at: now.format(&Rfc3339).unwrap(),
                    expires_at: (now + time::Duration::days(1)).format(&Rfc3339).unwrap(),
                };
                barrier.wait();
                // A 409 is a lost race; a 500 is an unknown outcome.
                let won = actions.admit(&observed, &lease, now, || Ok(()));
                (lease.claim, won.unwrap_or(false))
            })
        })
        .collect();
    let results: Vec<_> = racers.into_iter().map(|r| r.join().unwrap()).collect();
    let winners: Vec<_> = results.iter().filter(|(_, won)| *won).collect();
    assert_eq!(winners.len(), 1, "{results:?}");
    let slot = actions.slot(LEASE_BRANCH, WIP).unwrap();
    assert_eq!(slot.lease().map(|l| &l.claim), Some(&winners[0].0));
    std::fs::remove_dir_all(dir).unwrap();
}
