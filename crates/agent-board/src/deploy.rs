//! Bounded staging diagnostics, never an authorization to deploy, and the
//! opt-in smoke path ([`smoke`]) that runs only after they are ready.
use anyhow::{Context, Result, ensure};
use board_forge::github::{Client, Transport};
use clap::Args;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;

mod smoke;

pub use smoke::Cancelled;

/// The smoke path's own deadline, checked before each step and well inside
/// the template's job timeout, so that its cleanup still runs.
const SMOKE_DEADLINE: Duration = Duration::from_secs(600);

/// The preflight reads that must all be ready before the smoke path writes.
const SMOKE_READS: [&str; 4] = ["repository", "project", "workflow", "agent-board-pin"];

#[derive(Args)]
pub struct Options {
    /// Trusted default-branch deployment configuration (never dispatcher input).
    #[arg(long, value_name = "FILE")]
    config: PathBuf,
    /// Offline JSON array of the four GraphQL responses; no credentials or network.
    #[arg(long, value_name = "FILE")]
    fixture: Option<PathBuf>,
    /// After a ready preflight, run the smoke path (writes): a fresh scratch repository and project, one fake-engine task, its result and derived Status, then cleanup by creation receipt.
    #[arg(long, conflicts_with = "fixture")]
    apply: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    owner: String,
    repository: String,
    project: u64,
    project_id: String,
    workflow: String,
    workflow_revision: String,
    agent_board_revision: String,
    environment: String,
}

impl Config {
    fn validate(&self) -> Result<()> {
        // Stops a staging rehearsal from inspecting or later targeting production.
        crate::live::validate_stage_scope(&self.owner, &self.repository)?;
        ensure!(
            self.project > 0 && self.project <= i32::MAX as u64 && !self.project_id.is_empty(),
            "exact project number (positive GraphQL Int) and node ID required"
        );
        ensure!(
            self.workflow.ends_with(".yml") || self.workflow.ends_with(".yaml"),
            "workflow must be a YAML basename"
        );
        ensure!(
            self.workflow
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
            "workflow must be an exact basename"
        );
        for pin in [&self.workflow_revision, &self.agent_board_revision] {
            // Stops moving refs from being reported as reviewed immutable code.
            ensure!(
                pin.len() == 40
                    && pin
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "revisions must be full lowercase 40-hex commit pins"
            );
        }
        ensure!(
            !self.environment.trim().is_empty(),
            "environment name required"
        );
        Ok(())
    }
}

fn check(checks: &mut Vec<Value>, group: &str, name: &str, state: &str, detail: &str) {
    checks.push(json!({"group":group,"check":name,"state":state,"detail":detail}));
}

fn inspect(config: &Config, mut read: impl FnMut(&str, Value) -> Result<Value>) -> Vec<Value> {
    let mut checks = Vec::new();
    check(
        &mut checks,
        "configuration",
        "identities-and-pins",
        "ready",
        "exact staging scope and immutable pin syntax validated; trusted config is an operator prerequisite, not authenticated by this command",
    );
    let name = config
        .repository
        .split_once('/')
        .map(|(_, name)| name)
        .unwrap_or_default();
    let variables = json!({"owner":config.owner,"repo":name,"number":config.project});
    let queries = [
        (
            "repository",
            "query($owner:String!,$repo:String!){repository(owner:$owner,name:$repo){id nameWithOwner hasIssuesEnabled viewerPermission}}",
            json!({"owner":config.owner,"repo":name}),
        ),
        (
            "project",
            "query($owner:String!,$number:Int!){organization(login:$owner){login projectV2(number:$number){id number viewerCanUpdate}}}",
            json!({"owner":config.owner,"number":config.project}),
        ),
        (
            "workflow",
            "query($owner:String!,$repo:String!,$expression:String!,$revision:GitObjectID!){repository(owner:$owner,name:$repo){nameWithOwner revision:object(oid:$revision){__typename oid} object(expression:$expression){__typename ... on Blob {oid}}}}",
            json!({"owner":config.owner,"repo":variables["repo"],"revision":config.workflow_revision,"expression":format!("{}:.github/workflows/{}",config.workflow_revision,config.workflow)}),
        ),
        (
            "agent-board-pin",
            "query($revision:GitObjectID!){repository(owner:\"cgwalters-forge\",name:\"agent-board\"){nameWithOwner object(oid:$revision){__typename oid}}}",
            json!({"revision":config.agent_board_revision}),
        ),
    ];
    // Four logical non-paginated queries, even on partial failures; the live
    // transport may retry each one within its own bounds. No write API.
    for (name, query, vars) in queries {
        let result = read(query, vars);
        let state = match &result {
            Err(_) => "unverified",
            Ok(v) => {
                let ready = match name {
                    "repository" => {
                        v["repository"]["nameWithOwner"] == config.repository
                            && v["repository"]["id"]
                                .as_str()
                                .is_some_and(|s| !s.is_empty())
                            && v["repository"]["hasIssuesEnabled"] == true
                            && matches!(
                                v["repository"]["viewerPermission"].as_str(),
                                Some("WRITE" | "MAINTAIN" | "ADMIN")
                            )
                    }
                    "project" => {
                        v["organization"]["login"] == config.owner
                            && v["organization"]["projectV2"]["id"] == config.project_id
                            && v["organization"]["projectV2"]["number"] == config.project
                            && v["organization"]["projectV2"]["viewerCanUpdate"] == true
                    }
                    "workflow" => {
                        v["repository"]["nameWithOwner"] == config.repository
                            && v["repository"]["revision"]["__typename"] == "Commit"
                            && v["repository"]["revision"]["oid"] == config.workflow_revision
                            && v["repository"]["object"]["__typename"] == "Blob"
                            && v["repository"]["object"]["oid"]
                                .as_str()
                                .is_some_and(|s| !s.is_empty())
                    }
                    _ => {
                        v["repository"]["nameWithOwner"] == "cgwalters-forge/agent-board"
                            && v["repository"]["object"]["__typename"] == "Commit"
                            && v["repository"]["object"]["oid"] == config.agent_board_revision
                    }
                };
                if ready { "ready" } else { "blocked" }
            }
        };
        // Do not echo transport errors: they may contain credential-bearing URLs.
        check(
            &mut checks,
            "stage-access",
            name,
            state,
            match name {
                "repository" => {
                    "requires exact repository, Issues enabled and viewer WRITE/MAINTAIN/ADMIN; effective token Issues write and Actions dispatch rights remain unverified"
                }
                "project" => {
                    "requires exact owner, project number/node ID and viewerCanUpdate; this is not proof a mutation would succeed"
                }
                "workflow" => {
                    "requires a workflow blob at the exact configured commit; content, transitive action pins and Actions policy are not audited"
                }
                _ => {
                    "requires the exact source commit in cgwalters-forge/agent-board; does not attest the running binary or a reviewed revision"
                }
            },
        );
    }
    check(
        &mut checks,
        "environment",
        "provisioning",
        "unverified",
        &format!(
            "operator must provision and protect environment {:?}, stage-only credentials, organization/SSO approval and branch restrictions; secret values are never read",
            config.environment
        ),
    );
    check(
        &mut checks,
        "stage-access",
        "credential-capabilities",
        "unverified",
        "verify token resource owner and exact repositories, Projects read/write, Issues read/write, Metadata read and Actions write; no mutation probe is performed",
    );
    check(
        &mut checks,
        "inference-broker",
        "admission-and-network",
        "unverified",
        "separately provision broker admission for exact caller workflow/revision, network ACLs, model and finite budgets; no broker or inference request performed",
    );
    checks
}

/// Whether every stage-access read the smoke path depends on is ready.
fn ready_for_smoke(checks: &[Value]) -> bool {
    SMOKE_READS.iter().all(|name| {
        checks
            .iter()
            .any(|c| c["group"] == "stage-access" && c["check"] == *name && c["state"] == "ready")
    })
}

pub fn run(options: &Options, runtime: &Runtime) -> Result<()> {
    if options.apply {
        return apply(options, runtime);
    }
    let checks = match load(options) {
        Err(error) => vec![
            json!({"group":"configuration","check":"identities-and-pins","state":"blocked","detail":format!("{error:#}")}),
        ],
        Ok(config) => {
            if let Some(path) = &options.fixture {
                let fixture = (|| -> Result<Vec<Value>> {
                    let values: Vec<Value> = serde_json::from_slice(
                        &std::fs::read(path).context("read deploy fixture")?,
                    )
                    .context("decode deploy fixture")?;
                    ensure!(values.len() == 4, "expected four responses");
                    Ok(values)
                })();
                match fixture {
                    Ok(values) => {
                        let mut values = values.into_iter();
                        inspect(&config, |_, _| values.next().context("incomplete fixture"))
                    }
                    // Keep file contents and paths out of the public report.
                    Err(_) => vec![
                        json!({"group":"fixture","check":"responses","state":"blocked","detail":"fixture must be a readable JSON array of exactly four responses; no live reads attempted"}),
                    ],
                }
            } else {
                let client = Client::from_env();
                inspect(&config, |query, vars| match &client {
                    Ok(client) => client.graphql(query, vars),
                    Err(_) => anyhow::bail!("credentials unavailable"),
                })
            }
        }
    };
    let blocked = checks.iter().any(|c| c["state"] == "blocked");
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"schema":"agent-board.deploy-preflight.v1","mode":"preview","writes":0,"checks":checks})
        )?
    );
    ensure!(!blocked, "deploy preflight blocked; no writes attempted");
    Ok(())
}

fn load(options: &Options) -> Result<Config> {
    let bytes = std::fs::read(&options.config).context("read deploy config")?;
    let config: Config = serde_json::from_slice(&bytes).context("decode deploy config")?;
    config.validate()?;
    Ok(config)
}

/// The smoke path, live only: it starts only after every read it depends on
/// is ready, and its report is printed whatever happened, receipts and
/// leftovers included.
fn apply(options: &Options, runtime: &Runtime) -> Result<()> {
    let config = load(options);
    let started = Instant::now();
    // No rate-limit wait runs past the deadline either, so a run ends, cleanup
    // included, inside the template's job timeout.
    let client = Client::from_env().map(|client| client.retry_within(started + SMOKE_DEADLINE));
    let checks = match &config {
        Err(error) => vec![
            json!({"group":"configuration","check":"identities-and-pins","state":"blocked","detail":format!("{error:#}")}),
        ],
        Ok(config) => inspect(config, |query, vars| match &client {
            Ok(client) => client.graphql(query, vars),
            Err(_) => anyhow::bail!("credentials unavailable"),
        }),
    };
    let (smoke, writes, result) = match (&config, &client) {
        (Ok(config), Ok(client)) if ready_for_smoke(&checks) => {
            let mut bytes = [0; 8];
            getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("generate smoke nonce"))?;
            let nonce = format!(
                "agent-board-smoke-{}",
                bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
            );
            smoke::run(runtime, config, client, client, &nonce, &mut || {
                started.elapsed() >= SMOKE_DEADLINE
            })
        }
        _ => (
            Value::Null,
            0,
            Err(anyhow::anyhow!(
                "deploy preflight not ready; no writes attempted"
            )),
        ),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema":"agent-board.deploy-smoke.v1",
            "mode":"apply",
            "writes":writes,
            "checks":checks,
            "smoke":smoke,
        }))?
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        serde_json::from_str(include_str!("../../../fixtures/deploy-config.json")).unwrap()
    }

    #[test]
    fn config_rejects_untrusted_scope_and_malformed_values() {
        let base = serde_json::to_value(json!({
            "owner":"cgwalters-forge-stage", "repository":"cgwalters-forge-stage/scratch",
            "project":1, "projectId":"PVT_fixture", "workflow":"dispatch.yml",
            "workflowRevision":"1111111111111111111111111111111111111111",
            "agentBoardRevision":"2222222222222222222222222222222222222222", "environment":"Main"
        }))
        .unwrap();
        for (key, value) in [
            ("owner", json!("cgwalters-forge")),
            ("repository", json!("production/board")),
            ("repository", json!("cgwalters-forge-stage/../board")),
            ("project", json!(0)),
            ("project", json!(i32::MAX as u64 + 1)),
            ("project", json!(u64::MAX)),
            ("projectId", json!("")),
            ("workflow", json!("../dispatch.yml")),
            ("workflowRevision", json!("main")),
            ("agentBoardRevision", json!("v1")),
            ("environment", json!("")),
            ("environment", Value::Null),
            ("unexpected", json!(true)),
        ] {
            // Replace one fact at a time to keep each boundary independently tested.
            let mut candidate = base.clone();
            candidate[key] = value;
            let parsed = serde_json::from_value::<Config>(candidate);
            assert!(
                parsed.map(|c| c.validate().is_err()).unwrap_or(true),
                "{key}"
            );
        }
    }

    #[test]
    fn config_accepts_largest_graphql_project_number() {
        let mut config = config();
        config.project = i32::MAX as u64;
        config.validate().unwrap();
    }

    #[test]
    fn bounded_preview_never_writes_and_incomplete_reads_never_pass() {
        let config = config();
        config.validate().unwrap();
        let base: Vec<Value> =
            serde_json::from_str(include_str!("../../../fixtures/deploy-reads.json")).unwrap();
        for case in [
            "ready",
            "missing-repository",
            "missing-project",
            "wrong-owner",
            "wrong-project",
            "incomplete",
            "read-error",
            "no-write-access",
            "missing-workflow",
        ] {
            let mut responses = base.clone();
            match case {
                "missing-repository" => responses[0]["repository"] = Value::Null,
                "missing-project" => responses[1]["organization"]["projectV2"] = Value::Null,
                "wrong-owner" => responses[1]["organization"]["login"] = json!("production"),
                "wrong-project" => responses[1]["organization"]["projectV2"]["id"] = json!("other"),
                "incomplete" => responses[1]["organization"]["projectV2"]
                    .as_object_mut()
                    .unwrap()
                    .remove("viewerCanUpdate")
                    .map(|_| ())
                    .unwrap(),
                "no-write-access" => responses[0]["repository"]["viewerPermission"] = json!("READ"),
                "missing-workflow" => responses[2]["repository"]["object"] = Value::Null,
                _ => {}
            }
            let mut requests = 0;
            let checks = inspect(&config, |query, _| {
                assert!(query.starts_with("query("), "unexpected write: {query}");
                let response = responses[requests].clone();
                requests += 1;
                if case == "read-error" {
                    anyhow::bail!("access denied")
                }
                Ok(response)
            });
            assert_eq!(requests, 4);
            let access = &checks[1..5];
            assert_eq!(
                access.iter().all(|c| c["state"] == "ready"),
                case == "ready",
                "{case}"
            );
            // The smoke path writes only after this exact preflight is ready.
            assert_eq!(ready_for_smoke(&checks), case == "ready", "{case}");
            assert!(
                checks
                    .iter()
                    .any(|c| c["group"] == "environment" && c["state"] == "unverified")
            );
            assert!(
                checks
                    .iter()
                    .any(|c| c["group"] == "inference-broker" && c["state"] == "unverified")
            );
        }
    }
}
