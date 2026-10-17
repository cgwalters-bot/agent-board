//! Read adapters and proposal lowering. Mutations are orchestrated only by the CLI's
//! operator commands (`project init`, `item add`/`item edit`/`ask --apply`), `poll --apply`
//! and its opt-in staging fixture, through the shared GitHub transport.
use anyhow::{Result, ensure};
use board_core::{Capability, Intent, Plan, Snapshot};
use serde_json::{Value, json};

pub mod action;
pub mod batch;
pub mod epic;
pub mod epic_apply;
pub mod github;
pub mod input;
pub mod overlap;
pub mod plan;
pub mod poll;
pub mod runs;
pub mod stack;
pub mod status;
pub mod supervise;
pub use github::{github_item_url, is_github_login, parse_github_item, validate_github_project};

pub trait Forge {
    fn snapshot(&self) -> Result<Snapshot>;
    fn capabilities(&self) -> Result<Vec<Capability>>;
}

pub trait OutputBackend {
    fn lower(&self, plan: &Plan) -> Result<Vec<Value>>;
}

pub struct GitHubSafeOutputs;

impl OutputBackend for GitHubSafeOutputs {
    fn lower(&self, plan: &Plan) -> Result<Vec<Value>> {
        ensure!(
            plan.schema == board_core::PLAN_SCHEMA,
            "unsupported plan schema"
        );
        validate_github_project(&plan.project)?;
        ensure!(plan.actions.len() <= 10, "proposal limit exceeded (10)");
        plan.actions.iter().map(|action| match &action.intent {
            Intent::CloseIssue { item } => {
                ensure!(item.host == "github.com", "item host mismatch");
                item.validate()?;
                ensure!(item.repository.split('/').count() == 2, "invalid GitHub repository");
                Ok(json!({"type":"close_issue", "target_repo":item.repository, "issue_number":item.number}))
            }
            Intent::SetStatus { item, value } => {
                ensure!(item.host == "github.com", "item host mismatch");
                item.validate()?;
                ensure!(item.repository.split('/').count() == 2, "invalid GitHub repository");
                let kind = action.prerequisites.iter().find(|p| p.field == board_core::BoardField::ContentKind)
                    .and_then(|p| p.expected.as_deref()).unwrap_or("issue");
                ensure!(matches!(kind, "issue" | "pull_request"), "unknown content kind");
                Ok(json!({"type":"update_project", "project":plan.project, "content_type":kind, "content_number":item.number, "target_repo":item.repository, "fields":{"Status":value}}))
            }
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use board_core::{Policy, reconcile};
    use url::Url;

    #[test]
    fn github_identity_table() {
        for input in [
            "example/intake#1",
            "https://github.com/example/intake/issues/1",
            "https://github.com/example/intake/pull/1",
        ] {
            assert_eq!(
                parse_github_item(input, None, "github.com").unwrap().number,
                1
            );
        }
        assert_eq!(
            parse_github_item("1", Some("example/intake"), "github.example")
                .unwrap()
                .host,
            "github.example"
        );
        for input in [
            "1",
            "example/intake#0",
            "https://gitlab.example/example/intake/issues/1",
            "group/sub/repo#1",
        ] {
            assert!(parse_github_item(input, None, "github.com").is_err());
        }
    }

    #[test]
    fn github_project_table() {
        for (url, valid) in [
            ("https://github.com/orgs/example/projects/1", true),
            ("https://github.com/users/example/projects/1", true),
            ("https://github.com/not-a-project", false),
            ("https://forge.example/orgs/example/projects/1", false),
            ("https://github.com/orgs/example/projects/0", false),
            ("https://github.com/orgs/my-org-2/projects/1", true),
            (
                format!("https://github.com/orgs/{}/projects/1", "a".repeat(39)).as_str(),
                true,
            ),
            (
                format!("https://github.com/orgs/{}/projects/1", "a".repeat(40)).as_str(),
                false,
            ),
            // Owners are printed into a copyable command: only login syntax passes.
            ("https://github.com/orgs/$(id)/projects/1", false),
            ("https://github.com/orgs/a;id/projects/1", false),
            ("https://github.com/orgs/x&&id/projects/1", false),
            ("https://github.com/orgs/-lead/projects/1", false),
            ("https://github.com/orgs/a%20b/projects/1", false),
            // Enterprise managed user logins carry an underscore suffix.
            ("https://github.com/users/jdoe_corp/projects/1", true),
        ] {
            assert_eq!(
                validate_github_project(&Url::parse(url).unwrap()).is_ok(),
                valid
            );
        }
    }

    #[test]
    fn exact_proposal() {
        let snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        let policy = Policy {
            project: snapshot.project.clone(),
            repositories: vec!["example/intake".into()],
            capabilities: vec![Capability::ProjectFieldEdit],
        };
        let plan = reconcile(&snapshot, &policy, &snapshot.clock).unwrap();
        assert_eq!(
            GitHubSafeOutputs.lower(&plan).unwrap(),
            vec![
                json!({"type":"update_project","project":snapshot.project,"content_type":"issue","content_number":1,"target_repo":"example/intake","fields":{"Status":"Triage"}})
            ]
        );
    }

    #[test]
    fn terminal_proposal_table() {
        for (state, status, reason, expected) in [
            (
                "closed",
                Some(board_core::Status::Backlog),
                "COMPLETED",
                json!({
                    "type":"update_project", "project":"https://github.com/orgs/example/projects/1",
                    "content_type":"issue", "content_number":1, "target_repo":"example/intake",
                    "fields":{"Status":"Done"}
                }),
            ),
            (
                "closed",
                Some(board_core::Status::Done),
                "NOT_PLANNED",
                json!({"type":"update_project", "project":"https://github.com/orgs/example/projects/1",
                "content_type":"issue", "content_number":1, "target_repo":"example/intake",
                "fields":{"Status":"Cancelled"}}),
            ),
        ] {
            let mut snapshot: Snapshot =
                serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
            snapshot.items.truncate(1);
            snapshot.items[0].state = state.into();
            snapshot.items[0].state_reason = Some(reason.into());
            snapshot.items[0].status = status;
            let policy = Policy {
                project: snapshot.project.clone(),
                repositories: vec!["example/intake".into()],
                capabilities: vec![Capability::ProjectFieldEdit, Capability::CloseIssue],
            };
            let plan = reconcile(&snapshot, &policy, &snapshot.clock).unwrap();
            assert_eq!(GitHubSafeOutputs.lower(&plan).unwrap(), vec![expected]);
        }
    }
}
