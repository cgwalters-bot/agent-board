//! Forge-neutral records and deterministic, preview-only reconciliation.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use url::Url;

pub mod admission;
mod agents;
pub use agents::{AgentIdentity, AgentPolicy, DEFAULT_AGENT_POLICY, check_proposals};

pub const SNAPSHOT_SCHEMA: &str = "board-snapshot/v1";
pub const PLAN_SCHEMA: &str = "board-plan/v1";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct ItemRef {
    pub host: String,
    pub repository: String,
    pub number: u64,
}

impl ItemRef {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.host.is_empty()
                && url::Host::parse(&self.host).is_ok()
                && !self.host.contains(['/', ':', '@']),
            "invalid issue host"
        );
        let parts: Vec<_> = self.repository.split('/').collect();
        ensure!(
            parts.len() >= 2
                && parts.iter().all(|part| !part.is_empty()
                    && *part != "."
                    && *part != ".."
                    && part
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))),
            "invalid issue repository"
        );
        ensure!(self.number > 0, "invalid issue number");
        Ok(())
    }

    pub fn display(&self) -> String {
        format!("{}#{}", self.repository, self.number)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChangeRef {
    pub item: ItemRef,
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunRef {
    pub host: String,
    pub repository: String,
    pub id: u64,
    pub attempt: u32,
    pub workflow_ref: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Actor {
    pub host: String,
    pub login: String,
    pub provenance: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum BoardField {
    IssueState,
    IssueStateReason,
    Status,
    ContentKind,
    Priority,
    Turn,
    Ask,
    Result,
    BlockedBy,
    Next,
    News,
    Run,
    Slot,
    BudgetTokens,
    SpentTokens,
    Request,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Status {
    Triage,
    Backlog,
    Todo,
    #[serde(rename = "In Progress")]
    InProgress,
    Draft,
    #[serde(rename = "In Review")]
    InReview,
    Blocked,
    /// An open ask for the operator, and the items it blocks; nonterminal like Blocked.
    #[serde(rename = "Needs human")]
    NeedsHuman,
    Done,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Turn {
    Coordinator,
    Worker,
    Operator,
    External,
    None,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Priority {
    P0,
    P1,
    P2,
    /// Used by label boards (`priority/P3`); GitHub boards provision P0 to P2.
    P3,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub url: Url,
    pub observed_at: String,
    pub provenance: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub complete: bool,
    pub observed_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub identity: ItemRef,
    /// Observed Agent single-select value, not proof of run identity.
    #[serde(default)]
    pub assignment: Option<String>,
    pub title: String,
    pub state: String,
    #[serde(default)]
    pub state_reason: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub status: Option<Status>,
    #[serde(deserialize_with = "required_nullable")]
    pub priority: Option<Priority>,
    #[serde(deserialize_with = "required_nullable")]
    pub turn: Option<Turn>,
    pub fields_complete: bool,
    /// Why `fields_complete` is false, where the reader can say (for example
    /// two labels of one exclusive scope); reconcile repeats it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub field_errors: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    // Legacy snapshots may omit this, but absence never establishes Issue identity.
    pub content_kind: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub assignees: Vec<String>,
    #[serde(default)]
    pub timestamps: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub fields: BTreeMap<String, serde_json::Value>,
}

// A missing field is unknown input, not an observed null. Using a custom
// deserializer prevents serde's implicit default for Option fields.
fn required_nullable<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub schema: String,
    pub project: Url,
    pub clock: String,
    pub coverage: Coverage,
    pub items: Vec<Item>,
    #[serde(default)]
    pub fields: Vec<serde_json::Value>,
}

impl Snapshot {
    pub fn has_status_option(&self, name: &str) -> bool {
        let matching: Vec<_> = self
            .fields
            .iter()
            .filter(|field| field["name"] == "Status")
            .collect();
        matching.len() == 1
            && matching[0]["dataType"] == "SINGLE_SELECT"
            && matching[0]["options"].as_array().is_some_and(|options| {
                options
                    .iter()
                    .filter(|option| option["name"] == name)
                    .count()
                    == 1
            })
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == SNAPSHOT_SCHEMA,
            "unsupported snapshot schema: {}",
            self.schema
        );
        ensure!(
            self.project.scheme() == "https",
            "project must be an HTTPS URL"
        );
        ensure!(
            self.project.username().is_empty()
                && self.project.password().is_none()
                && self.project.query().is_none()
                && self.project.fragment().is_none(),
            "project URL must be canonical"
        );
        time::OffsetDateTime::parse(&self.clock, &time::format_description::well_known::Rfc3339)
            .context("invalid snapshot clock")?;
        time::OffsetDateTime::parse(
            &self.coverage.observed_at,
            &time::format_description::well_known::Rfc3339,
        )
        .context("invalid coverage timestamp")?;
        let mut seen = std::collections::BTreeSet::new();
        for item in &self.items {
            ensure!(seen.insert(&item.identity), "duplicate item identity");
            item.identity.validate()?;
            ensure!(
                ["open", "closed"].contains(&item.state.as_str()),
                "invalid issue state"
            );
            ensure!(
                item.field_errors.is_empty() || !item.fields_complete,
                "field errors on an item with complete fields"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Capability {
    ProjectFieldEdit,
    CreateIssue,
    CloseIssue,
    TypedRequest,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub project: Url,
    pub repositories: Vec<String>,
    pub capabilities: Vec<Capability>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Precondition {
    pub item: ItemRef,
    pub field: BoardField,
    pub expected: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Intent {
    SetStatus {
        item: ItemRef,
        value: Status,
    },
    /// A preview of drift, not proof of accepted completion. An independent
    /// applier must verify issue type and authenticated completion evidence.
    CloseIssue {
        item: ItemRef,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub key: String,
    pub reason: String,
    pub intent: Intent,
    pub prerequisites: Vec<Precondition>,
    pub evidence: Vec<Evidence>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub schema: String,
    pub project: Url,
    pub actions: Vec<Action>,
    pub diagnostics: BTreeMap<String, String>,
}

pub fn reconcile(snapshot: &Snapshot, policy: &Policy, now: &str) -> Result<Plan> {
    reconcile_inner(snapshot, policy, now, None)
}

/// Additional observed facts for the opt-in whole-board status pass. Unknown
/// run state must not be mistaken for an observed absence of running work.
#[derive(Clone, Debug, Default)]
pub struct StatusFacts {
    pub draft: bool,
    pub review_requested: bool,
    pub linked_prs_all_merged: bool,
    pub active_run: Option<bool>,
}

pub fn reconcile_status(
    snapshot: &Snapshot,
    policy: &Policy,
    facts: &BTreeMap<ItemRef, StatusFacts>,
) -> Result<Plan> {
    reconcile_inner(snapshot, policy, &snapshot.clock, Some(facts))
}

fn reconcile_inner(
    snapshot: &Snapshot,
    policy: &Policy,
    now: &str,
    status_facts: Option<&BTreeMap<ItemRef, StatusFacts>>,
) -> Result<Plan> {
    snapshot.validate()?;
    ensure!(
        snapshot.project == policy.project,
        "policy project mismatch"
    );
    ensure!(
        now == snapshot.clock,
        "clock must match injected snapshot clock"
    );
    let mut plan = Plan {
        schema: PLAN_SCHEMA.into(),
        project: snapshot.project.clone(),
        actions: vec![],
        diagnostics: BTreeMap::new(),
    };
    if !policy.capabilities.contains(&Capability::ProjectFieldEdit) {
        bail!("unsupported_capability: project field edits");
    }
    if !snapshot.coverage.complete {
        plan.diagnostics.insert(
            "coverage".into(),
            "missing_data: incomplete snapshot; no repairs proposed".into(),
        );
        return Ok(plan);
    }
    for item in &snapshot.items {
        if item.content_kind == "pull_request" && status_facts.is_none() {
            continue;
        }
        if !matches!(item.content_kind.as_str(), "issue" | "pull_request") {
            plan.diagnostics.insert(
                item.identity.display(),
                "missing_data: unknown content kind; issue identity required".into(),
            );
            continue;
        }
        if !item.fields_complete {
            let mut diagnostic = String::from("missing_data: incomplete fields");
            if !item.field_errors.is_empty() {
                diagnostic = format!("{diagnostic}: {}", item.field_errors.join("; "));
            }
            plan.diagnostics.insert(item.identity.display(), diagnostic);
            continue;
        }
        if !policy.repositories.contains(&item.identity.repository) {
            continue;
        }
        ensure!(
            Some(item.identity.host.as_str()) == snapshot.project.host_str(),
            "item host differs from project host"
        );
        let derived = status_facts
            .and_then(|facts| facts.get(&item.identity))
            .and_then(|facts| {
                if item.state == "closed" || facts.linked_prs_all_merged {
                    Some(("terminal_content", Status::Done))
                } else if facts.review_requested
                    || item
                        .labels
                        .iter()
                        .any(|label| label == "needs-human" || label == "question")
                {
                    Some(("open_human_ask", Status::NeedsHuman))
                } else if facts.draft || facts.active_run == Some(true) {
                    Some(("active_work", Status::InProgress))
                } else if facts.active_run == Some(false)
                    && matches!(item.status, Some(Status::InProgress | Status::NeedsHuman))
                {
                    Some(("inactive_work", Status::Todo))
                } else {
                    None
                }
            });
        let (reason, intent) = if let Some((reason, value)) = derived {
            if item.status.as_ref() == Some(&value) {
                continue;
            }
            (
                reason,
                Intent::SetStatus {
                    item: item.identity.clone(),
                    value,
                },
            )
        } else {
            match (&item.status, item.state.as_str()) {
                (Some(Status::Done), "open") => {
                    plan.diagnostics.insert(
                    item.identity.display(),
                    "needs_review: Done issue is open, possibly reopened; no closure proposed. Review completion evidence and reset Status if needed".into(),
                );
                    continue;
                }
                (status, "closed") => {
                    let target = match item.state_reason.as_deref() {
                        Some("NOT_PLANNED") => Status::Cancelled,
                        Some("COMPLETED") => Status::Done,
                        _ => {
                            plan.diagnostics.insert(item.identity.display(),
                            "missing_data: closed issue has unknown state reason; no terminal repair proposed".into());
                            continue;
                        }
                    };
                    if *status == Some(target.clone()) {
                        continue;
                    }
                    (
                        if target == Status::Cancelled {
                            "closed_issue_not_cancelled"
                        } else {
                            "closed_issue_not_done"
                        },
                        Intent::SetStatus {
                            item: item.identity.clone(),
                            value: target,
                        },
                    )
                }
                (None, "open") => (
                    "missing_status",
                    Intent::SetStatus {
                        item: item.identity.clone(),
                        value: Status::Triage,
                    },
                ),
                _ => continue,
            }
        };
        if let Intent::SetStatus { value, .. } = &intent {
            let name = serde_json::to_value(value)?;
            let supported = name
                .as_str()
                .is_some_and(|name| snapshot.has_status_option(name));
            if !supported {
                plan.diagnostics.insert(item.identity.display(),
                    format!("schema_drift: Status has no unambiguous single-select option {name}; no repair proposed"));
                continue;
            }
        }
        let key_input = serde_json::to_vec(&(snapshot.project.as_str(), &item.identity, reason))?;
        plan.actions.push(Action {
            key: format!("{:x}", Sha256::digest(key_input)),
            reason: reason.into(),
            intent,
            prerequisites: {
                let mut prerequisites = vec![
                    Precondition {
                        item: item.identity.clone(),
                        field: BoardField::Status,
                        expected: item
                            .status
                            .as_ref()
                            .map(serde_json::to_value)
                            .transpose()?
                            .and_then(|value| value.as_str().map(str::to_owned)),
                    },
                    Precondition {
                        item: item.identity.clone(),
                        field: BoardField::IssueState,
                        expected: Some(item.state.clone()),
                    },
                    Precondition {
                        item: item.identity.clone(),
                        field: BoardField::IssueStateReason,
                        expected: item.state_reason.clone(),
                    },
                ];
                if status_facts.is_some() {
                    prerequisites.push(Precondition {
                        item: item.identity.clone(),
                        field: BoardField::ContentKind,
                        expected: Some(item.content_kind.clone()),
                    });
                }
                prerequisites
            },
            evidence: item.evidence.clone(),
        });
    }
    plan.actions.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambiguous_and_malformed_status_schema_table() {
        let valid = serde_json::json!({"name":"Status","dataType":"SINGLE_SELECT","options":[{"name":"Triage"}]});
        for fields in [
            serde_json::json!([]),
            serde_json::json!([valid.clone(), valid.clone()]),
            serde_json::json!([{"name":"Status","dataType":"TEXT","options":[{"name":"Triage"}]}]),
            serde_json::json!([{"name":"Status","dataType":"SINGLE_SELECT","options":[{"name":"Triage"},{"name":"Triage"}]}]),
            serde_json::json!([{"name":"Status","dataType":"SINGLE_SELECT","options":null}]),
            serde_json::json!([{"name":"Status","dataType":"SINGLE_SELECT","options":[null,{},"Triage"]}]),
            serde_json::json!([{"name":"Status","dataType":"SINGLE_SELECT","options":[{"name":"Todo"},{"name":"In Progress"},{"name":"Done"}]}]),
        ] {
            let mut snapshot: Snapshot =
                serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
            snapshot.items.truncate(1);
            snapshot.fields = serde_json::from_value(fields).unwrap();
            let policy = Policy {
                project: snapshot.project.clone(),
                repositories: vec!["example/intake".into()],
                capabilities: vec![Capability::ProjectFieldEdit],
            };
            assert!(!snapshot.has_status_option("Triage"));
            let plan = reconcile(&snapshot, &policy, &snapshot.clock).unwrap();
            assert!(plan.actions.is_empty());
            assert!(plan.diagnostics["example/intake#1"].contains("schema_drift"));
        }
    }

    #[test]
    fn schema_and_terminal_safety_table() {
        for (state, status, reason, schema, expected) in [
            ("open", Some(Status::Done), None, true, None),
            ("open", None, None, false, None),
            ("open", None, None, true, Some(Status::Triage)),
            (
                "closed",
                Some(Status::Todo),
                Some("NOT_PLANNED"),
                true,
                Some(Status::Cancelled),
            ),
            (
                "closed",
                Some(Status::Done),
                Some("NOT_PLANNED"),
                true,
                Some(Status::Cancelled),
            ),
            (
                "closed",
                Some(Status::Todo),
                Some("COMPLETED"),
                true,
                Some(Status::Done),
            ),
            ("closed", Some(Status::Todo), None, true, None),
            ("closed", Some(Status::Todo), Some("OTHER"), true, None),
        ] {
            let mut snapshot: Snapshot =
                serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
            snapshot.items.truncate(1);
            snapshot.items[0].state = state.into();
            snapshot.items[0].status = status;
            snapshot.items[0].state_reason = reason.map(str::to_owned);
            if !schema {
                snapshot.fields.clear();
            }
            let policy = Policy {
                project: snapshot.project.clone(),
                repositories: vec!["example/intake".into()],
                capabilities: vec![Capability::ProjectFieldEdit, Capability::CloseIssue],
            };
            let plan = reconcile(&snapshot, &policy, &snapshot.clock).unwrap();
            assert_eq!(plan.actions.len(), usize::from(expected.is_some()));
            if let Some(expected) = expected {
                assert!(
                    matches!(&plan.actions[0].intent, Intent::SetStatus { value, .. } if *value == expected)
                );
            } else {
                assert!(!plan.diagnostics.is_empty());
            }
        }
    }

    #[test]
    fn content_kind_table() {
        for kind in [
            None,
            Some(""),
            Some("unknown"),
            Some("issue"),
            Some("pull_request"),
        ] {
            for (status, state) in [
                (Some(Status::Done), "open"),
                (Some(Status::Backlog), "closed"),
                (None, "open"),
            ] {
                let mut value: serde_json::Value =
                    serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
                if let Some(kind) = kind {
                    value["items"][0]["content_kind"] = kind.into();
                } else {
                    value["items"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("content_kind");
                }
                let mut snapshot: Snapshot = serde_json::from_value(value).unwrap();
                snapshot.items.truncate(1);
                snapshot.items[0].status = status;
                snapshot.items[0].state = state.into();
                snapshot.items[0].state_reason = Some("COMPLETED".into());
                let policy = Policy {
                    project: snapshot.project.clone(),
                    repositories: vec!["example/intake".into()],
                    capabilities: vec![Capability::ProjectFieldEdit, Capability::CloseIssue],
                };
                let plan = reconcile(&snapshot, &policy, &snapshot.clock).unwrap();
                assert_eq!(
                    plan.actions.len(),
                    usize::from(
                        kind == Some("issue") && state != "open"
                            || kind == Some("issue") && snapshot.items[0].status.is_none()
                    ),
                    "{kind:?}/{state}"
                );
                let unknown = !matches!(kind, Some("issue" | "pull_request"));
                assert_eq!(
                    plan.diagnostics.len(),
                    usize::from(
                        unknown
                            || kind == Some("issue")
                                && snapshot.items[0].status == Some(Status::Done)
                                && state == "open"
                    )
                );
                if unknown {
                    assert!(
                        plan.diagnostics[&snapshot.items[0].identity.display()]
                            .contains("issue identity required")
                    );
                }
            }
        }
    }

    #[test]
    fn terminal_drift_table() {
        let statuses = [
            None,
            Some(Status::Triage),
            Some(Status::Backlog),
            Some(Status::Todo),
            Some(Status::InProgress),
            Some(Status::Draft),
            Some(Status::InReview),
            Some(Status::Blocked),
            Some(Status::NeedsHuman),
            Some(Status::Done),
            Some(Status::Cancelled),
        ];
        for status in statuses {
            for state in ["open", "closed"] {
                let mut snapshot: Snapshot =
                    serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
                snapshot.items.truncate(1);
                snapshot.items[0].status = status.clone();
                snapshot.items[0].state = state.into();
                snapshot.items[0].state_reason = Some("COMPLETED".into());
                let policy = Policy {
                    project: snapshot.project.clone(),
                    repositories: vec!["example/intake".into()],
                    capabilities: vec![Capability::ProjectFieldEdit, Capability::CloseIssue],
                };
                let expected = match (&status, state) {
                    (Some(Status::Done), "open") => None,
                    (status, "closed") if *status != Some(Status::Done) => {
                        Some("closed_issue_not_done")
                    }
                    (None, "open") => Some("missing_status"),
                    _ => None,
                };
                let plan = reconcile(&snapshot, &policy, &snapshot.clock).unwrap();
                assert_eq!(plan.actions.len(), usize::from(expected.is_some()));
                if let Some(reason) = expected {
                    let action = &plan.actions[0];
                    assert_eq!(action.reason, reason);
                    assert_eq!(action.prerequisites.len(), 3);
                    assert_eq!(action.prerequisites[1].field, BoardField::IssueState);
                    assert_eq!(action.prerequisites[1].expected.as_deref(), Some(state));
                    match &action.intent {
                        Intent::CloseIssue { .. } => snapshot.items[0].state = "closed".into(),
                        Intent::SetStatus { value, .. } => {
                            snapshot.items[0].status = Some(value.clone());
                        }
                    }
                    assert!(
                        reconcile(&snapshot, &policy, &snapshot.clock)
                            .unwrap()
                            .actions
                            .is_empty()
                    );
                }
            }
        }
    }

    #[test]
    fn incomplete_and_unauthorized_drift_table() {
        for (status, state) in [
            (None, "open"),
            (Some(Status::Done), "open"),
            (Some(Status::Backlog), "closed"),
        ] {
            for restriction in ["coverage", "fields", "repository", "close_capability"] {
                let mut snapshot: Snapshot =
                    serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
                snapshot.items.truncate(1);
                snapshot.items[0].status = status.clone();
                snapshot.items[0].state = state.into();
                snapshot.items[0].state_reason = Some("COMPLETED".into());
                let mut policy = Policy {
                    project: snapshot.project.clone(),
                    repositories: vec!["example/intake".into()],
                    capabilities: vec![Capability::ProjectFieldEdit, Capability::CloseIssue],
                };
                match restriction {
                    "coverage" => snapshot.coverage.complete = false,
                    "fields" => snapshot.items[0].fields_complete = false,
                    "repository" => policy.repositories.clear(),
                    "close_capability" => policy.capabilities = vec![Capability::ProjectFieldEdit],
                    _ => unreachable!(),
                }
                let plan = reconcile(&snapshot, &policy, &snapshot.clock).unwrap();
                let should_block =
                    restriction != "close_capability" || status == Some(Status::Done);
                assert_eq!(
                    plan.actions.is_empty(),
                    should_block,
                    "{restriction}: {status:?}"
                );
            }
        }
    }

    #[test]
    fn rule_table() {
        for (status, complete, allowed, count) in [
            (None, true, true, 1),
            (Some(Status::Triage), true, true, 0),
            (Some(Status::Done), true, true, 0),
            (None, false, true, 0),
            (None, true, false, 0),
        ] {
            let mut snapshot: Snapshot =
                serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
            snapshot.items.truncate(1);
            snapshot.items[0].status = status;
            snapshot.items[0].fields_complete = complete;
            let policy = Policy {
                project: snapshot.project.clone(),
                repositories: if allowed {
                    vec!["example/intake".into()]
                } else {
                    vec![]
                },
                capabilities: vec![Capability::ProjectFieldEdit],
            };
            let plan = reconcile(&snapshot, &policy, &snapshot.clock).unwrap();
            assert_eq!(plan.actions.len(), count);
            assert_eq!(
                serde_json::to_vec(&plan).unwrap(),
                serde_json::to_vec(&reconcile(&snapshot, &policy, &snapshot.clock).unwrap())
                    .unwrap()
            );
            if count == 1 {
                snapshot.items[0].status = Some(Status::Triage);
                assert!(
                    reconcile(&snapshot, &policy, &snapshot.clock)
                        .unwrap()
                        .actions
                        .is_empty()
                );
            }
        }
    }

    #[test]
    fn neutral_identity_and_project() {
        let identity = ItemRef {
            host: "forge.example".into(),
            repository: "group/subgroup/repository".into(),
            number: 1,
        };
        identity.validate().unwrap();
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        snapshot.project = Url::parse("https://forge.example/group/board").unwrap();
        snapshot.items[0].identity = identity;
        snapshot.validate().unwrap();
    }

    #[test]
    fn malformed_snapshot_table() {
        for (field, value) in [
            ("repository", "not-a-repository"),
            ("repository", "example/.."),
            ("host", "github.com/path"),
        ] {
            let mut value_json: serde_json::Value =
                serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
            value_json["items"][0]["identity"][field] = value.into();
            let snapshot: Snapshot = serde_json::from_value(value_json).unwrap();
            assert!(snapshot.validate().is_err());
        }
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        snapshot.project = Url::parse("http://forge.example/board").unwrap();
        assert!(snapshot.validate().is_err());
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        snapshot.items[0].field_errors = vec!["no priority/ label".into()];
        assert!(snapshot.validate().is_err());
        snapshot.items[0].fields_complete = false;
        snapshot.validate().unwrap();
    }

    #[test]
    fn unknown_snapshot_version_is_rejected() {
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        snapshot.schema = "board-snapshot/v2".into();
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn unknown_snapshot_fields_are_rejected() {
        let original: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        for pointer in ["", "/coverage", "/items/0", "/items/0/identity"] {
            let mut value = original.clone();
            value
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("unknown".into(), true.into());
            assert!(
                serde_json::from_value::<Snapshot>(value).is_err(),
                "{pointer}"
            );
        }
        let mut unknown_evidence = original;
        unknown_evidence["items"][0]["evidence"] = serde_json::json!([{
            "url": "https://forge.example/evidence",
            "observed_at": "2026-10-08T00:00:00Z",
            "provenance": "fixture",
            "unknown": true,
        }]);
        assert!(serde_json::from_value::<Snapshot>(unknown_evidence).is_err());
    }

    #[test]
    fn missing_status_is_not_observed_null() {
        let original: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        let mut missing = original.clone();
        missing["items"][0]
            .as_object_mut()
            .unwrap()
            .remove("status");
        assert!(serde_json::from_value::<Snapshot>(missing.clone()).is_err());
        missing["items"][0]["sta tus"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<Snapshot>(missing).is_err());
        let mut explicit_null = original;
        explicit_null["items"][0]["status"] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<Snapshot>(explicit_null)
                .unwrap()
                .items[0]
                .status
                .is_none()
        );
    }

    #[test]
    fn ordering_is_input_independent() {
        let snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        let policy = Policy {
            project: snapshot.project.clone(),
            repositories: vec!["example/intake".into()],
            capabilities: vec![Capability::ProjectFieldEdit],
        };
        let mut first = snapshot.clone();
        for item in &mut first.items {
            item.status = None;
        }
        let mut second = first.clone();
        second.items.reverse();
        assert_eq!(
            reconcile(&first, &policy, &first.clock).unwrap(),
            reconcile(&second, &policy, &second.clock).unwrap()
        );
    }
}
