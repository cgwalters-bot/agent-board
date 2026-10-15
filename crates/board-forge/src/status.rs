//! Opt-in, read-only status proposals. Board text is not run authority: a Run
//! field may only cause a bounded GET in the caller's configured repositories.
use crate::{
    github::API_ROOT,
    poll::{Rest, validate_repository},
};
use anyhow::{Context, Result, ensure};
use board_core::{Capability, Policy, Snapshot, StatusFacts};
use std::collections::BTreeMap;

pub const STATUS_REQUEST_LIMIT: usize = 500;

#[derive(Debug, Default)]
pub struct Config {
    pub enabled: bool,
    pub project: String,
    pub repositories: Vec<String>,
    pub run_repositories: Vec<String>,
}

pub fn configured(value: &serde_json::Value) -> Result<Config> {
    let Some(value) = value.get("statusReconcile") else {
        return Ok(Config::default());
    };
    for key in value
        .as_object()
        .context("statusReconcile must be an object")?
        .keys()
    {
        ensure!(
            ["enabled", "project", "repositories", "runRepositories"].contains(&key.as_str()),
            "unknown statusReconcile field {key}"
        );
    }
    Ok(Config {
        enabled: value
            .get("enabled")
            .map(|v| v.as_bool().context("invalid statusReconcile enabled"))
            .transpose()?
            .unwrap_or(false),
        project: value
            .get("project")
            .map(|v| v.as_str().context("invalid statusReconcile project"))
            .transpose()?
            .unwrap_or("")
            .into(),
        repositories: serde_json::from_value(
            value
                .get("repositories")
                .cloned()
                .unwrap_or(serde_json::json!([])),
        )?,
        run_repositories: serde_json::from_value(
            value
                .get("runRepositories")
                .cloned()
                .unwrap_or(serde_json::json!([])),
        )?,
    })
}

pub fn plan<R: Rest>(
    rest: &R,
    snapshot: &Snapshot,
    config: &Config,
    operator: &str,
) -> Result<board_core::Plan> {
    ensure!(config.enabled, "status reconciliation is disabled");
    snapshot.validate()?;
    ensure!(
        snapshot.project.as_str() == config.project,
        "status reconciliation project mismatch"
    );
    for repo in config.repositories.iter().chain(&config.run_repositories) {
        validate_repository(repo)?;
    }
    let policy = Policy {
        project: snapshot.project.clone(),
        repositories: config.repositories.clone(),
        capabilities: vec![Capability::ProjectFieldEdit],
    };
    let mut requests = 0;
    let mut get = |path: &str| -> Result<serde_json::Value> {
        ensure!(
            requests < STATUS_REQUEST_LIMIT,
            "status request limit exceeded"
        );
        requests += 1;
        let response = rest.get(&format!("{API_ROOT}{path}"), None)?;
        ensure!(
            response.status == 200,
            "status read returned {}",
            response.status
        );
        Ok(response.body)
    };
    let mut facts = BTreeMap::new();
    if snapshot.coverage.complete {
        for item in &snapshot.items {
            if !item.fields_complete
                || !config.repositories.contains(&item.identity.repository)
                || !matches!(item.content_kind.as_str(), "issue" | "pull_request")
            {
                continue;
            }
            ensure!(
                item.identity.host == crate::github::DEFAULT_HOST,
                "status item host mismatch"
            );
            let mut observed = StatusFacts::default();
            if item.content_kind == "pull_request" && item.state == "open" {
                let pull = get(&format!(
                    "/repos/{}/pulls/{}",
                    item.identity.repository, item.identity.number
                ))?;
                ensure!(
                    pull["number"].as_u64() == Some(item.identity.number)
                        && pull["state"] == "open",
                    "pull changed since snapshot"
                );
                observed.draft = pull["draft"].as_bool().context("missing pull draft fact")?;
                observed.review_requested = pull["requested_reviewers"]
                    .as_array()
                    .context("missing requested reviewers")?
                    .iter()
                    .any(|reviewer| reviewer["login"].as_str() == Some(operator));
            }
            // The snapshot reader marks truncated/redacted nested field values
            // incomplete. Never infer completion from an empty set of links.
            let linked: Vec<_> = item
                .fields
                .values()
                .filter(|v| v["__typename"] == "ProjectV2ItemFieldPullRequestValue")
                .filter_map(|v| v["pullRequests"].as_array())
                .flatten()
                .collect();
            observed.linked_prs_all_merged =
                !linked.is_empty() && linked.iter().all(|pr| pr["state"] == "MERGED");
            if let Some(run) = item.fields.get("Run") {
                let run = run
                    .as_str()
                    .or_else(|| run["text"].as_str())
                    .context("invalid Run field")?;
                if !run.is_empty() {
                    let prefix = format!("https://{}/", crate::github::DEFAULT_HOST);
                    let (repo, id) = run
                        .strip_prefix(&prefix)
                        .and_then(|p| p.split_once("/actions/runs/"))
                        .context("invalid Run URL")?;
                    validate_repository(repo)?;
                    ensure!(
                        config.run_repositories.iter().any(|r| r == repo),
                        "Run repository outside status scope"
                    );
                    ensure!(
                        !id.is_empty()
                            && id.bytes().all(|b| b.is_ascii_digit())
                            && id.parse::<u64>()? > 0,
                        "invalid Run ID"
                    );
                    let run = get(&format!("/repos/{repo}/actions/runs/{id}"))?;
                    ensure!(
                        run["id"].as_u64() == Some(id.parse()?),
                        "run identity mismatch"
                    );
                    observed.active_run = Some(match run["status"].as_str() {
                        Some("queued" | "in_progress" | "waiting" | "pending" | "requested") => {
                            true
                        }
                        Some("completed") => false,
                        _ => anyhow::bail!("unknown run status"),
                    });
                }
            }
            // No Run URL alone cannot establish the absence of a live job.
            facts.insert(item.identity.clone(), observed);
        }
    }
    board_core::reconcile_status(snapshot, &policy, &facts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GitHubSafeOutputs, OutputBackend, poll::RestResponse};
    use board_core::{Intent, Status};
    use serde_json::{Value, json};
    use std::cell::RefCell;

    struct Offline {
        pull: Value,
        run: Value,
        reads: RefCell<Vec<String>>,
    }

    impl Rest for Offline {
        fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
            self.reads.borrow_mut().push(url.into());
            Ok(RestResponse {
                status: 200,
                etag: None,
                last_modified: None,
                link: None,
                body: if url.contains("/actions/runs/") {
                    self.run.clone()
                } else {
                    self.pull.clone()
                },
            })
        }

        fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
            panic!("status reconciliation must never write")
        }
    }

    fn fixture() -> (Snapshot, Config, Offline) {
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        snapshot.items.truncate(1);
        snapshot.items[0].status = Some(Status::Todo);
        snapshot.items[0].state = "open".into();
        snapshot.fields = vec![
            json!({"name":"Status", "dataType":"SINGLE_SELECT", "options":
            (["Triage", "Todo", "In Progress", "Needs human", "Done"].map(|name| json!({"name":name})))}),
        ];
        let config = Config {
            enabled: true,
            project: snapshot.project.to_string(),
            repositories: vec![snapshot.items[0].identity.repository.clone()],
            run_repositories: vec!["example/runners".into()],
        };
        let offline = Offline {
            pull: json!({"number":snapshot.items[0].identity.number, "state":"open", "draft":false, "requested_reviewers":[]}),
            run: json!({"id":42, "status":"in_progress"}),
            reads: RefCell::new(vec![]),
        };
        (snapshot, config, offline)
    }

    #[test]
    fn whole_board_status_table() {
        for (case, expected) in [
            ("closed_issue", Some(Status::Done)),
            ("closed_pr", Some(Status::Done)),
            ("draft_pr", Some(Status::InProgress)),
            ("label", Some(Status::NeedsHuman)),
            ("question", Some(Status::NeedsHuman)),
            ("review_requested", Some(Status::NeedsHuman)),
            ("other_reviewer", None),
            ("active_run", Some(Status::InProgress)),
            ("finished_run", Some(Status::Todo)),
            ("unknown_run", None),
            ("merged_links", Some(Status::Done)),
            ("open_link", None),
            ("empty_links", None),
            ("terminal_wins", Some(Status::Done)),
            ("ask_wins", Some(Status::NeedsHuman)),
            ("incomplete", None),
            ("out_of_scope", None),
        ] {
            let (mut snapshot, mut config, mut offline) = fixture();
            let item = &mut snapshot.items[0];
            // No Lead, Workflow, assignment or Turn exempts an item.
            item.assignment = Some("unrelated".into());
            item.fields.insert("Lead".into(), json!("human"));
            item.fields.insert("Workflow".into(), json!("other"));
            match case {
                "closed_issue" | "closed_pr" | "terminal_wins" => {
                    item.state = "closed".into();
                    item.state_reason = Some("NOT_PLANNED".into());
                    if case == "closed_pr" {
                        item.content_kind = "pull_request".into();
                    }
                    if case == "terminal_wins" {
                        item.labels.push("needs-human".into());
                    }
                }
                "draft_pr" | "review_requested" | "other_reviewer" => {
                    item.content_kind = "pull_request".into();
                    offline.pull["draft"] = json!(case == "draft_pr");
                    offline.pull["requested_reviewers"] = json!([{"login":if case == "review_requested" { "operator" } else { "someone" }}]);
                }
                "label" | "question" | "ask_wins" => {
                    item.labels.push(
                        if case == "question" {
                            "question"
                        } else {
                            "needs-human"
                        }
                        .into(),
                    );
                }
                "active_run" | "finished_run" | "unknown_run" => {
                    item.status = Some(Status::InProgress);
                    if case != "unknown_run" {
                        item.fields.insert(
                            "Run".into(),
                            json!({"text":"https://github.com/example/runners/actions/runs/42"}),
                        );
                    }
                    if case == "active_run" {
                        item.status = Some(Status::Todo);
                    }
                    if case == "finished_run" {
                        offline.run["status"] = json!("completed");
                    }
                }
                "merged_links" | "open_link" | "empty_links" => {
                    let links = match case {
                        "merged_links" => json!([{"state":"MERGED"}, {"state":"MERGED"}]),
                        "open_link" => json!([{"state":"MERGED"}, {"state":"OPEN"}]),
                        _ => json!([]),
                    };
                    item.fields.insert("Linked pull requests".into(), json!({"__typename":"ProjectV2ItemFieldPullRequestValue", "pullRequests":links}));
                }
                "incomplete" => {
                    snapshot.coverage.complete = false;
                }
                "out_of_scope" => {
                    config.repositories.clear();
                }
                _ => unreachable!(),
            }
            if case == "ask_wins" {
                snapshot.items[0].fields.insert(
                    "Run".into(),
                    json!("https://github.com/example/runners/actions/runs/42"),
                );
            }
            let plan = plan(&offline, &snapshot, &config, "operator").unwrap();
            assert_eq!(
                plan.actions.len(),
                usize::from(expected.is_some()),
                "{case}"
            );
            if let Some(expected) = expected {
                assert!(
                    matches!(&plan.actions[0].intent, Intent::SetStatus { value, .. } if *value == expected),
                    "{case}"
                );
                let outputs = GitHubSafeOutputs.lower(&plan).unwrap();
                assert_eq!(outputs[0]["content_type"], snapshot.items[0].content_kind);
            }
            assert_eq!(
                serde_json::to_vec(&plan).unwrap(),
                serde_json::to_vec(&super::plan(&offline, &snapshot, &config, "operator").unwrap())
                    .unwrap()
            );
        }
    }

    #[test]
    fn configuration_and_read_boundaries_table() {
        assert!(!configured(&json!({})).unwrap().enabled);
        assert!(!configured(&json!({"statusReconcile":{}})).unwrap().enabled);
        for value in [
            json!(null),
            json!({"enabled":"yes"}),
            json!({"enable":true}),
        ] {
            assert!(configured(&json!({"statusReconcile":value})).is_err());
        }
        for case in [
            "disabled",
            "project",
            "host",
            "run_repo",
            "run_path",
            "run_id",
            "run_status",
            "pull_draft",
            "pull_changed",
        ] {
            let (mut snapshot, mut config, mut offline) = fixture();
            match case {
                "disabled" => config.enabled = false,
                "project" => config.project.push('2'),
                "host" => snapshot.items[0].identity.host = "elsewhere.test".into(),
                "pull_draft" | "pull_changed" => {
                    snapshot.items[0].content_kind = "pull_request".into();
                    if case == "pull_draft" {
                        offline.pull["draft"] = Value::Null;
                    } else {
                        offline.pull["state"] = json!("closed");
                    }
                }
                _ => {
                    let url = if case == "run_path" {
                        "https://evil.test/example/runners/actions/runs/42"
                    } else {
                        "https://github.com/example/runners/actions/runs/42"
                    };
                    snapshot.items[0].fields.insert("Run".into(), json!(url));
                    if case == "run_repo" {
                        config.run_repositories.clear();
                    }
                    if case == "run_id" {
                        offline.run["id"] = json!(43);
                    }
                    if case == "run_status" {
                        offline.run["status"] = json!("unknown");
                    }
                }
            }
            assert!(
                plan(&offline, &snapshot, &config, "operator").is_err(),
                "{case}"
            );
            if matches!(
                case,
                "disabled" | "project" | "host" | "run_repo" | "run_path"
            ) {
                assert!(offline.reads.borrow().is_empty(), "{case}");
            }
        }
    }

    #[test]
    fn request_budget_is_bounded() {
        let (mut snapshot, config, offline) = fixture();
        let mut item = snapshot.items[0].clone();
        item.fields.insert(
            "Run".into(),
            json!("https://github.com/example/runners/actions/runs/42"),
        );
        snapshot.items = (1..=STATUS_REQUEST_LIMIT + 1)
            .map(|number| {
                let mut item = item.clone();
                item.identity.number = number as u64;
                item
            })
            .collect();
        assert!(
            plan(&offline, &snapshot, &config, "operator")
                .unwrap_err()
                .to_string()
                .contains("request limit")
        );
        assert_eq!(offline.reads.borrow().len(), STATUS_REQUEST_LIMIT);
    }
}
