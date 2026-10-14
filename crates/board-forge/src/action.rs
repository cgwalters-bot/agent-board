//! Typed operator proposals only; checking never authorizes or executes writes.
use crate::{
    plan::{Comment, fenced_json, newest_fenced},
    poll::validate_repository,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

pub const ACTION_SCHEMA: &str = "agent-board/operator-action/v1";
pub const ACTION_FENCE: &str = "agent-board-action";
pub const ACTION_LIMIT: usize = 50;
pub const TEXT_LIMIT: usize = 4000;

#[derive(Debug, PartialEq)]
pub enum Kind {
    CloseIssue {
        reason: String,
    },
    ReopenIssue,
    AddLabel {
        label: String,
    },
    RemoveLabel {
        label: String,
    },
    SetProjectField {
        project: url::Url,
        field: String,
        value: String,
    },
    AddSubIssue {
        parent: Target,
    },
    MoveSubIssue {
        parent: Target,
        previous_parent: Target,
    },
    ApprovePullRequest {
        head_sha: String,
    },
    MergePullRequest {
        head_sha: String,
        method: String,
    },
}

#[derive(Debug, PartialEq)]
pub struct Target {
    pub repo: String,
    pub number: u64,
}

#[derive(Debug)]
pub struct Action {
    pub kind: Kind,
    pub target: Target,
    /// Validated typed mutation, printed without dropping any fields.
    mutation: Value,
}

impl Action {
    pub fn mutation(&self) -> &Value {
        &self.mutation
    }
}

#[derive(Debug)]
pub struct Proposal {
    pub title: String,
    pub why: String,
    pub actions: Vec<Action>,
}

/// Loaded from a protected default-branch file, never a proposal or workflow input.
pub struct Config {
    pub writers: Vec<String>,
    pub repositories: Vec<String>,
}

fn fields(value: &Value, required: &[&str]) -> Result<()> {
    let object = value.as_object().context("expected an object")?;
    for key in object.keys() {
        ensure!(required.contains(&key.as_str()), "unknown field {key}");
    }
    for key in required {
        ensure!(object.contains_key(*key), "missing field {key}");
    }
    Ok(())
}

fn text<'a>(value: &'a Value, key: &str, limit: usize) -> Result<&'a str> {
    let text = value[key]
        .as_str()
        .with_context(|| format!("{key} must be a string"))?;
    ensure!(
        !text.trim().is_empty() && text.chars().count() <= limit,
        "invalid {key} length"
    );
    Ok(text)
}

fn number(value: &Value, key: &str) -> Result<()> {
    ensure!(integer(&value[key]).is_some(), "invalid {key} number");
    Ok(())
}

// JSON Schema and JavaScript numbers do not distinguish 1 from 1.0 or 1e0.
fn integer(value: &Value) -> Option<u64> {
    value
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 1.0 && *n <= 9_007_199_254_740_991.0 && n.fract() == 0.0)
        .map(|n| n as u64)
}

impl Config {
    pub fn from_json(value: &Value) -> Result<Self> {
        fields(value, &["writers", "repositories"])?;
        let list = |key: &str| -> Result<Vec<String>> {
            let values = value[key]
                .as_array()
                .with_context(|| format!("{key} must be an array"))?;
            ensure!(
                !values.is_empty() && values.len() <= 100,
                "invalid {key} length"
            );
            values
                .iter()
                .map(|v| v.as_str().map(str::to_owned).context("expected a string"))
                .collect()
        };
        let config = Self {
            writers: list("writers")?,
            repositories: list("repositories")?,
        };
        for writer in &config.writers {
            ensure!(crate::is_github_login(writer), "invalid writer login");
        }
        for repo in &config.repositories {
            validate_repository(repo)?;
        }
        Ok(config)
    }

    fn repository(&self, value: &Value, key: &str) -> Result<()> {
        let repo = text(value, key, 200)?;
        validate_repository(repo)?;
        ensure!(
            self.repositories
                .iter()
                .any(|r| r.eq_ignore_ascii_case(repo)),
            "repository {repo} is not allowed"
        );
        Ok(())
    }
}

impl Proposal {
    pub fn from_json(value: &Value, config: &Config) -> Result<Self> {
        fields(value, &["schema", "title", "why", "actions"])?;
        ensure!(
            value["schema"] == ACTION_SCHEMA,
            "unsupported action schema"
        );
        let title = text(value, "title", 200)?;
        ensure!(!title.contains(['\r', '\n']), "title must be one line");
        let why = text(value, "why", TEXT_LIMIT)?;
        let actions = value["actions"]
            .as_array()
            .context("actions must be an array")?;
        // Bound bulk proposals so checking and human review cannot expand without limit.
        ensure!(
            !actions.is_empty() && actions.len() <= ACTION_LIMIT,
            "invalid actions length"
        );
        let actions = actions
            .iter()
            .map(|action| -> Result<Action> {
                let kind = text(action, "type", 40)?;
                let extra: &[&str] = match kind {
                    "close_issue" => &["reason"],
                    "reopen_issue" => &[],
                    "add_label" | "remove_label" => &["label"],
                    "set_project_field" => &["project", "field", "value"],
                    "add_sub_issue" => &["parent_repo", "parent_number"],
                    "move_sub_issue" => &[
                        "parent_repo",
                        "parent_number",
                        "previous_parent_repo",
                        "previous_parent_number",
                    ],
                    "approve_pull_request" => &["head_sha"],
                    "merge_pull_request" => &["head_sha", "method"],
                    _ => bail!("unsupported action type {kind}"),
                };
                let mut required = vec!["type", "repo", "number"];
                required.extend_from_slice(extra);
                fields(action, &required)?;
                config.repository(action, "repo")?;
                number(action, "number")?;
                for key in extra {
                    match *key {
                        "parent_repo" | "previous_parent_repo" => config.repository(action, key)?,
                        "parent_number" | "previous_parent_number" => number(action, key)?,
                        "project" => {
                            let raw = text(action, key, 200)?;
                            let project = url::Url::parse(raw)?;
                            crate::validate_github_project(&project)?;
                            let parts: Vec<_> = project.path().split('/').collect();
                            ensure!(
                                parts.len() == 5
                                    && parts[4].len() <= 15
                                    && parts[4].bytes().all(|b| b.is_ascii_digit())
                                    && parts[4].starts_with(|c: char| ('1'..='9').contains(&c))
                                    && project.as_str() == raw,
                                "noncanonical project URL"
                            );
                        }
                        "field" => ensure!(
                            ["Priority", "Status"].contains(&text(action, key, 200)?),
                            "unsupported project field"
                        ),
                        "head_sha" => {
                            let sha = text(action, key, 40)?;
                            ensure!(
                                sha.len() == 40
                                    && sha
                                        .bytes()
                                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                                "invalid head SHA"
                            );
                        }
                        "method" => ensure!(
                            ["merge", "squash", "rebase"].contains(&text(action, key, 20)?),
                            "invalid merge method"
                        ),
                        _ => {
                            text(action, key, if *key == "reason" { TEXT_LIMIT } else { 200 })?;
                        }
                    }
                }
                let string =
                    |key: &str| -> Result<String> { Ok(text(action, key, TEXT_LIMIT)?.into()) };
                let target = |repo: &str, number: &str| -> Result<Target> {
                    Ok(Target {
                        repo: string(repo)?,
                        number: integer(&action[number]).context("invalid target number")?,
                    })
                };
                let kind = match kind {
                    "close_issue" => Kind::CloseIssue {
                        reason: string("reason")?,
                    },
                    "reopen_issue" => Kind::ReopenIssue,
                    "add_label" => Kind::AddLabel {
                        label: string("label")?,
                    },
                    "remove_label" => Kind::RemoveLabel {
                        label: string("label")?,
                    },
                    "set_project_field" => Kind::SetProjectField {
                        project: url::Url::parse(&string("project")?)?,
                        field: string("field")?,
                        value: string("value")?,
                    },
                    "add_sub_issue" => Kind::AddSubIssue {
                        parent: target("parent_repo", "parent_number")?,
                    },
                    "move_sub_issue" => Kind::MoveSubIssue {
                        parent: target("parent_repo", "parent_number")?,
                        previous_parent: target("previous_parent_repo", "previous_parent_number")?,
                    },
                    "approve_pull_request" => Kind::ApprovePullRequest {
                        head_sha: string("head_sha")?,
                    },
                    "merge_pull_request" => Kind::MergePullRequest {
                        head_sha: string("head_sha")?,
                        method: string("method")?,
                    },
                    _ => bail!("unsupported action type"),
                };
                Ok(Action {
                    kind,
                    target: target("repo", "number")?,
                    mutation: action.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            title: title.into(),
            why: why.into(),
            actions,
        })
    }
}

pub fn newest_action<'a>(
    comments: &'a [Comment],
    config: &Config,
) -> Result<(Proposal, &'a Comment)> {
    let writers: Vec<_> = config.writers.iter().map(String::as_str).collect();
    let comment = newest_fenced(comments, &writers, ACTION_FENCE)?;
    let value = fenced_json(&comment.body, ACTION_FENCE)?.context("action fence vanished")?;
    Ok((Proposal::from_json(&value, config)?, comment))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/operator-actions.json")).unwrap()
    }

    #[test]
    fn shared_cases_and_bounds() {
        let fixture = fixture();
        let config = Config::from_json(&fixture["config"]).unwrap();
        let valid = &fixture["valid"];
        let proposal = Proposal::from_json(valid, &config).unwrap();
        assert_eq!(proposal.actions.len(), 9);
        for action in fixture["valid_actions"].as_array().unwrap() {
            let mut value = valid.clone();
            value["actions"] = json!([action]);
            assert!(Proposal::from_json(&value, &config).is_ok(), "{value}");
        }
        for (action, expected) in proposal
            .actions
            .iter()
            .zip(valid["actions"].as_array().unwrap())
        {
            assert_eq!(&action.mutation, expected);
        }
        for key in ["invalid_actions", "scope_rejections"] {
            for action in fixture[key].as_array().unwrap() {
                let mut value = valid.clone();
                value["actions"] = json!([action]);
                assert!(Proposal::from_json(&value, &config).is_err(), "{value}");
            }
        }
        for (key, value) in [
            ("title", json!("two\nlines")),
            ("title", json!("x".repeat(201))),
            ("why", json!("x".repeat(TEXT_LIMIT + 1))),
            ("schema", json!("v2")),
            ("actions", json!([])),
            (
                "actions",
                json!(vec![valid["actions"][0].clone(); ACTION_LIMIT + 1]),
            ),
            ("extra", Value::Null),
        ] {
            let mut invalid = valid.clone();
            invalid[key] = value;
            assert!(Proposal::from_json(&invalid, &config).is_err());
        }
        for key in ["schema", "title", "why", "actions"] {
            let mut invalid = valid.clone();
            invalid.as_object_mut().unwrap().remove(key);
            assert!(Proposal::from_json(&invalid, &config).is_err());
        }
        for action in valid["actions"].as_array().unwrap() {
            for key in action.as_object().unwrap().keys() {
                let mut invalid = action.clone();
                invalid[key] = Value::Null;
                let mut value = valid.clone();
                value["actions"] = json!([invalid]);
                assert!(Proposal::from_json(&value, &config).is_err());
            }
        }
        assert!(Config::from_json(&json!({"writers": [], "repositories": []})).is_err());
    }

    #[test]
    fn provenance_and_fences() {
        let fixture = fixture();
        let config = Config::from_json(&fixture["config"]).unwrap();
        let body = format!("```{ACTION_FENCE}\n{}\n```", fixture["valid"]);
        let original = Comment {
            url: "comment-1".into(),
            body: body.clone(),
            author: Some("PLANNER-BOT".into()),
            edited: false,
            editor: None,
            changed: time::OffsetDateTime::UNIX_EPOCH,
        };
        assert!(newest_action(std::slice::from_ref(&original), &config).is_ok());
        for editor in [Some("visitor".into()), None] {
            let edited = Comment {
                edited: true,
                editor,
                body: "fence removed".into(),
                changed: original.changed + time::Duration::seconds(1),
                ..original.clone()
            };
            assert!(newest_action(&[original.clone(), edited], &config).is_err());
        }
        let outsider = Comment {
            author: Some("visitor".into()),
            ..original.clone()
        };
        assert!(newest_action(&[outsider], &config).is_err());
        for body in [
            format!("{body}\n{body}"),
            body.replace("```agent-board-action", "> ```agent-board-action"),
            format!("```{ACTION_FENCE}\n{{"),
        ] {
            assert!(
                newest_action(
                    &[Comment {
                        body,
                        ..original.clone()
                    }],
                    &config
                )
                .is_err()
            );
        }
    }
}
