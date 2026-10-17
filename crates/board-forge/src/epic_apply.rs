//! Opt-in epic mutations, with a dispatch-style claim preventing blind replay.
use crate::epic::{Bounds, EPIC_PLAN_FENCE, Epic, EpicPlan, IssueRef, read_epic};
use crate::github::{API_ROOT, Transport, is_github_login};
use crate::plan::{Comment, fenced_json, issue_comments, newest_fenced, plan_id, send, viewer};
use crate::poll::Rest;
use anyhow::{Context, Result, ensure};
use board_core::ItemRef;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub const APPLY_MARKER: &str = "agent-board-epic-apply";
pub const FINGERPRINT_MARKER: &str = "agent-board-epic-fingerprint";

/// Optional caller observations, never authority to select a different plan.
#[derive(Debug, Default)]
pub struct Preconditions {
    pub request: Option<String>,
    pub comment: Option<u64>,
    pub updated_at: Option<OffsetDateTime>,
}

#[derive(Debug)]
pub struct PlanChanged;

impl std::fmt::Display for PlanChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("epic plan precondition failed: plan changed; re-check before applying")
    }
}

impl std::error::Error for PlanChanged {}

impl Preconditions {
    fn enabled(&self) -> bool {
        self.request.is_some() || self.comment.is_some() || self.updated_at.is_some()
    }

    fn check(&self, value: &Value, from: &Comment) -> Result<()> {
        let comment = from
            .url
            .rsplit_once("#issuecomment-")
            .and_then(|(_, id)| id.parse::<u64>().ok());
        if self
            .request
            .as_ref()
            .is_some_and(|id| value.get("request").and_then(Value::as_str) != Some(id.as_str()))
            || self.comment.is_some_and(|id| comment != Some(id))
            || self.updated_at.is_some_and(|at| from.changed != at)
        {
            return Err(PlanChanged.into());
        }
        Ok(())
    }
}

/// Trusted configuration from the caller's default-branch checkout, not inputs
/// supplied by a dispatcher or values supplied in a plan.
#[derive(Debug)]
pub struct Config {
    pub operator: String,
    pub bot: String,
    pub bounds: Bounds,
    pub project: String,
    pub priority_field: String,
    pub priority_field_id: String,
    pub priorities: BTreeMap<String, String>,
    pub default_priority: String,
}

impl Config {
    pub fn from_json(value: &Value) -> Result<Self> {
        let object = value.as_object().context("epic config is not an object")?;
        for key in object.keys() {
            ensure!(
                [
                    "operator",
                    "bot",
                    "repositories",
                    "project",
                    "priority_field",
                    "priority_field_id",
                    "priorities",
                    "default_priority"
                ]
                .contains(&key.as_str()),
                "unknown epic config field {key:?}"
            );
        }
        let text = |key: &str| -> Result<String> {
            Ok(object
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .with_context(|| format!("epic config has no nonempty {key}"))?
                .to_owned())
        };
        let operator = text("operator")?;
        let bot = text("bot")?;
        ensure!(
            is_github_login(&operator) && is_github_login(&bot),
            "invalid configured login"
        );
        let repositories = object
            .get("repositories")
            .and_then(Value::as_array)
            .context("epic config repositories is not a list")?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .context("repository is not a string")
            })
            .collect::<Result<Vec<_>>>()?;
        let priorities = object
            .get("priorities")
            .and_then(Value::as_object)
            .context("epic config priorities is not an object")?
            .iter()
            .map(|(name, id)| {
                Ok((
                    name.clone(),
                    id.as_str()
                        .filter(|s| !s.trim().is_empty())
                        .context("priority has no option id")?
                        .to_owned(),
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let bounds = Bounds {
            repositories,
            priorities: priorities.keys().cloned().collect(),
        };
        bounds.validate()?;
        let default_priority = text("default_priority")?;
        ensure!(
            priorities.contains_key(&default_priority),
            "default_priority is not a configured option"
        );
        Ok(Self {
            operator,
            bot,
            bounds,
            project: text("project")?,
            priority_field: text("priority_field")?,
            priority_field_id: text("priority_field_id")?,
            priorities,
            default_priority,
        })
    }

    pub fn writers(&self) -> [&str; 2] {
        [&self.operator, &self.bot]
    }
}

/// Canonical membership/state fingerprint, not a digest of untrusted text.
/// A census can call this with the same complete sub-issue snapshot. Body and
/// Priority drift are deliberately not represented by this protocol version.
pub fn fingerprint(epic: &Epic) -> Result<String> {
    let mut states: Vec<_> = epic
        .sub_issues
        .iter()
        .map(|i| (i.to_string().to_ascii_lowercase(), "OPEN"))
        .chain(
            epic.closed
                .iter()
                .map(|i| (i.to_string().to_ascii_lowercase(), "CLOSED")),
        )
        .collect();
    states.sort_unstable();
    Ok(serde_json::to_string(&states)?)
}

fn marker(id: &str) -> String {
    format!("<!-- {APPLY_MARKER}: {id} -->")
}

fn records(comments: &[Comment], config: &Config, id: &str) -> Vec<u64> {
    let marker = marker(id);
    comments
        .iter()
        .filter(|c| {
            c.author
                .as_deref()
                .is_some_and(|a| config.writers().iter().any(|w| w.eq_ignore_ascii_case(a)))
                && c.body.lines().next().map(str::trim_end) == Some(marker.as_str())
        })
        .map(|c| {
            c.url
                .rsplit_once("#issuecomment-")
                .and_then(|(_, n)| n.parse().ok())
                .unwrap_or(0)
        })
        .collect()
}

fn accepted(comments: &[Comment], config: &Config) -> Result<(Value, Comment)> {
    let from = newest_fenced(comments, &config.writers(), EPIC_PLAN_FENCE)?;
    let value = fenced_json(&from.body, EPIC_PLAN_FENCE)?.context("epic plan fence vanished")?;
    Ok((value, from.clone()))
}

/// Read-only preview. Apply checks for an existing claim before validating
/// current sub-issues: a prior close otherwise makes its own plan invalid.
pub fn preview<T: Transport>(client: &T, issue: &ItemRef, config: &Config) -> Result<Vec<Value>> {
    preview_checked(client, issue, config, &Preconditions::default())
}

pub fn preview_checked<T: Transport>(
    client: &T,
    issue: &ItemRef,
    config: &Config,
    expected: &Preconditions,
) -> Result<Vec<Value>> {
    let comments = issue_comments(client, issue)?;
    let (value, from) = accepted(&comments, config)?;
    expected.check(&value, &from)?;
    let epic = read_epic(client, issue)?;
    let plan = EpicPlan::validate(&value, &config.bounds, &epic)?;
    ensure!(
        epic.designated,
        "parent does not have the operator's epic label"
    );
    let mut out = Vec::new();
    for mut mutation in plan.preview(&epic) {
        if mutation["graphql"] == "updateProjectV2ItemFieldValue" {
            mutation["field"] = config.priority_field.clone().into();
            out.push(json!({"action": mutation["action"], "graphql": "addProjectV2ItemById", "project": config.project, "issue": mutation["issue"]}));
        }
        let link = mutation["graphql"] == "addSubIssue";
        let n = mutation["action"].clone();
        out.push(mutation);
        if link {
            out.push(json!({"action": n, "graphql": "addProjectV2ItemById", "project": config.project, "issue": format!("NEW-{n}")}));
            out.push(json!({"action": n, "graphql": "updateProjectV2ItemFieldValue", "project": config.project, "field": config.priority_field, "value": config.default_priority, "issue": format!("NEW-{n}")}));
        }
    }
    Ok(out)
}

/// Claims before writing, never retries a claimed plan, and stops at the first
/// failed request. As for dispatch, serialize callers: GitHub comments are not
/// a transactional lock. A killed or ambiguous pass needs operator inspection
/// and a replacement plan, not an automatic replay.
pub fn apply<C: Transport + Rest>(
    client: &C,
    issue: &ItemRef,
    config: &Config,
    now: OffsetDateTime,
) -> Result<Vec<String>> {
    apply_checked(client, issue, config, now, &Preconditions::default())
}

pub fn apply_checked<C: Transport + Rest>(
    client: &C,
    issue: &ItemRef,
    config: &Config,
    now: OffsetDateTime,
    expected: &Preconditions,
) -> Result<Vec<String>> {
    let comments = issue_comments(client, issue)?;
    let (value, from) = accepted(&comments, config)?;
    expected.check(&value, &from)?;
    let request = crate::epic::request(&value)?;
    let request_output = || request.as_ref().map(|id| format!("Request: {id}"));
    let id = plan_id(&from)?;
    ensure!(
        viewer(client)?.eq_ignore_ascii_case(&config.bot),
        "epic apply requires the configured bot's token"
    );
    if !records(&comments, config, &id).is_empty() {
        return Ok(request_output()
            .into_iter()
            .chain(["Already claimed; nothing changed.".into()])
            .collect());
    }
    let epic = read_epic(client, issue)?;
    ensure!(
        epic.designated,
        "parent does not have the operator's epic label"
    );
    EpicPlan::validate(&value, &config.bounds, &epic)?;
    let path = format!(
        "{API_ROOT}repos/{}/issues/{}/comments",
        issue.repository, issue.number
    );
    // Prevent replacement or editing between the caller's check and our first
    // write, including the replay claim. One bounded read, not a polling loop.
    if expected.enabled() {
        let latest = issue_comments(client, issue)?;
        let (current, comment) = accepted(&latest, config)?;
        expected.check(&current, &comment)?;
        if plan_id(&comment)? != id || current != value {
            return Err(PlanChanged.into());
        }
    }
    let (_, posted) = send(
        client,
        "POST",
        &path,
        &json!({"body": format!("{}\nApplying {}. Do not replay this plan after a failure.", marker(&id), from.url)}),
        0,
    )?;
    let mine = posted["id"]
        .as_u64()
        .context("apply record has no id; no mutations attempted")?;
    let mut done = Vec::new();
    let result = (|| -> Result<()> {
        let again = issue_comments(client, issue)?;
        let (current, comment) = accepted(&again, config)?;
        expected.check(&current, &comment)?;
        if plan_id(&comment)? != id || current != value {
            return Err(PlanChanged.into());
        }
        ensure!(
            !records(&again, config, &id).iter().any(|n| *n < mine),
            "an earlier apply claim won; no mutations attempted"
        );
        // Recheck membership and all refusals after claiming, before any write.
        let epic = read_epic(client, issue)?;
        ensure!(
            epic.designated,
            "parent no longer has the operator's epic label"
        );
        let checked = EpicPlan::validate(&current, &config.bounds, &epic)?;
        execute(client, &checked, &epic, config, &mut done)
    })();
    let dated = now.format(&Rfc3339)?;
    let outcome = match &result {
        Ok(()) => "Applied.".to_owned(),
        Err(_) => "Stopped after a failed request; its outcome may be ambiguous. Inspect before replacing the plan.".to_owned(),
    };
    // Keep request-level progress, including parts of a multi-request action.
    // Never copy API error text into a bot-authored comment: it could forge a
    // plan fence or marker under this login.
    let progress = done
        .iter()
        .map(|s| format!("- {s}"))
        .collect::<Vec<_>>()
        .join("\n");
    let record_body = format!("{}\n{dated}: {outcome}\n{progress}", marker(&id));
    send(
        client,
        "PATCH",
        &format!(
            "{API_ROOT}repos/{}/issues/comments/{mine}",
            issue.repository
        ),
        &json!({"body": record_body}),
        0,
    )
    .with_context(|| format!("could not update apply record; claim remains; do not replay. Confirmed writes:\n{progress}"))?;
    let final_epic = read_epic(client, issue).with_context(|| {
        format!("could not read final epic; claim remains. Confirmed writes:\n{progress}")
    })?;
    let status = format!(
        "<!-- {FINGERPRINT_MARKER}: {} -->\n{dated}: {outcome}\n{progress}",
        fingerprint(&final_epic)?
    );
    send(client, "POST", &path, &json!({"body": status}), 0).with_context(|| {
        format!("could not post epic status; claim remains. Confirmed writes:\n{progress}")
    })?;
    result.with_context(|| {
        format!("epic apply stopped; confirmed writes:\n{progress}\nClaim retained; do not replay.")
    })?;
    Ok(request_output().into_iter().chain(done).collect())
}

fn execute<C: Transport + Rest>(
    client: &C,
    plan: &EpicPlan,
    epic: &Epic,
    config: &Config,
    done: &mut Vec<String>,
) -> Result<()> {
    let mut created = BTreeMap::new();
    for mutation in plan.mutations(epic) {
        let n = mutation["action"]
            .as_u64()
            .context("mutation has no action")?;
        if let Some(method) = mutation["method"].as_str() {
            let mut url = mutation["url"]
                .as_str()
                .context("mutation has no URL")?
                .to_owned();
            let mut body = mutation["body"].clone();
            for (k, issue) in &created {
                let issue: &IssueRef = issue;
                // Resolve the entire generated path, never substrings: NEW-1
                // is also a valid repository/owner name outside this action.
                let placeholder = format!("{API_ROOT}repos/{}/issues/NEW-{k}/comments", issue.repo);
                if url == placeholder {
                    url = format!(
                        "{API_ROOT}repos/{}/issues/{}/comments",
                        issue.repo, issue.number
                    );
                } else {
                    continue;
                }
                if method == "POST"
                    && body["body"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("```agent-board-plan\n"))
                {
                    let job = crate::plan::fenced_json(
                        body["body"].as_str().context("job body")?,
                        crate::plan::PLAN_FENCE,
                    )?
                    .context("job fence")?;
                    if job["item"] == format!("NEW-{k}") {
                        let mut job = job;
                        job["item"] = issue.number.into();
                        // A slice stacks only on an earlier one, created by now.
                        if let Some(lower) = job["stacks_on"]
                            .as_str()
                            .and_then(|s| s.strip_prefix("NEW-"))
                        {
                            let lower = created
                                .get(&lower.parse::<u64>().context("stacks_on placeholder")?)
                                .context("stacks_on was not created")?;
                            job["stacks_on"] = lower.number.into();
                        }
                        body["body"] =
                            format!("```{}\n{job:#}\n```\n", crate::plan::PLAN_FENCE).into();
                    }
                }
            }
            let (_, response) = send(client, method, &url, &body, 0)?;
            done.push(format!(
                "Action {n}: {method} {}",
                url.trim_start_matches(API_ROOT)
            ));
            if method == "POST" && url.ends_with("/issues") {
                let repo = url
                    .strip_prefix(&format!("{API_ROOT}repos/"))
                    .and_then(|s| s.strip_suffix("/issues"))
                    .context("created issue repository")?;
                let number = response["number"]
                    .as_u64()
                    .filter(|n| *n > 0)
                    .context("created issue has no number; stop and inspect")?;
                let issue = IssueRef {
                    repo: repo.into(),
                    number,
                };
                created.insert(n, issue.clone());
            }
        } else if mutation["graphql"] == "addSubIssue" {
            let sub = created.get(&n).context("sub-issue was not created")?;
            client.graphql("mutation($issueId: ID!, $subIssueUrl: String!) { addSubIssue(input: {issueId: $issueId, subIssueUrl: $subIssueUrl}) { issue { id } } }", json!({"issueId": epic.id, "subIssueUrl": format!("https://github.com/{}/issues/{}", sub.repo, sub.number)}))?;
            done.push(format!("Action {n}: linked {sub}."));
            set_priority(client, sub, &config.default_priority, config, done, n)?;
        } else {
            let target = mutation["issue"].as_str().context("priority target")?;
            let issue = if let Some((_, k)) = target.rsplit_once("#NEW-") {
                created
                    .get(&k.parse::<u64>()?)
                    .context("priority target was not created")?
                    .clone()
            } else {
                IssueRef::parse(target)?
            };
            set_priority(
                client,
                &issue,
                mutation["value"].as_str().context("priority value")?,
                config,
                done,
                n,
            )?;
        }
    }
    Ok(())
}

fn set_priority<T: Transport>(
    client: &T,
    issue: &IssueRef,
    value: &str,
    config: &Config,
    done: &mut Vec<String>,
    n: u64,
) -> Result<()> {
    let (owner, name) = issue.repo.split_once('/').context("priority repository")?;
    let data = client.graphql("query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { issue(number: $number) { id } } }", json!({"owner": owner, "name": name, "number": issue.number}))?;
    let content = data
        .pointer("/repository/issue/id")
        .and_then(Value::as_str)
        .context("priority target is not an issue")?;
    let data = client.graphql("mutation($project: ID!, $content: ID!) { addProjectV2ItemById(input: {projectId: $project, contentId: $content}) { item { id } } }", json!({"project": config.project, "content": content}))?;
    done.push(format!("Action {n}: enrolled {issue}."));
    let item = data
        .pointer("/addProjectV2ItemById/item/id")
        .and_then(Value::as_str)
        .context("project membership has no id")?;
    let option = config
        .priorities
        .get(value)
        .context("unconfigured Priority")?;
    client.graphql("mutation($project: ID!, $item: ID!, $field: ID!, $option: String!) { updateProjectV2ItemFieldValue(input: {projectId: $project, itemId: $item, fieldId: $field, value: {singleSelectOptionId: $option}}) { projectV2Item { id } } }", json!({"project": config.project, "item": item, "field": config.priority_field_id, "option": option}))?;
    done.push(format!(
        "Action {n}: set {issue} {}={value}.",
        config.priority_field
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epic::EPIC_PLAN_SCHEMA;
    use std::cell::{Cell, RefCell};

    fn config() -> Config {
        Config::from_json(
            &json!({"operator": "operator", "bot": "planner", "repositories": ["example/work"],
            "project": "P_config", "priority_field": "Order", "priority_field_id": "F_config",
            "priorities": {"High": "O_high", "Normal": "O_normal"}, "default_priority": "Normal"}),
        )
        .unwrap()
    }

    fn issue() -> ItemRef {
        crate::github::parse_github_issue("example/epics#1", None, "github.com").unwrap()
    }

    fn comment(id: u64, body: String, editor: Option<&str>) -> Value {
        json!({"url": format!("https://github.com/example/epics/issues/1#issuecomment-{id}"), "body": body,
            "createdAt": "2026-10-14T10:00:00Z", "lastEditedAt": editor.map(|_| "2026-10-14T11:00:00Z"),
            "author": {"login": "planner"}, "editor": editor.map(|login| json!({"login": login}))})
    }

    struct Forge {
        comments: RefCell<Vec<Value>>,
        requests: RefCell<Vec<(String, String, Value)>>,
        created: Cell<bool>,
        closed: Cell<bool>,
        fail: Option<&'static str>,
        foreign_on_recheck: bool,
        edit_on_read: Option<usize>,
        labelled: bool,
        remove_label_on_recheck: bool,
        epic_reads: Cell<usize>,
        reads: Cell<usize>,
        login: &'static str,
    }

    impl Forge {
        fn new(actions: Value) -> Self {
            let plan =
                json!({"schema": EPIC_PLAN_SCHEMA, "epic": "example/epics#1", "actions": actions});
            Self {
                comments: RefCell::new(vec![comment(
                    1,
                    format!("```{EPIC_PLAN_FENCE}\n{plan}\n```"),
                    None,
                )]),
                requests: RefCell::new(vec![]),
                created: Cell::new(false),
                closed: Cell::new(false),
                fail: None,
                foreign_on_recheck: false,
                edit_on_read: None,
                labelled: true,
                remove_label_on_recheck: false,
                epic_reads: Cell::new(0),
                reads: Cell::new(0),
                login: "planner",
            }
        }

        fn full() -> Self {
            Self::new(json!([
                {"type": "create_sub_issue", "repo": "example/work", "title": "child", "body": "scope",
                 "plan": {"schema": crate::plan::JOB_PLAN_SCHEMA, "kind": "implement", "runner": "hosted", "brief": "do it"}},
                {"type": "set_priority", "new": 1, "value": "High"},
                {"type": "rescope_sub_issue", "issue": "example/work#2", "body": "new scope", "reason": "split"},
                {"type": "close_sub_issue", "issue": "example/work#3", "reason": "obsolete"}
            ]))
        }

        fn log(&self, method: &str, target: &str, body: Value) -> Result<()> {
            self.requests
                .borrow_mut()
                .push((method.into(), target.into(), body));
            ensure!(self.fail != Some(target), "injected failure at {target}");
            Ok(())
        }
    }

    impl Transport for Forge {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            if query.contains("viewer") {
                return Ok(json!({"viewer": {"login": self.login}}));
            }
            if query.contains("comments(first") {
                self.reads.set(self.reads.get() + 1);
                if self.edit_on_read == Some(self.reads.get()) {
                    let body = self.comments.borrow()[0]["body"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    self.comments.borrow_mut()[0] = comment(1, body, Some("planner"));
                }
                if self.foreign_on_recheck && self.reads.get() == 2 {
                    let body = self.comments.borrow()[0]["body"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    self.comments.borrow_mut()[0] = comment(1, body, Some("intruder"));
                }
                return Ok(
                    json!({"repository": {"issue": {"comments": {"nodes": *self.comments.borrow(), "pageInfo": {"hasNextPage": false}}}}}),
                );
            }
            if query.contains("subIssues(first") {
                self.epic_reads.set(self.epic_reads.get() + 1);
                ensure!(
                    self.fail != Some("final_epic") || self.epic_reads.get() < 3,
                    "injected final epic read failure"
                );
                let labelled =
                    self.labelled && !(self.remove_label_on_recheck && self.epic_reads.get() >= 2);
                let mut nodes = vec![
                    json!({"number": 2, "state": "OPEN", "repository": {"nameWithOwner": "example/work"}}),
                    json!({"number": 3, "state": if self.closed.get() { "CLOSED" } else { "OPEN" }, "repository": {"nameWithOwner": "example/work"}}),
                ];
                if self.created.get() {
                    nodes.push(json!({"number": 10, "state": "OPEN", "repository": {"nameWithOwner": "example/work"}}));
                }
                return Ok(
                    json!({"repository": {"issue": {"id": "E", "labels": {"nodes": if labelled { vec![json!({"name": "epic"})] } else { vec![] }}, "subIssues": {"nodes": nodes, "pageInfo": {"hasNextPage": false}}}}}),
                );
            }
            if query.starts_with("query") {
                return Ok(
                    json!({"repository": {"issue": {"id": format!("I{}", variables["number"])}}}),
                );
            }
            if query.contains("addSubIssue(") {
                self.log("graphql", "link", variables.clone())?;
                assert_eq!(variables["issueId"], "E");
                assert!(
                    (variables["subIssueUrl"].as_str().unwrap())
                        .starts_with("https://github.com/example/work/issues/1"),
                    "{variables}"
                );
                return Ok(json!({"addSubIssue": {"issue": {"id": "E"}}}));
            }
            if query.contains("addProjectV2ItemById(") {
                self.log("graphql", "enroll", variables.clone())?;
                assert_eq!(variables["project"], "P_config");
                return Ok(json!({"addProjectV2ItemById": {"item": {"id": "M"}}}));
            }
            assert!(query.contains("updateProjectV2ItemFieldValue("), "{query}");
            self.log("graphql", "priority", variables.clone())?;
            assert_eq!(variables["project"], "P_config");
            assert_eq!(variables["field"], "F_config");
            assert_eq!(variables["item"], "M");
            assert!([json!("O_normal"), json!("O_high")].contains(&variables["option"]));
            Ok(json!({"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "M"}}}))
        }
    }

    impl Rest for Forge {
        fn get(&self, _: &str, _: Option<(&str, &str)>) -> Result<crate::poll::RestResponse> {
            anyhow::bail!("unexpected GET")
        }

        fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
            let path = url.strip_prefix(API_ROOT).unwrap();
            if method == "POST"
                && path == "repos/example/epics/issues/1/comments"
                && self.comments.borrow().len() > 1
                && self.fail == Some("status")
            {
                anyhow::bail!("injected status failure");
            }
            self.log(method, path, body.clone())?;
            if method == "POST" && path == "repos/example/work/issues" {
                self.created.set(true);
                // Numbered from 10 in the order created.
                let made = (self.requests.borrow().iter())
                    .filter(|(m, p, _)| m == "POST" && p == path)
                    .count();
                return Ok((201, json!({"number": 9 + made})));
            }
            if method == "POST" && path == "repos/example/epics/issues/1/comments" {
                let id = self.comments.borrow().len() as u64 + 1;
                self.comments.borrow_mut().push(comment(
                    id,
                    body["body"].as_str().unwrap().into(),
                    None,
                ));
                return Ok((201, json!({"id": id})));
            }
            if path == "repos/example/epics/issues/comments/2" {
                self.comments.borrow_mut()[1]["body"] = body["body"].clone();
            }
            if path == "repos/example/work/issues/3" {
                self.closed.set(true);
            }
            Ok((200, json!({})))
        }
    }

    #[test]
    fn preconditions_refuse_without_writes() {
        for case in [
            "match",
            "request",
            "comment",
            "timestamp",
            "missing",
            "edit",
            "replacement",
            "post-claim-edit",
        ] {
            let mut forge = Forge::full();
            let body = forge.comments.borrow()[0]["body"]
                .as_str()
                .unwrap()
                .to_owned();
            let mut value = fenced_json(&body, EPIC_PLAN_FENCE).unwrap().unwrap();
            if case != "missing" {
                value["request"] = json!("checked-1");
            }
            forge.comments.borrow_mut()[0]["body"] =
                json!(format!("```{EPIC_PLAN_FENCE}\n{value}\n```"));
            let expected = Preconditions {
                request: Some(
                    if case == "request" {
                        "other"
                    } else {
                        "checked-1"
                    }
                    .into(),
                ),
                comment: Some(if case == "comment" { 2 } else { 1 }),
                updated_at: Some(
                    OffsetDateTime::parse(
                        if case == "timestamp" {
                            "2026-10-14T09:00:00Z"
                        } else {
                            "2026-10-14T10:00:00Z"
                        },
                        &Rfc3339,
                    )
                    .unwrap(),
                ),
            };
            if case == "edit" {
                forge.edit_on_read = Some(2);
            }
            if case == "post-claim-edit" {
                forge.edit_on_read = Some(3);
            }
            if case == "replacement" {
                forge.comments.borrow_mut().push(comment(
                    2,
                    format!("```{EPIC_PLAN_FENCE}\n{value}\n```"),
                    Some("planner"),
                ));
            }
            let result = apply_checked(
                &forge,
                &issue(),
                &config(),
                OffsetDateTime::UNIX_EPOCH,
                &expected,
            );
            if case == "match" {
                result.unwrap();
                let writes = forge.requests.borrow().len();
                apply_checked(
                    &forge,
                    &issue(),
                    &config(),
                    OffsetDateTime::UNIX_EPOCH,
                    &expected,
                )
                .unwrap();
                assert_eq!(forge.requests.borrow().len(), writes);
            } else {
                let error = result.unwrap_err();
                assert!(
                    error.downcast_ref::<PlanChanged>().is_some(),
                    "{case}: {error:#}"
                );
                if case == "post-claim-edit" {
                    assert!(!forge.created.get());
                    assert!(!forge.closed.get());
                    assert!(
                        forge.comments.borrow()[1]["body"]
                            .as_str()
                            .unwrap()
                            .starts_with("<!-- agent-board-epic-apply: ")
                    );
                } else {
                    assert!(forge.requests.borrow().is_empty(), "{case}");
                }
            }
        }
    }

    #[test]
    fn preview_preconditions_compare_full_instants() {
        for (timestamp, matches) in [
            ("2026-10-14T11:00:00+01:00", true),
            ("2026-10-14T10:00:00.001Z", false),
        ] {
            let forge = Forge::full();
            let expected = Preconditions {
                updated_at: Some(OffsetDateTime::parse(timestamp, &Rfc3339).unwrap()),
                ..Preconditions::default()
            };
            let result = preview_checked(&forge, &issue(), &config(), &expected);
            assert_eq!(result.is_ok(), matches);
            if let Err(error) = result {
                assert!(error.downcast_ref::<PlanChanged>().is_some());
            }
            assert!(forge.requests.borrow().is_empty());
        }
    }

    #[test]
    fn applies_once_with_configured_ids_and_real_job_plan_number() {
        let forge = Forge::full();
        let changes = apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH).unwrap();
        assert_eq!(changes.len(), 11, "{changes:?}");
        let requests = forge.requests.borrow().clone();
        let targets: Vec<_> = requests
            .iter()
            .map(|(_, target, _)| target.as_str())
            .collect();
        assert_eq!(
            targets,
            [
                "repos/example/epics/issues/1/comments",
                "repos/example/work/issues",
                "link",
                "enroll",
                "priority",
                "repos/example/work/issues/10/comments",
                "enroll",
                "priority",
                "repos/example/work/issues/2",
                "repos/example/work/issues/2/comments",
                "repos/example/work/issues/3/comments",
                "repos/example/work/issues/3",
                "repos/example/epics/issues/comments/2",
                "repos/example/epics/issues/1/comments"
            ]
        );
        let job = crate::plan::JobPlan::from_comment(requests[5].2["body"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!((job.repo.as_str(), job.item), ("example/work", 10));
        assert_eq!(requests[8].2, json!({"body": "new scope"}));
        assert_eq!(
            requests[11].2,
            json!({"state": "closed", "state_reason": "not_planned"})
        );
        let comments = forge.comments.borrow();
        let status = comments.last().unwrap()["body"].as_str().unwrap();
        assert!(status.starts_with(&format!("<!-- {FINGERPRINT_MARKER}: ")));
        assert!(status.contains("1970-01-01T00:00:00Z: Applied."));
        assert!(status.contains("[\"example/work#3\",\"CLOSED\"]"));
        drop(comments);
        assert_eq!(
            apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH).unwrap(),
            ["Already claimed; nothing changed."]
        );
        assert_eq!(forge.requests.borrow().len(), requests.len());
    }

    #[test]
    fn stacked_slice_plan_names_the_created_lower_issue() {
        let job = json!({"schema": crate::plan::JOB_PLAN_SCHEMA, "kind": "implement",
            "runner": "hosted", "brief": "do it"});
        let forge = Forge::new(json!([
            {"type": "create_sub_issue", "repo": "example/work", "title": "lower", "body": "a", "plan": job},
            {"type": "create_sub_issue", "repo": "example/work", "title": "upper", "body": "b",
             "plan": job, "stacks_on": 1}
        ]));
        apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH).unwrap();
        let requests = forge.requests.borrow();
        let jobs: Vec<_> = requests
            .iter()
            .filter_map(|(_, target, body)| {
                let job = crate::plan::JobPlan::from_comment(body["body"].as_str()?).unwrap()?;
                Some((target.as_str(), job.item, job.stacks_on))
            })
            .collect();
        assert_eq!(
            jobs,
            [
                ("repos/example/work/issues/10/comments", 10, None),
                ("repos/example/work/issues/11/comments", 11, Some(10)),
            ]
        );
    }

    #[test]
    fn partial_failure_stops_and_retains_claim() {
        let mut forge = Forge::full();
        forge.fail = Some("priority");
        let error = format!(
            "{:#}",
            apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH).unwrap_err()
        );
        assert!(error.contains("confirmed writes:"), "{error}");
        assert!(error.contains("linked example/work#10"), "{error}");
        assert!(!error.contains("set example/work#10"), "{error}");
        assert!(!forge.closed.get());
        let status = forge.comments.borrow().last().unwrap()["body"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(status.contains("Stopped after a failed request"));
        assert!(status.contains("enrolled example/work#10"));
        let writes = forge.requests.borrow().len();
        apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH).unwrap();
        assert_eq!(forge.requests.borrow().len(), writes);
    }

    #[test]
    fn foreign_edit_and_wrong_token_refuse_before_mutations() {
        for when in ["before", "claim", "token"] {
            let mut forge = Forge::full();
            if when == "before" {
                let body = forge.comments.borrow()[0]["body"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                forge.comments.borrow_mut()[0] = comment(1, body, Some("intruder"));
            } else if when == "claim" {
                forge.foreign_on_recheck = true;
            } else {
                forge.login = "operator";
            }
            assert!(apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH).is_err());
            assert!(!forge.created.get());
            assert!(!forge.closed.get());
            if when != "claim" {
                assert!(forge.requests.borrow().is_empty());
            }
        }
    }

    #[test]
    fn request_is_exposed_without_changing_writes() {
        for actions in [
            json!([]),
            json!([{"type": "set_priority", "issue": "example/work#2", "value": "High"}]),
        ] {
            let plain = Forge::new(actions.clone());
            let tagged = Forge::new(actions);
            let mut value = fenced_json(
                tagged.comments.borrow()[0]["body"].as_str().unwrap(),
                EPIC_PLAN_FENCE,
            )
            .unwrap()
            .unwrap();
            value["request"] = json!("Request-123");
            tagged.comments.borrow_mut()[0]["body"] =
                json!(format!("```{EPIC_PLAN_FENCE}\n{value}\n```"));
            let plain_preview = preview(&plain, &issue(), &config()).unwrap();
            let tagged_preview = preview(&tagged, &issue(), &config()).unwrap();
            assert_eq!(tagged_preview[0], json!({"request": "Request-123"}));
            assert_eq!(tagged_preview[1..], plain_preview);
            let now = OffsetDateTime::UNIX_EPOCH;
            let plain_output = apply(&plain, &issue(), &config(), now).unwrap();
            let tagged_output = apply(&tagged, &issue(), &config(), now).unwrap();
            assert_eq!(tagged_output[0], "Request: Request-123");
            assert_eq!(tagged_output[1..], plain_output);
            assert_eq!(*tagged.requests.borrow(), *plain.requests.borrow());
            assert_eq!(
                apply(&tagged, &issue(), &config(), now).unwrap(),
                ["Request: Request-123", "Already claimed; nothing changed."]
            );
        }
    }

    #[test]
    fn preview_is_read_only_and_config_is_strict() {
        let forge = Forge::full();
        let preview = preview(&forge, &issue(), &config()).unwrap();
        assert!(forge.requests.borrow().is_empty());
        assert!(
            preview
                .iter()
                .any(|m| m["field"] == "Order" && m["value"] == "Normal")
        );
        for value in [json!({}), json!({"unknown": true})] {
            assert!(Config::from_json(&value).is_err());
        }
        let mut epic = read_epic(&forge, &issue()).unwrap();
        let first = fingerprint(&epic).unwrap();
        epic.sub_issues.reverse();
        assert_eq!(fingerprint(&epic).unwrap(), first);
        epic.closed.push(epic.sub_issues.pop().unwrap());
        assert_ne!(fingerprint(&epic).unwrap(), first);
    }

    #[test]
    fn placeholder_resolution_never_changes_repository_or_owner_names() {
        let forge = Forge::new(json!([]));
        let mut config = config();
        config
            .bounds
            .repositories
            .extend(["example/NEW-1".into(), "NEW-1/work".into()]);
        let mut epic = read_epic(&forge, &issue()).unwrap();
        epic.sub_issues.extend([
            IssueRef::parse("example/NEW-1#42").unwrap(),
            IssueRef::parse("NEW-1/work#43").unwrap(),
        ]);
        let value = json!({"schema": EPIC_PLAN_SCHEMA, "epic": "example/epics#1", "actions": [
            {"type": "create_sub_issue", "repo": "example/work", "title": "child", "body": "scope"},
            {"type": "rescope_sub_issue", "issue": "example/NEW-1#42", "body": "new scope", "reason": "split"},
            {"type": "rescope_sub_issue", "issue": "NEW-1/work#43", "body": "new scope", "reason": "split"}
        ]});
        let plan = EpicPlan::validate(&value, &config.bounds, &epic).unwrap();
        execute(&forge, &plan, &epic, &config, &mut Vec::new()).unwrap();
        let targets: Vec<_> = forge
            .requests
            .borrow()
            .iter()
            .map(|(_, t, _)| t.clone())
            .collect();
        for expected in [
            "repos/example/NEW-1/issues/42",
            "repos/example/NEW-1/issues/42/comments",
            "repos/NEW-1/work/issues/43",
            "repos/NEW-1/work/issues/43/comments",
        ] {
            assert!(targets.iter().any(|t| t == expected), "{targets:?}");
        }
        assert!(
            !targets
                .iter()
                .any(|t| t.contains("example/10/") || t.contains("repos/10/"))
        );
    }

    #[test]
    fn only_operator_designated_epics_may_be_decomposed() {
        for before in [true, false] {
            let mut forge = Forge::full();
            forge.labelled = !before;
            forge.remove_label_on_recheck = !before;
            let error = format!(
                "{:#}",
                apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH).unwrap_err()
            );
            assert!(error.contains("operator's epic label"), "{error}");
            assert!(!forge.created.get());
            if before {
                assert!(forge.requests.borrow().is_empty());
                assert!(preview(&forge, &issue(), &config()).is_err());
            }
        }
    }

    #[test]
    fn failure_at_each_stage_stops_action_writes_and_retains_claim() {
        for (target, action_requests) in [
            ("repos/example/work/issues", 1),
            ("link", 2),
            ("enroll", 3),
            ("priority", 4),
            ("repos/example/work/issues/10/comments", 5),
            ("repos/example/work/issues/2", 8),
            ("repos/example/work/issues/2/comments", 9),
            ("repos/example/work/issues/3/comments", 10),
            ("repos/example/work/issues/3", 11),
            ("repos/example/epics/issues/comments/2", 11),
            ("final_epic", 11),
            ("status", 11),
        ] {
            let mut forge = Forge::full();
            forge.fail = Some(target);
            let error = format!(
                "{:#}",
                apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH).unwrap_err()
            );
            assert!(
                error.contains("injected failure")
                    || error.contains("injected final")
                    || error.contains("injected status"),
                "{target}: {error}"
            );
            let actual = forge
                .requests
                .borrow()
                .iter()
                .filter(|(_, path, _)| !path.starts_with("repos/example/epics/"))
                .count();
            assert_eq!(actual, action_requests, "{target}");
            assert!(
                forge.comments.borrow()[1]["body"]
                    .as_str()
                    .unwrap()
                    .starts_with(&format!("<!-- {APPLY_MARKER}: "))
            );
            let requests = forge.requests.borrow().len();
            apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH).unwrap();
            assert_eq!(forge.requests.borrow().len(), requests, "{target}");
        }
    }
}
