//! Typed operator proposals only; checking never authorizes or executes writes.
use crate::{
    is_github_login,
    plan::{Comment, fenced_json, newest_fenced},
    poll::validate_repository,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Number, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const ACTION_SCHEMA: &str = "agent-board/operator-action/v1";
pub const ACTION_FENCE: &str = "agent-board-action";
pub const ACTION_LIMIT: usize = 50;
pub const TEXT_LIMIT: usize = 4000;
/// GitHub's 48 KB Actions variable limit, in UTF-8 bytes.
pub const VARIABLE_VALUE_LIMIT: usize = 49_152;
pub const VARIABLE_NAME_LIMIT: usize = 100;
pub const VARIABLE_REPOSITORY_LIMIT: usize = 100;
pub const RULESET_PATH_LIMIT: usize = 200;
/// Largest ruleset source file, in bytes.
pub const RULESET_SOURCE_LIMIT: usize = 64 * 1024;
/// The fields of a ruleset a create or update writes; the digest covers only these.
pub const RULESET_FIELDS: [&str; 6] = [
    "bypass_actors",
    "conditions",
    "enforcement",
    "name",
    "rules",
    "target",
];
pub const RULESET_TARGETS: [&str; 3] = ["branch", "tag", "push"];
/// `disabled` would switch enforcement off through a reviewed-looking change.
pub const RULESET_ENFORCEMENTS: [&str; 2] = ["evaluate", "active"];
const SAFE_INTEGER: u64 = 9_007_199_254_740_991;
/// Fields that may be null: the proposer's view of absent org state.
const NULLABLE: [&str; 2] = ["expected_current", "expected_current_sha256"];

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
    SetOrgVariable {
        name: String,
        value: String,
        visibility: Visibility,
        /// The value the proposer saw; `None` means the variable must be unset.
        expected_current: Option<String>,
    },
    ApplyOrgRuleset {
        source: RulesetSource,
        ruleset_name: String,
        /// [`ruleset_digest`] of the live ruleset; `None` means it must not exist.
        expected_current_sha256: Option<String>,
    },
}

#[derive(Debug, PartialEq)]
pub enum Visibility {
    All,
    /// Repositories of the organisation, all on the configured allowlist.
    Selected(Vec<String>),
}

/// A ruleset body in a reviewed file: never inline in the proposal.
#[derive(Debug, PartialEq)]
pub struct RulesetSource {
    pub repo: String,
    pub path: String,
    pub sha: String,
}

#[derive(Debug, PartialEq)]
pub struct Target {
    pub repo: String,
    pub number: u64,
}

/// What an action changes.
#[derive(Debug, PartialEq)]
pub enum Scope {
    /// One issue or pull request.
    Item(Target),
    /// Settings of the configured organisation.
    Org(String),
}

#[derive(Debug)]
pub struct Action {
    pub kind: Kind,
    pub scope: Scope,
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
    /// The one organisation org-scoped kinds may change; `None` refuses them.
    pub org: Option<String>,
}

fn fields(value: &Value, required: &[&str], optional: &[&str]) -> Result<()> {
    let object = value.as_object().context("expected an object")?;
    for key in object.keys() {
        ensure!(
            required.contains(&key.as_str()) || optional.contains(&key.as_str()),
            "unknown field {key}"
        );
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
        .filter(|n| n.is_finite() && *n >= 1.0 && *n <= SAFE_INTEGER as f64 && n.fract() == 0.0)
        .map(|n| n as u64)
}

fn lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn hex_field(value: &Value, key: &str, len: usize) -> Result<String> {
    let hex = text(value, key, len)?;
    ensure!(
        lower_hex(hex, len),
        "invalid {key}: expected {len} lowercase hex digits"
    );
    Ok(hex.into())
}

/// A field that is null exactly when the proposer saw no current state.
fn nullable<T>(value: &Value, key: &str, f: impl FnOnce() -> Result<T>) -> Result<Option<T>> {
    match value[key] {
        Value::Null => Ok(None),
        _ => f().map(Some),
    }
}

/// Upper case so the proposal and GitHub's stored name are byte-identical.
fn variable_name(value: &Value) -> Result<String> {
    let name = text(value, "name", VARIABLE_NAME_LIMIT)?;
    ensure!(
        name.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
            && name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            && !name.starts_with("GITHUB_"),
        "invalid variable name {name:?}: expected upper-case letters, digits and underscores, \
         not starting with a digit or GITHUB_"
    );
    Ok(name.into())
}

/// Variable values are opaque: blank values are allowed and nothing is trimmed.
fn variable_value(value: &Value, key: &str) -> Result<String> {
    let text = value[key]
        .as_str()
        .with_context(|| format!("{key} must be a string"))?;
    ensure!(
        !text.is_empty() && text.len() <= VARIABLE_VALUE_LIMIT,
        "{key} must be 1 to {VARIABLE_VALUE_LIMIT} UTF-8 bytes"
    );
    Ok(text.into())
}

/// `[A-Za-z0-9_.-]+` segments, none `.` or `..`, relative, ending in `.json`.
fn validate_ruleset_path(path: &str) -> Result<()> {
    ensure!(
        path.len() <= RULESET_PATH_LIMIT
            && path.ends_with(".json")
            && path.split('/').all(|segment| {
                !segment.is_empty()
                    && segment != "."
                    && segment != ".."
                    && segment
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            }),
        "invalid ruleset path {path:?}: expected a relative .json path without . or .. segments"
    );
    Ok(())
}

impl Config {
    pub fn from_json(value: &Value) -> Result<Self> {
        fields(value, &["writers", "repositories"], &["org"])?;
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
        let org = match value.get("org") {
            None => None,
            Some(org) => {
                let org = org.as_str().context("org must be a string")?;
                ensure!(
                    org.is_empty() || is_github_login(org),
                    "invalid org {org:?}: expected a GitHub owner"
                );
                (!org.is_empty()).then(|| org.to_owned())
            }
        };
        let config = Self {
            writers: list("writers")?,
            repositories: list("repositories")?,
            org,
        };
        for writer in &config.writers {
            ensure!(is_github_login(writer), "invalid writer login");
        }
        for repo in &config.repositories {
            validate_repository(repo)?;
        }
        Ok(config)
    }

    fn allowed(&self, repo: &str) -> Result<()> {
        validate_repository(repo)?;
        ensure!(
            self.repositories
                .iter()
                .any(|r| r.eq_ignore_ascii_case(repo)),
            "repository {repo} is not allowed"
        );
        Ok(())
    }

    fn repository(&self, value: &Value, key: &str) -> Result<()> {
        self.allowed(text(value, key, 200)?)
    }

    fn org(&self, value: &Value) -> Result<String> {
        let org = text(value, "org", 39)?;
        ensure!(is_github_login(org), "invalid org {org:?}");
        let configured = self
            .org
            .as_deref()
            .with_context(|| format!("organisation {org} is not allowed: no org is configured"))?;
        ensure!(
            org.eq_ignore_ascii_case(configured),
            "organisation {org} is not allowed"
        );
        Ok(org.into())
    }

    /// Allowlisted repositories of `org`, each named once.
    fn org_repositories(&self, value: &Value, org: &str) -> Result<Vec<String>> {
        let repos = value["repositories"]
            .as_array()
            .context("repositories must be an array")?;
        ensure!(
            (1..=VARIABLE_REPOSITORY_LIMIT).contains(&repos.len()),
            "invalid repositories length"
        );
        let mut seen = BTreeSet::new();
        repos
            .iter()
            .map(|repo| {
                let repo = repo.as_str().context("repositories must be strings")?;
                self.allowed(repo)?;
                ensure!(
                    repo.split_once('/')
                        .is_some_and(|(owner, _)| owner.eq_ignore_ascii_case(org)),
                    "repository {repo} is not allowed: not owned by {org}"
                );
                ensure!(
                    seen.insert(repo.to_ascii_lowercase()),
                    "duplicate repository {repo} is not allowed"
                );
                Ok(repo.to_owned())
            })
            .collect()
    }

    fn ruleset_source(&self, value: &Value) -> Result<RulesetSource> {
        let source = &value["source"];
        fields(source, &["repo", "path", "sha"], &[]).context("invalid source")?;
        let repo = text(source, "repo", 200)?;
        self.allowed(repo)?;
        let path = text(source, "path", RULESET_PATH_LIMIT)?;
        validate_ruleset_path(path)?;
        Ok(RulesetSource {
            repo: repo.into(),
            path: path.into(),
            sha: hex_field(source, "sha", 40)?,
        })
    }
}

fn item_action(kind: &str, action: &Value, config: &Config) -> Result<Action> {
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
    fields(action, &required, &[])?;
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
                hex_field(action, key, 40)?;
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
    let string = |key: &str| -> Result<String> { Ok(text(action, key, TEXT_LIMIT)?.into()) };
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
        scope: Scope::Item(target("repo", "number")?),
        mutation: action.clone(),
    })
}

fn org_variable(action: &Value, config: &Config) -> Result<Action> {
    let required = [
        "type",
        "org",
        "name",
        "value",
        "visibility",
        "expected_current",
    ];
    let selected: &[&str] = if action["visibility"] == "selected" {
        &["repositories"]
    } else {
        &[]
    };
    fields(action, &[&required[..], selected].concat(), &[])?;
    let org = config.org(action)?;
    let kind = Kind::SetOrgVariable {
        name: variable_name(action)?,
        value: variable_value(action, "value")?,
        visibility: match text(action, "visibility", 20)? {
            "all" => Visibility::All,
            "selected" => Visibility::Selected(config.org_repositories(action, &org)?),
            other => bail!("unsupported variable visibility {other:?}"),
        },
        expected_current: nullable(action, "expected_current", || {
            variable_value(action, "expected_current")
        })?,
    };
    Ok(Action {
        kind,
        scope: Scope::Org(org),
        mutation: action.clone(),
    })
}

fn org_ruleset(action: &Value, config: &Config) -> Result<Action> {
    fields(
        action,
        &[
            "type",
            "org",
            "source",
            "ruleset_name",
            "expected_current_sha256",
        ],
        &[],
    )?;
    let org = config.org(action)?;
    let kind = Kind::ApplyOrgRuleset {
        source: config.ruleset_source(action)?,
        ruleset_name: text(action, "ruleset_name", 200)?.into(),
        expected_current_sha256: nullable(action, "expected_current_sha256", || {
            hex_field(action, "expected_current_sha256", 64)
        })?,
    };
    Ok(Action {
        kind,
        scope: Scope::Org(org),
        mutation: action.clone(),
    })
}

impl Proposal {
    pub fn from_json(value: &Value, config: &Config) -> Result<Self> {
        fields(value, &["schema", "title", "why", "actions"], &[])?;
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
            .map(|action| {
                let object = action.as_object().context("expected an object")?;
                // Only the two expected-state fields may be null, and only by name.
                for (key, field) in object {
                    ensure!(
                        !field.is_null() || NULLABLE.contains(&key.as_str()),
                        "{key} must not be null"
                    );
                }
                match text(action, "type", 40)? {
                    "set_org_variable" => org_variable(action, config),
                    "apply_org_ruleset" => org_ruleset(action, config),
                    kind => item_action(kind, action, config),
                }
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            title: title.into(),
            why: why.into(),
            actions,
        })
    }
}

// Integers in JavaScript's safe range only, so serde_json and JSON.stringify
// spell every accepted number the same way.
fn safe_integer(number: &Number) -> Result<i64> {
    let integer = match (number.as_i64(), number.as_f64()) {
        (Some(integer), _) => integer,
        (None, Some(float)) if float.fract() == 0.0 && float.abs() <= SAFE_INTEGER as f64 => {
            float as i64
        }
        _ => bail!("unsupported number {number}: expected an integer"),
    };
    ensure!(
        integer.unsigned_abs() <= SAFE_INTEGER,
        "unsupported number {number}: outside JavaScript's safe integer range"
    );
    Ok(integer)
}

fn write_canonical(value: &Value, out: &mut String) -> Result<()> {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            // JavaScript's default sort: UTF-16 code units, not code points.
            entries.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, (key, value)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key)?);
                out.push(':');
                write_canonical(value, out)?;
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out)?;
            }
            out.push(']');
        }
        Value::Number(number) => out.push_str(&safe_integer(number)?.to_string()),
        other => out.push_str(&serde_json::to_string(other)?),
    }
    Ok(())
}

fn canonical(value: &Value) -> Result<String> {
    let mut out = String::new();
    write_canonical(value, &mut out)?;
    Ok(out)
}

/// A ruleset's writable fields as compact JSON with object keys sorted by code
/// point and arrays in order: the body a create or update sends, and the input
/// of [`ruleset_digest`]. Read-only fields of a live ruleset (`id`, `_links`, ...)
/// are dropped; non-integer numbers are refused.
pub fn normalize_ruleset(ruleset: &Value) -> Result<String> {
    let object = ruleset.as_object().context("a ruleset must be an object")?;
    let writable: serde_json::Map<_, _> = object
        .iter()
        .filter(|(key, _)| RULESET_FIELDS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    canonical(&Value::Object(writable))
}

/// Lowercase hex SHA-256 of [`normalize_ruleset`], as `expected_current_sha256` holds it.
pub fn ruleset_digest(ruleset: &Value) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(normalize_ruleset(ruleset)?.as_bytes())
    ))
}

/// Checks a ruleset source file against its proposal and the live ruleset
/// (`None` when creating), returning the normalised body to send. The file is
/// the writable body only, and adds no bypass actor the live ruleset lacks.
pub fn check_ruleset_source(
    source: &str,
    ruleset_name: &str,
    live: Option<&Value>,
) -> Result<String> {
    ensure!(
        source.len() <= RULESET_SOURCE_LIMIT,
        "ruleset source exceeds {RULESET_SOURCE_LIMIT} bytes"
    );
    let value: Value = serde_json::from_str(source).context("decode ruleset source")?;
    fields(
        &value,
        &["name", "target", "enforcement", "rules"],
        &["bypass_actors", "conditions"],
    )
    .context("invalid ruleset source")?;
    ensure!(
        value["name"] == ruleset_name,
        "ruleset source name does not match {ruleset_name:?}"
    );
    ensure!(
        RULESET_TARGETS.contains(&value["target"].as_str().unwrap_or_default()),
        "unsupported ruleset target"
    );
    ensure!(
        RULESET_ENFORCEMENTS.contains(&value["enforcement"].as_str().unwrap_or_default()),
        "unsupported ruleset enforcement: expected evaluate or active"
    );
    let rules = value["rules"]
        .as_array()
        .context("rules must be an array")?;
    for rule in rules {
        ensure!(
            rule["type"].is_string(),
            "each rule must be an object with a string type"
        );
    }
    if let Some(conditions) = value.get("conditions") {
        ensure!(conditions.is_object(), "conditions must be an object");
    }
    let actors = |ruleset: &Value| -> Result<Vec<String>> {
        match ruleset.get("bypass_actors") {
            None => Ok(Vec::new()),
            Some(actors) => actors
                .as_array()
                .context("bypass_actors must be an array")?
                .iter()
                .map(canonical)
                .collect(),
        }
    };
    let allowed = live.map(actors).transpose()?.unwrap_or_default();
    let mut seen = BTreeSet::new();
    for actor in actors(&value)? {
        ensure!(
            allowed.contains(&actor),
            "ruleset source adds bypass actor {actor}"
        );
        ensure!(seen.insert(actor), "ruleset source repeats a bypass actor");
    }
    normalize_ruleset(&value)
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
        assert_eq!(proposal.actions.len(), 11);
        assert_eq!(
            proposal.actions[0].scope,
            Scope::Item(Target {
                repo: "example/work".into(),
                number: 1
            })
        );
        assert_eq!(proposal.actions[9].scope, Scope::Org("example".into()));
        assert_eq!(
            proposal.actions[9].kind,
            Kind::SetOrgVariable {
                name: "REVIEW_WORKFLOW".into(),
                value: "example/work/.github/workflows/review.yml@refs/heads/main".into(),
                visibility: Visibility::Selected(vec![
                    "example/work".into(),
                    "example/other".into()
                ]),
                expected_current: None,
            }
        );
        assert!(matches!(
            &proposal.actions[10].kind,
            Kind::ApplyOrgRuleset { source, expected_current_sha256: Some(digest), .. }
                if source.path == "rulesets/required-review.json"
                    && *digest == fixture["ruleset_digests"][0]["sha256"]
        ));
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
                let error = format!("{:#}", Proposal::from_json(&value, &config).unwrap_err());
                // The review site matches scope rejections by this phrase.
                assert!(
                    key == "invalid_actions" || error.contains("not allowed"),
                    "{error}"
                );
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
                if NULLABLE.contains(&key.as_str()) {
                    continue;
                }
                let mut invalid = action.clone();
                invalid[key] = Value::Null;
                let mut value = valid.clone();
                value["actions"] = json!([invalid]);
                assert!(Proposal::from_json(&value, &config).is_err());
            }
        }
        assert!(Config::from_json(&json!({"writers": [], "repositories": []})).is_err());
    }

    /// Org kinds need a configured org, and values are bounded in UTF-8 bytes.
    #[test]
    fn org_config_and_value_bytes() {
        let fixture = fixture();
        let valid = &fixture["valid"];
        let org_actions: Vec<Value> = valid["actions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|action| action.get("org").is_some())
            .cloned()
            .collect();
        assert_eq!(org_actions.len(), 2);
        let config = |org: Option<Value>| {
            let mut config = fixture["config"].clone();
            match org {
                Some(org) => config["org"] = org,
                None => drop(config.as_object_mut().unwrap().remove("org")),
            }
            Config::from_json(&config)
        };
        for (org, accepted) in [
            (Some(json!("example")), true),
            (Some(json!("Example")), true),
            (None, false),
            (Some(json!("")), false),
            (Some(json!("elsewhere")), false),
        ] {
            let config = config(org).unwrap();
            for action in &org_actions {
                let mut value = valid.clone();
                value["actions"] = json!([action]);
                assert_eq!(
                    Proposal::from_json(&value, &config).is_ok(),
                    accepted,
                    "{:?} {action}",
                    config.org
                );
            }
            // Repository-scoped kinds do not depend on the org.
            let mut value = valid.clone();
            value["actions"] = valid["actions"].as_array().unwrap()[..9].to_vec().into();
            assert!(Proposal::from_json(&value, &config).is_ok());
        }
        for org in [json!("-bad"), json!("example/work"), json!(null), json!(1)] {
            assert!(config(Some(org)).is_err());
        }

        let config = config(Some(json!("example"))).unwrap();
        // Three-byte characters: within the schema's code-point limit, over the byte limit.
        let wide = "\u{20ac}".repeat(VARIABLE_VALUE_LIMIT / 3);
        for (key, value, accepted) in [
            ("value", "x".repeat(VARIABLE_VALUE_LIMIT), true),
            ("value", "x".repeat(VARIABLE_VALUE_LIMIT + 1), false),
            ("value", wide.clone(), true),
            ("value", format!("{wide}x"), false),
            ("expected_current", "x".repeat(VARIABLE_VALUE_LIMIT), true),
            ("expected_current", format!("{wide}x"), false),
        ] {
            let mut action = org_actions[0].clone();
            action[key] = json!(value);
            let mut proposal = valid.clone();
            proposal["actions"] = json!([action]);
            assert_eq!(
                Proposal::from_json(&proposal, &config).is_ok(),
                accepted,
                "{key} of {} bytes",
                value.len()
            );
        }
    }

    #[test]
    fn ruleset_digests_and_sources() {
        let fixture = fixture();
        for vector in fixture["ruleset_digests"].as_array().unwrap() {
            assert_eq!(
                ruleset_digest(&vector["ruleset"]).unwrap(),
                vector["sha256"],
                "{}",
                normalize_ruleset(&vector["ruleset"]).unwrap()
            );
        }
        // UTF-16 order puts U+10000 (a surrogate pair) before U+FF61; code-point order would not.
        assert!(
            normalize_ruleset(&fixture["ruleset_digests"][2]["ruleset"])
                .unwrap()
                .contains("{\"z\":3,\"\u{10000}\":2,\"\u{ff61}\":1}")
        );
        let live = &fixture["ruleset_digests"][0]["ruleset"];
        // Equal numbers spelled differently normalise alike; fractions are refused.
        let mut spelled = live.clone();
        spelled["rules"][0]["parameters"]["workflows"][0]["repository_id"] = json!(123456.0);
        assert_eq!(
            ruleset_digest(&spelled).unwrap(),
            ruleset_digest(live).unwrap()
        );
        for number in [json!(1.5), json!(9_007_199_254_740_992_u64), json!(-1e300)] {
            let mut invalid = live.clone();
            invalid["rules"][0]["parameters"]["workflows"][0]["repository_id"] = number;
            assert!(ruleset_digest(&invalid).is_err());
        }
        assert!(ruleset_digest(&json!([])).is_err());

        // The live ruleset's writable body, as a reviewed source file would hold it.
        let body: Value = serde_json::from_str(&normalize_ruleset(live).unwrap()).unwrap();
        let name = "Required review";
        let with = |key: &str, value: Value| {
            let mut source = body.clone();
            source[key] = value;
            source.to_string()
        };
        let without = |key: &str| {
            let mut source = body.clone();
            source.as_object_mut().unwrap().remove(key);
            source.to_string()
        };
        let admin = &body["bypass_actors"][0];
        let other = json!({"actor_id": 7, "actor_type": "Team", "bypass_mode": "always"});
        let pull_requests = json!({"actor_id": null, "actor_type": "OrganizationAdmin", "bypass_mode": "pull_request"});
        let large = format!("{}{}", " ".repeat(RULESET_SOURCE_LIMIT), body);
        for (source, live, accepted) in [
            (body.to_string(), Some(live), true),
            (with("enforcement", json!("evaluate")), Some(live), true),
            (with("target", json!("push")), Some(live), true),
            (with("bypass_actors", json!([])), None, true),
            (without("bypass_actors"), None, true),
            (without("conditions"), Some(live), true),
            (with("bypass_actors", json!([])), Some(live), true),
            (body.to_string(), None, false),
            (
                with("bypass_actors", json!([admin, admin])),
                Some(live),
                false,
            ),
            (
                with("bypass_actors", json!([admin, other])),
                Some(live),
                false,
            ),
            (
                with("bypass_actors", json!([pull_requests])),
                Some(live),
                false,
            ),
            (with("bypass_actors", json!({})), Some(live), false),
            (with("enforcement", json!("disabled")), Some(live), false),
            (with("target", json!("repository")), Some(live), false),
            (with("name", json!("Other")), Some(live), false),
            (with("name", json!(null)), Some(live), false),
            (with("id", json!(42)), Some(live), false),
            (with("rules", json!([{"type": 1}])), Some(live), false),
            (with("rules", json!(["deletion"])), Some(live), false),
            (with("rules", json!({})), Some(live), false),
            (with("conditions", json!([])), Some(live), false),
            (
                with("rules", json!([{"type": "x", "n": 0.5}])),
                Some(live),
                false,
            ),
            (without("rules"), Some(live), false),
            (without("enforcement"), Some(live), false),
            ("[]".into(), Some(live), false),
            ("{".into(), Some(live), false),
            (large, Some(live), false),
        ] {
            assert_eq!(
                check_ruleset_source(&source, name, live).is_ok(),
                accepted,
                "{source:.200} {}",
                live.is_some()
            );
        }
        assert_eq!(
            check_ruleset_source(&body.to_string(), name, Some(live)).unwrap(),
            normalize_ruleset(live).unwrap()
        );
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
