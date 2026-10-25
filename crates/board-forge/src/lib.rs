//! Read adapters and proposal lowering. Mutations are orchestrated only by the CLI's
//! operator commands (`project init`, `item add`/`item edit`/`ask --apply`), `poll --apply`
//! and its opt-in staging fixture, through the shared GitHub transport.
use anyhow::{Result, ensure};
use board_core::{Capability, Intent, Plan, Snapshot};
use serde_json::{Value, json};

pub mod action;
pub mod batch;
pub mod broker;
pub mod caps;
pub mod changes;
pub mod epic;
pub mod epic_apply;
pub mod forgejo;
pub mod github;
pub mod input;
pub mod org_status;
pub mod overlap;
pub mod pass;
pub mod plan;
pub mod poll;
pub mod release_train;
pub mod review;
pub mod runs;
pub mod stack;
pub mod status;
pub mod supervise;
pub use github::{github_item_url, is_github_login, parse_github_item, validate_github_project};

/// The attribution every bot-posted text ends with, the poll's and the
/// supervisor executor's alike, unless agent-board.json's `generatedBy`
/// names another ([`Footer`]).
pub const FOOTER: &str = "Generated-by: https://github.com/cgwalters/#llms";
/// The longest `generatedBy`, in characters.
pub const FOOTER_LIMIT: usize = 200;

/// The attribution the poll and the supervisor executor append, and the
/// executor strips from the agent's text before appending it once: both read
/// it from the same `generatedBy`, defaulting to [`FOOTER`].
#[derive(Clone, Debug, PartialEq)]
pub struct Footer(String);

impl Default for Footer {
    fn default() -> Self {
        Self(FOOTER.into())
    }
}

impl std::fmt::Display for Footer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Footer {
    /// `generatedBy`: absent or null keeps [`FOOTER`]. One trimmed line of
    /// at most [`FOOTER_LIMIT`] characters: the executor strips it as the
    /// last line of the agent's trimmed text, which a footer with a line
    /// break or surrounding space would never match, so the agent's own copy
    /// would be posted above the executor's.
    pub fn from_json(value: Option<&Value>) -> Result<Self> {
        let Some(value) = value.filter(|v| !v.is_null()) else {
            return Ok(Self::default());
        };
        let text = value.as_str().unwrap_or_default();
        ensure!(
            !text.is_empty()
                && text.trim() == text
                && text.chars().count() <= FOOTER_LIMIT
                && !text.chars().any(char::is_control),
            "generatedBy is not one trimmed line of 1 to {FOOTER_LIMIT} characters"
        );
        Ok(Self(text.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

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
                // agentic-job's close_issue (docs/safe-outputs.md, "Closing issues")
                // takes `repo`, not update_project's `target_repo`, and requires a
                // body so watchers learn why. No state_reason: plans carry none, and
                // the handler defaults to completed.
                let body = format!("Closed by agent-board's reconcile rule `{}`.", action.reason);
                Ok(json!({"type":"close_issue", "repo":item.repository, "issue_number":item.number, "body":body}))
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
    fn generated_by_is_one_trimmed_bounded_line() {
        let long = "x".repeat(FOOTER_LIMIT + 1);
        let cases: &[(Option<Value>, Option<&str>)] = &[
            (None, Some(FOOTER)),
            (Some(Value::Null), Some(FOOTER)),
            (Some(json!("Made by a tool")), Some("Made by a tool")),
            (Some(json!("x".repeat(FOOTER_LIMIT))), Some(&long[1..])),
            (Some(json!(long)), None),
            (Some(json!("")), None),
            (Some(json!(" padded")), None),
            (Some(json!("padded ")), None),
            (Some(json!("two\nlines")), None),
            (Some(json!("carriage\rreturn")), None),
            (Some(json!("tab\there")), None),
            (Some(json!(1)), None),
            (Some(json!(["Made by"])), None),
        ];
        for (value, expected) in cases {
            let got = Footer::from_json(value.as_ref()).ok();
            assert_eq!(got.as_ref().map(Footer::as_str), *expected, "{value:?}");
        }
    }

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
    fn close_issue_matches_agentic_job() {
        let snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        let item = snapshot.items[0].identity.clone();
        let plan = Plan {
            schema: board_core::PLAN_SCHEMA.into(),
            project: snapshot.project.clone(),
            actions: vec![board_core::Action {
                key: "k".into(),
                reason: "accepted_completion".into(),
                intent: Intent::CloseIssue { item },
                prerequisites: vec![],
                evidence: vec![],
            }],
            diagnostics: Default::default(),
        };
        assert_eq!(
            GitHubSafeOutputs.lower(&plan).unwrap(),
            vec![json!({
                "type":"close_issue", "repo":"example/intake", "issue_number":1,
                "body":"Closed by agent-board's reconcile rule `accepted_completion`."
            })]
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
