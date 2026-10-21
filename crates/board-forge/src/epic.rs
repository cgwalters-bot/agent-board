//! Epic plans: a coordinator's proposed changes to an epic's sub-issues,
//! written as a fenced block in a comment on the epic (slice 2 of
//! cgwalters-forge/tracker#572), and the GitHub mutations they would make.
//! Schema: docs/epic-plans.md.
//!
//! Any run that comments as the planner can write a plan, so the bounds
//! checked here, which come from the caller's configuration, are what limit
//! it. Applying is opt-in and records a claim before making changes.
use crate::github::{API_ROOT, DEFAULT_HOST, Transport};
use crate::plan::{Comment, JobPlan, fenced_json, issue_comments, newest_fenced};
use crate::poll::validate_repository;
use anyhow::{Context, Result, bail, ensure};
use board_core::ItemRef;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fmt;

pub const EPIC_PLAN_SCHEMA: &str = "agent-board/epic-plan/v1";
/// The info string of the fence that holds an epic plan.
pub const EPIC_PLAN_FENCE: &str = "agent-board-epic-plan";
/// The label that makes an issue an epic. Only the operator applies it: a
/// plan that could would let the planner start epics of its own.
pub const EPIC_LABEL: &str = "epic";
pub const PRIORITY_FIELD: &str = "Priority";
/// Actions in one plan, so a runaway planner cannot file issues by the dozen.
pub const ACTION_LIMIT: usize = 20;
/// Characters of a title, GitHub's own limit.
pub const TITLE_LIMIT: usize = 256;
/// Characters of a body; GitHub refuses bodies over 65,536.
pub const BODY_LIMIT: usize = 60_000;
/// Characters of the reason posted when closing or re-scoping.
pub const REASON_LIMIT: usize = 4_000;
pub const REQUEST_LIMIT: usize = 128;
const LABEL_LIMIT: usize = 10;
/// GitHub allows 100 sub-issues per parent, so they fit in one page.
pub const SUB_ISSUE_LIMIT: usize = 100;

const SUB_ISSUES_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      id
      state
      labels(first: 100) { nodes { name } }
      subIssues(first: 100) {
        nodes { number state repository { nameWithOwner } }
        pageInfo { hasNextPage }
      }
    }
  }
}"#;

/// An issue, OWNER/REPO#N.
#[derive(Clone, Debug, PartialEq)]
pub struct IssueRef {
    pub repo: String,
    pub number: u64,
}

impl IssueRef {
    pub fn parse(text: &str) -> Result<Self> {
        let (repo, number) = text
            .split_once('#')
            .with_context(|| format!("{text:?} is not OWNER/REPO#N"))?;
        validate_repository(repo)?;
        ensure!(
            number.starts_with(|c: char| ('1'..='9').contains(&c))
                && number.bytes().all(|c| c.is_ascii_digit()),
            "{text:?} has no positive issue number"
        );
        Ok(Self {
            repo: repo.into(),
            number: number.parse().context("issue number")?,
        })
    }

    /// GitHub names repositories without regard to case.
    fn same(&self, other: &Self) -> bool {
        self.number == other.number && self.repo.eq_ignore_ascii_case(&other.repo)
    }
}

impl fmt::Display for IssueRef {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}#{}", self.repo, self.number)
    }
}

/// What an action on the board applies to: an existing issue, or the issue
/// an earlier create action in the same plan makes (its 1-based position).
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Issue(IssueRef),
    New(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Create {
        repo: String,
        title: String,
        body: String,
        labels: Vec<String>,
        /// Posted on the new issue; its `item` is 0 until the issue exists.
        plan: Option<JobPlan>,
        /// The slice in the same repository whose pull request the plan's
        /// run builds on ([`crate::stack`]); posted as the plan's `stacks_on`.
        stacks_on: Option<Target>,
    },
    Close {
        issue: IssueRef,
        reason: String,
    },
    /// A close whose sub-issue closed after the plan was written, typically
    /// by its merged pull request. Satisfied, so it makes no mutations; kept
    /// in place so that later actions' `new` positions still line up.
    AlreadyClosed {
        issue: IssueRef,
    },
    Rescope {
        issue: IssueRef,
        body: String,
        reason: String,
    },
    SetPriority {
        target: Target,
        value: String,
    },
}

/// Action types, with the fields each takes besides `type`.
const ACTIONS: &[(&str, &[&str])] = &[
    (
        "create_sub_issue",
        &["repo", "title", "body", "labels", "plan", "stacks_on"],
    ),
    ("close_sub_issue", &["issue", "reason"]),
    ("rescope_sub_issue", &["issue", "body", "reason"]),
    ("set_priority", &["issue", "new", "value"]),
];

/// What a plan may touch, from the caller's own configuration (never from a
/// plan or a dispatcher's inputs). Empty bounds allow nothing.
#[derive(Clone, Debug, Default)]
pub struct Bounds {
    /// Repositories, OWNER/NAME, whose issues a plan may create or change.
    pub repositories: Vec<String>,
    /// The Priority field's options.
    pub priorities: Vec<String>,
}

impl Bounds {
    pub fn validate(&self) -> Result<()> {
        for repo in &self.repositories {
            validate_repository(repo).context("invalid configured repository")?;
        }
        for option in &self.priorities {
            ensure!(
                !option.trim().is_empty(),
                "empty configured Priority option"
            );
        }
        Ok(())
    }
}

/// The epic as GitHub has it now.
#[derive(Clone, Debug)]
pub struct Epic {
    pub issue: IssueRef,
    /// Its node ID, which addSubIssue takes.
    pub id: String,
    /// Positive evidence of the operator's epic designation. Missing/redacted
    /// labels are not authority to apply a decomposition.
    pub designated: bool,
    /// Whether the epic itself is open. Closing it is the operator's decision
    /// that the decomposition is finished or dropped, so a plan written before
    /// may not change its sub-issues.
    pub open: bool,
    /// Its open sub-issues, the only ones a plan may change.
    pub sub_issues: Vec<IssueRef>,
    /// Its closed sub-issues: closed by the operator or by finished work, so
    /// a plan may not change or revive them.
    pub closed: Vec<IssueRef>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EpicPlan {
    pub request: Option<String>,
    pub actions: Vec<Action>,
}

/// Correlation metadata only, never authority or a replay key. Restrict it to
/// bounded ASCII so exposing it cannot inject output lines or comment markup.
pub(crate) fn request(value: &Value) -> Result<Option<String>> {
    let Some(value) = value.get("request") else {
        return Ok(None);
    };
    let value = value.as_str().context("request is not a string")?;
    ensure!(
        !value.is_empty() && value.len() <= REQUEST_LIMIT,
        "request must have 1 to {REQUEST_LIMIT} characters"
    );
    ensure!(
        value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-'),
        "request must contain only ASCII letters, digits and hyphens"
    );
    Ok(Some(value.to_owned()))
}

fn text<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    object
        .get(key)
        .with_context(|| format!("no {key}"))?
        .as_str()
        .with_context(|| format!("{key} is not a string"))
}

/// A non-blank string field of at most `limit` characters.
fn bounded<'a>(object: &'a Map<String, Value>, key: &str, limit: usize) -> Result<&'a str> {
    let value = text(object, key)?;
    ensure!(!value.trim().is_empty(), "{key} is empty");
    ensure!(
        value.chars().count() <= limit,
        "{key} exceeds {limit} characters"
    );
    Ok(value)
}

impl EpicPlan {
    /// Validate one plan object against `bounds` and the epic it was found on.
    /// Errors in the plan as a whole stop at the first; each action is checked
    /// in full and every refused action named, so a planner sees all at once.
    pub fn validate(value: &Value, bounds: &Bounds, epic: &Epic) -> Result<Self> {
        let object = value
            .as_object()
            .context("epic plan is not a JSON object")?;
        for key in object.keys() {
            ensure!(
                ["schema", "epic", "actions", "request"].contains(&key.as_str()),
                "unknown epic plan field {key:?}"
            );
        }
        let field = |key| text(object, key).with_context(|| "epic plan".to_owned());
        ensure!(
            field("schema")? == EPIC_PLAN_SCHEMA,
            "unsupported epic plan schema (expected {EPIC_PLAN_SCHEMA})"
        );
        let named = IssueRef::parse(field("epic")?).context("epic plan epic")?;
        ensure!(
            named.same(&epic.issue),
            "epic plan is for {named}, but was found on {}",
            epic.issue
        );
        // Here rather than in each caller, so the dry run, the preview and
        // apply (before and after its claim) refuse alike.
        ensure!(
            epic.open,
            "epic is closed; a plan may not change a closed epic's sub-issues"
        );
        let actions = object
            .get("actions")
            .context("epic plan has no actions")?
            .as_array()
            .context("epic plan actions is not a list")?;
        ensure!(
            actions.len() <= ACTION_LIMIT,
            "epic plan has more than {ACTION_LIMIT} actions"
        );
        let mut checker = Checker {
            bounds,
            epic,
            done: Vec::new(),
            seen: BTreeMap::new(),
        };
        let mut errors = Vec::new();
        for (index, value) in actions.iter().enumerate() {
            let kind = value["type"].as_str().unwrap_or("?");
            match checker.action(value) {
                Ok(action) => checker.done.push(Some(action)),
                Err(error) => {
                    errors.push(format!("action {} ({kind}): {error:#}", index + 1));
                    checker.done.push(None);
                }
            }
        }
        if !errors.is_empty() {
            bail!("epic plan refused:\n  {}", errors.join("\n  "));
        }
        Ok(Self {
            request: request(value)?,
            actions: checker.done.into_iter().flatten().collect(),
        })
    }

    /// The GitHub mutations applying the plan would make, in order, one JSON
    /// object each, tagged with its action's 1-based position. Where the
    /// number of an issue the plan creates belongs, they say `NEW-k`. A
    /// project field is named by field and option rather than node IDs, which
    /// apply resolves (adding a new issue to the project first).
    pub fn mutations(&self, epic: &Epic) -> Vec<Value> {
        let rest = |n: usize, method: &str, path: String, body: Value| json!({"action": n, "method": method, "url": format!("{API_ROOT}{path}"), "body": body});
        let field = |n: usize, issue: String, field: &str, value: &str| {
            json!({"action": n, "graphql": "updateProjectV2ItemFieldValue",
                "issue": issue, "field": field, "value": value})
        };
        let target = |target: &Target| match target {
            Target::Issue(issue) => issue.to_string(),
            Target::New(k) => match k.checked_sub(1).and_then(|i| self.actions.get(i)) {
                Some(Action::Create { repo, .. }) => format!("{repo}#NEW-{k}"),
                _ => format!("NEW-{k}"),
            },
        };
        let mut out = Vec::new();
        for (index, action) in self.actions.iter().enumerate() {
            let n = index + 1;
            match action {
                Action::Create {
                    repo,
                    title,
                    body,
                    labels,
                    plan,
                    stacks_on,
                } => {
                    let mut issue = json!({"title": title, "body": body});
                    if !labels.is_empty() {
                        issue["labels"] = json!(labels);
                    }
                    out.push(rest(n, "POST", format!("repos/{repo}/issues"), issue));
                    out.push(json!({"action": n, "graphql": "addSubIssue", "variables": {
                        "issueId": epic.id,
                        "subIssueUrl": format!("https://{DEFAULT_HOST}/{repo}/issues/NEW-{n}"),
                    }}));
                    if let Some(plan) = plan {
                        let mut fields = json!({"schema": crate::plan::JOB_PLAN_SCHEMA,
                            "repo": plan.repo, "item": format!("NEW-{n}"), "kind": plan.kind,
                            "runner": plan.runner, "brief": plan.brief});
                        if !plan.pool.is_empty() {
                            fields["pool"] = json!(plan.pool);
                        }
                        // Apply puts the created issue's number in place of NEW-k.
                        match stacks_on {
                            Some(Target::Issue(lower)) => fields["stacks_on"] = lower.number.into(),
                            Some(Target::New(k)) => fields["stacks_on"] = format!("NEW-{k}").into(),
                            None => {}
                        }
                        let fence = crate::plan::PLAN_FENCE;
                        out.push(rest(
                            n,
                            "POST",
                            format!("repos/{repo}/issues/NEW-{n}/comments"),
                            json!({"body": format!("```{fence}\n{fields:#}\n```\n")}),
                        ));
                    }
                }
                Action::Close { issue, reason } => {
                    let path = format!("repos/{}/issues/{}", issue.repo, issue.number);
                    out.push(rest(
                        n,
                        "POST",
                        format!("{path}/comments"),
                        json!({"body": reason}),
                    ));
                    // A coordinator closes what it drops; finished work closes
                    // through its pull request.
                    out.push(rest(
                        n,
                        "PATCH",
                        path,
                        json!({"state": "closed", "state_reason": "not_planned"}),
                    ));
                }
                // Neither comment nor re-close: the reason was written for an
                // open issue, and closing again would change how it closed.
                // Listed, not sent, so a dry run and apply's status say so.
                Action::AlreadyClosed { issue } => out
                    .push(json!({"action": n, "satisfied": format!("{issue} is already closed")})),
                Action::Rescope {
                    issue,
                    body,
                    reason,
                } => {
                    let path = format!("repos/{}/issues/{}", issue.repo, issue.number);
                    out.push(rest(n, "PATCH", path.clone(), json!({"body": body})));
                    out.push(rest(
                        n,
                        "POST",
                        format!("{path}/comments"),
                        json!({"body": reason}),
                    ));
                }
                Action::SetPriority { target: t, value } => {
                    out.push(field(n, target(t), PRIORITY_FIELD, value));
                }
            }
        }
        out
    }

    /// JSON preview: correlation metadata followed by the proposed mutations.
    /// Keep metadata out of `mutations`, which is also used by the executor.
    pub fn preview(&self, epic: &Epic) -> Vec<Value> {
        self.request
            .iter()
            .map(|id| json!({"request": id}))
            .chain(self.mutations(epic))
            .collect()
    }
}

/// The action type after which nothing else may touch its issue.
const CLOSE: &str = "close_sub_issue";

/// Checks one action at a time against the bounds and the actions before it.
struct Checker<'a> {
    bounds: &'a Bounds,
    epic: &'a Epic,
    /// The actions so far; `None` for a refused one.
    done: Vec<Option<Action>>,
    /// Each (type, issue) acted on, and the action that did; a slice stacked
    /// on a current sub-issue counts under the pseudo-type `stacks_on`.
    seen: BTreeMap<(String, String), usize>,
}

impl Checker<'_> {
    fn repository(&self, repo: &str) -> Result<()> {
        ensure!(
            self.bounds
                .repositories
                .iter()
                .any(|r| r.eq_ignore_ascii_case(repo)),
            "repository {repo} is not in the configured list"
        );
        Ok(())
    }

    /// An existing issue an action may change: an open sub-issue in a
    /// configured repository, never the epic.
    fn sub_issue(&self, text: &str) -> Result<IssueRef> {
        let issue = IssueRef::parse(text)?;
        ensure!(
            !issue.same(&self.epic.issue),
            "{issue} is the epic itself; a plan may not change the epic's text, priority or labels"
        );
        ensure!(
            !self.epic.closed.iter().any(|s| s.same(&issue)),
            "{issue} is a closed sub-issue; a plan may not change it"
        );
        // Spelled as GitHub has it, not as the plan does.
        let issue = self
            .epic
            .sub_issues
            .iter()
            .find(|s| s.same(&issue))
            .with_context(|| {
                format!(
                    "{issue} is neither new nor a current sub-issue of {}",
                    self.epic.issue
                )
            })?;
        self.repository(&issue.repo)?;
        Ok(issue.clone())
    }

    /// A closed sub-issue of the epic, spelled as GitHub has it, if `text` is one.
    fn closed_sub_issue(&self, text: &str) -> Result<Option<IssueRef>> {
        let issue = IssueRef::parse(text)?;
        Ok(self.epic.closed.iter().find(|s| s.same(&issue)).cloned())
    }

    fn target(&self, object: &Map<String, Value>) -> Result<Target> {
        match (object.get("issue"), object.get("new")) {
            (Some(_), None) => Ok(Target::Issue(self.sub_issue(text(object, "issue")?)?)),
            (None, Some(new)) => {
                let k = new
                    .as_u64()
                    .and_then(|k| usize::try_from(k).ok())
                    .filter(|k| (1..=self.done.len()).contains(k))
                    .context("new is not the position of an earlier action")?;
                match &self.done[k - 1] {
                    Some(Action::Create { .. }) => Ok(Target::New(k)),
                    Some(_) => bail!("new names action {k}, which does not create a sub-issue"),
                    None => bail!("new names action {k}, which is refused"),
                }
            }
            _ => bail!("needs exactly one of issue or new"),
        }
    }

    /// The slice a new one in `repo` stacks on: an open sub-issue (`OWNER/REPO#N`)
    /// or the position of an earlier create action, in the same repository,
    /// since a pull request's base is a branch of its own repository.
    fn lower(&self, value: &Value, repo: &str) -> Result<Target> {
        let (lower, in_repo) = match value {
            Value::String(issue) => {
                let issue = self.sub_issue(issue).context("stacks_on")?;
                let in_repo = issue.repo.clone();
                (Target::Issue(issue), in_repo)
            }
            Value::Number(k) => {
                let k = k
                    .as_u64()
                    .and_then(|k| usize::try_from(k).ok())
                    .filter(|k| (1..=self.done.len()).contains(k))
                    .context("stacks_on is not the position of an earlier action")?;
                match &self.done[k - 1] {
                    Some(Action::Create {
                        repo,
                        plan: Some(plan),
                        ..
                    }) if plan.kind == "implement" => (Target::New(k), repo.clone()),
                    Some(_) => bail!(
                        "stacks_on names action {k}, which does not create a sub-issue with an implement plan"
                    ),
                    None => bail!("stacks_on names action {k}, which is refused"),
                }
            }
            _ => bail!("stacks_on is neither OWNER/REPO#N nor the position of an earlier action"),
        };
        ensure!(
            in_repo.eq_ignore_ascii_case(repo),
            "stacks_on is in {in_repo}, not {repo}"
        );
        Ok(lower)
    }

    /// One action of a type on any one issue, and nothing else on an issue the
    /// plan closes: either would repeat or contradict.
    fn once(&mut self, kind: &str, key: String) -> Result<()> {
        let clash = self
            .seen
            .iter()
            .find(|((k, issue), _)| *issue == key && (k == kind || k == CLOSE || kind == CLOSE));
        if let Some((_, earlier)) = clash {
            bail!("{key} is already the subject of action {earlier}");
        }
        self.seen.insert((kind.into(), key), self.done.len() + 1);
        Ok(())
    }

    /// A new slice stacks on the open sub-issue `key`: counted as an action on
    /// it, so a close of it in the same plan clashes in either order (`once`
    /// refuses anything on an issue the plan closes). Otherwise apply would
    /// abandon the pull request the slice's brief builds on, and dispatch
    /// would find the lower issue closed and start the run unstacked
    /// (agent-board#207). A rescope or priority of the lower slice, or another
    /// slice stacking on it, stays allowed: none of those contradicts.
    fn stacks_on(&mut self, key: String) -> Result<()> {
        const KIND: &str = "stacks_on";
        let closed = self
            .seen
            .iter()
            .find(|((k, issue), _)| *issue == key && k == CLOSE);
        if let Some((_, earlier)) = closed {
            bail!("{key} is already the subject of action {earlier}");
        }
        self.seen
            .entry((KIND.into(), key))
            .or_insert(self.done.len() + 1);
        Ok(())
    }

    fn action(&mut self, value: &Value) -> Result<Action> {
        let object = value.as_object().context("action is not a JSON object")?;
        let kind = text(object, "type")?;
        let (_, fields) = ACTIONS
            .iter()
            .find(|(name, _)| *name == kind)
            .with_context(|| format!("unknown action type {kind:?}"))?;
        for key in object.keys() {
            ensure!(
                key == "type" || fields.contains(&key.as_str()),
                "unknown field {key:?}"
            );
        }
        let target_key = |target: &Target| match target {
            Target::Issue(issue) => issue.to_string().to_ascii_lowercase(),
            Target::New(k) => format!("NEW-{k}"),
        };
        Ok(match kind {
            "create_sub_issue" => {
                let repo = text(object, "repo")?;
                validate_repository(repo)?;
                self.repository(repo)?;
                let labels = match object.get("labels") {
                    None => Vec::new(),
                    Some(labels) => labels
                        .as_array()
                        .context("labels is not a list")?
                        .iter()
                        .map(|l| {
                            let label = l.as_str().context("a label is not a string")?;
                            ensure!(!label.trim().is_empty(), "a label is empty");
                            ensure!(
                                !label.trim().eq_ignore_ascii_case(EPIC_LABEL),
                                "the {EPIC_LABEL:?} label is the operator's to apply"
                            );
                            Ok(label.to_owned())
                        })
                        .collect::<Result<Vec<_>>>()?,
                };
                ensure!(
                    labels.len() <= LABEL_LIMIT,
                    "more than {LABEL_LIMIT} labels"
                );
                let plan = match object.get("plan") {
                    None => None,
                    Some(plan) => Some(job_plan(plan, repo).context("plan")?),
                };
                let stacks_on = match object.get("stacks_on") {
                    None => None,
                    Some(lower) => {
                        ensure!(
                            plan.as_ref().is_some_and(|p| p.kind == "implement"),
                            "stacks_on needs an implement plan, whose run opens the pull request"
                        );
                        Some(self.lower(lower, repo)?)
                    }
                };
                let action = Action::Create {
                    repo: repo.into(),
                    title: bounded(object, "title", TITLE_LIMIT)?.into(),
                    body: bounded(object, "body", BODY_LIMIT)?.into(),
                    labels,
                    plan,
                    stacks_on,
                };
                // Last, so that only an action that passes counts. A new lower
                // slice cannot be closed by this plan, so only a current one
                // is recorded.
                if let Action::Create {
                    stacks_on: Some(lower @ Target::Issue(_)),
                    ..
                } = &action
                {
                    self.stacks_on(target_key(lower))?;
                }
                action
            }
            "close_sub_issue" | "rescope_sub_issue" => {
                let named = text(object, "issue")?;
                // A plan goes stale when a sub-issue it closes closes first
                // (tracker#596: its pull request merged); refusing would block
                // the epic's every later pass. Only a close is satisfied so;
                // anything else on a closed sub-issue still revives it.
                let stale = match kind {
                    "close_sub_issue" => self.closed_sub_issue(named)?,
                    _ => None,
                };
                let issue = match &stale {
                    // Still bounded as an open one would be.
                    Some(issue) => {
                        self.repository(&issue.repo)?;
                        issue.clone()
                    }
                    None => self.sub_issue(named)?,
                };
                let reason: String = bounded(object, "reason", REASON_LIMIT)?.into();
                // The reason is posted as a comment by the applier, who is a
                // plan writer: a fence or hidden marker in it would be a job
                // plan, an epic plan or a dispatch record under that login.
                ensure!(
                    !reason.contains("```agent-board") && !reason.contains("<!--"),
                    "reason holds an agent-board fence or a hidden marker"
                );
                let action = if stale.is_some() {
                    Action::AlreadyClosed {
                        issue: issue.clone(),
                    }
                } else if kind == "close_sub_issue" {
                    Action::Close {
                        issue: issue.clone(),
                        reason,
                    }
                } else {
                    Action::Rescope {
                        issue: issue.clone(),
                        body: bounded(object, "body", BODY_LIMIT)?.into(),
                        reason,
                    }
                };
                // Last, so that only an action that passes counts.
                self.once(kind, target_key(&Target::Issue(issue)))?;
                action
            }
            "set_priority" => {
                let target = self.target(object)?;
                let value = text(object, "value")?;
                ensure!(
                    self.bounds.priorities.iter().any(|p| p == value),
                    "Priority {value:?} is not one of the field's options ({})",
                    self.bounds.priorities.join(", ")
                );
                self.once(kind, target_key(&target))?;
                Action::SetPriority {
                    target,
                    value: value.into(),
                }
            }
            _ => bail!("unknown action type {kind:?}"),
        })
    }
}

/// A job plan for an issue a plan creates. Its repository and number are the
/// new issue's, so it must not name them; the rest is checked as any job plan is.
fn job_plan(value: &Value, repo: &str) -> Result<JobPlan> {
    let mut object = value.as_object().context("is not a JSON object")?.clone();
    for key in ["repo", "item"] {
        ensure!(
            !object.contains_key(key),
            "names {key}, which is the new issue's"
        );
    }
    ensure!(
        !object.contains_key("stacks_on"),
        "names stacks_on, which the action names, as a slice of the epic"
    );
    object.insert("repo".into(), repo.into());
    // A placeholder that passes the number check; reset below.
    object.insert("item".into(), 1.into());
    let mut plan = JobPlan::from_json(&Value::Object(object))?;
    plan.item = 0;
    Ok(plan)
}

/// The epic and its current sub-issues.
pub fn read_epic<T: Transport>(transport: &T, issue: &ItemRef) -> Result<Epic> {
    ensure!(
        issue.host == DEFAULT_HOST,
        "epic plans are read only from github.com"
    );
    validate_repository(&issue.repository)?;
    let (owner, name) = issue
        .repository
        .split_once('/')
        .context("issue repository")?;
    let data = transport.graphql(
        SUB_ISSUES_QUERY,
        json!({"owner": owner, "name": name, "number": issue.number}),
    )?;
    let node = data
        .pointer("/repository/issue")
        .filter(|c| c.is_object())
        .with_context(|| format!("{} is not an issue", issue.display()))?;
    let id = node["id"].as_str().context("the epic has no node id")?;
    let connection = &node["subIssues"];
    ensure!(
        connection["pageInfo"]["hasNextPage"] == false,
        "{} has more than {SUB_ISSUE_LIMIT} sub-issues, or GitHub did not say",
        issue.display()
    );
    let mut epic = Epic {
        issue: IssueRef {
            repo: issue.repository.clone(),
            number: issue.number,
        },
        id: id.into(),
        designated: node["labels"]["nodes"]
            .as_array()
            .is_some_and(|labels| labels.iter().any(|l| l["name"] == EPIC_LABEL)),
        open: match node["state"].as_str() {
            Some("OPEN") => true,
            Some("CLOSED") => false,
            _ => bail!("{} has no state", issue.display()),
        },
        sub_issues: Vec::new(),
        closed: Vec::new(),
    };
    for node in connection["nodes"]
        .as_array()
        .context("sub-issues have no nodes")?
    {
        let repo = node["repository"]["nameWithOwner"]
            .as_str()
            .context("sub-issue has no repository")?;
        validate_repository(repo)?;
        let sub_issue = IssueRef {
            repo: repo.into(),
            number: node["number"]
                .as_u64()
                .filter(|n| *n > 0)
                .context("sub-issue has no number")?,
        };
        match node["state"].as_str() {
            Some("OPEN") => epic.sub_issues.push(sub_issue),
            Some("CLOSED") => epic.closed.push(sub_issue),
            _ => bail!("sub-issue {sub_issue} has no state"),
        }
    }
    Ok(epic)
}

/// The newest accepted epic plan on `issue`, checked, with the comment it
/// came from. Which comment counts is decided as for job plans
/// ([`crate::plan::newest_plan`]): writers only, newest wins, and a foreign
/// edit or an invalid newest plan is an error rather than a fallback.
pub fn newest_epic_plan<T: Transport>(
    transport: &T,
    issue: &ItemRef,
    writers: &[&str],
    bounds: &Bounds,
) -> Result<(EpicPlan, Epic, Comment)> {
    bounds.validate()?;
    let comments = issue_comments(transport, issue)?;
    let newest = newest_fenced(&comments, writers, EPIC_PLAN_FENCE)?;
    let value = fenced_json(&newest.body, EPIC_PLAN_FENCE)
        .with_context(|| format!("newest epic plan {}", newest.url))?
        .context("epic plan fence vanished")?;
    let epic = read_epic(transport, issue)?;
    let plan = EpicPlan::validate(&value, bounds, &epic)
        .with_context(|| format!("newest epic plan {}", newest.url))?;
    Ok((plan, epic, newest.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::JOB_PLAN_SCHEMA;

    const EPIC: &str = "cgwalters-forge/tracker#572";

    fn epic() -> Epic {
        Epic {
            issue: IssueRef::parse(EPIC).unwrap(),
            id: "I_epic".into(),
            designated: true,
            open: true,
            sub_issues: vec![
                IssueRef::parse("cgwalters-forge/agent-board#63").unwrap(),
                IssueRef::parse("cgwalters-forge/tracker#573").unwrap(),
                IssueRef::parse("elsewhere/repo#1").unwrap(),
            ],
            closed: vec![
                IssueRef::parse("cgwalters-forge/tracker#574").unwrap(),
                IssueRef::parse("elsewhere/repo#2").unwrap(),
            ],
        }
    }

    fn bounds() -> Bounds {
        Bounds {
            repositories: vec![
                "cgwalters-forge/tracker".into(),
                "cgwalters-forge/agent-board".into(),
            ],
            priorities: vec!["P0".into(), "P1".into(), "P2".into()],
        }
    }

    fn create() -> Value {
        json!({"type": "create_sub_issue", "repo": "cgwalters-forge/agent-board",
            "title": "epic apply", "body": "Apply epic plans.",
            "plan": {"schema": JOB_PLAN_SCHEMA, "kind": "implement", "runner": "hosted",
                "brief": "Implement epic apply."}})
    }

    fn plan(actions: Vec<Value>) -> Value {
        json!({"schema": EPIC_PLAN_SCHEMA, "epic": EPIC, "actions": actions})
    }

    fn valid() -> Vec<Value> {
        vec![
            create(),
            json!({"type": "set_priority", "new": 1, "value": "P1"}),
            json!({"type": "close_sub_issue", "issue": "cgwalters-forge/tracker#573",
                "reason": "Folded into agent-board#63."}),
            json!({"type": "rescope_sub_issue", "issue": "CGWalters-Forge/agent-board#63",
                "body": "Schema and dry run only.", "reason": "Apply is its own slice."}),
            json!({"type": "set_priority", "issue": "cgwalters-forge/agent-board#63", "value": "P0"}),
        ]
    }

    #[test]
    fn request_validation_table() {
        for (request, valid) in [
            (None, true),
            (Some(json!("Az09-request")), true),
            (Some(json!("a".repeat(REQUEST_LIMIT))), true),
            (Some(json!("")), false),
            (Some(json!("a".repeat(REQUEST_LIMIT + 1))), false),
            (Some(json!("has_underscore")), false),
            (Some(json!("has space")), false),
            (Some(json!("id\n")), false),
            (Some(json!("é")), false),
            (Some(Value::Null), false),
            (Some(json!(123)), false),
        ] {
            let mut value = plan(vec![]);
            if let Some(request) = &request {
                value["request"] = request.clone();
            }
            let checked = EpicPlan::validate(&value, &bounds(), &epic());
            assert_eq!(checked.is_ok(), valid, "{value}: {checked:?}");
            if let Ok(checked) = checked {
                assert_eq!(
                    checked.request.as_deref(),
                    request.as_ref().and_then(Value::as_str)
                );
                let expected: Vec<Value> = request
                    .into_iter()
                    .map(|id| json!({"request": id}))
                    .collect();
                assert_eq!(checked.preview(&epic()), expected);
                assert!(checked.mutations(&epic()).is_empty());
            }
        }
        let schema: Value =
            serde_json::from_str(include_str!("../../../docs/epic-plans.schema.json")).unwrap();
        assert_eq!(schema["properties"]["request"]["maxLength"], REQUEST_LIMIT);
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn valid_plan_and_its_mutations() {
        let epic = epic();
        let checked = EpicPlan::validate(&plan(valid()), &bounds(), &epic).unwrap();
        assert_eq!(checked.actions.len(), 5);
        let Action::Create {
            plan: Some(job), ..
        } = &checked.actions[0]
        else {
            panic!("{:?}", checked.actions[0])
        };
        assert_eq!(
            (job.repo.as_str(), job.item),
            ("cgwalters-forge/agent-board", 0)
        );
        let mutations = checked.mutations(&epic);
        let summary: Vec<String> = mutations
            .iter()
            .map(|m| {
                let what = match m["graphql"].as_str() {
                    Some("addSubIssue") => format!("addSubIssue {}", m["variables"]["subIssueUrl"]),
                    Some(op) => format!("{op} {} {}={}", m["issue"], m["field"], m["value"]),
                    None => format!("{} {}", m["method"], m["url"]),
                };
                format!("{} {}", m["action"], what.replace('"', ""))
            })
            .collect();
        let api = "https://api.github.com/repos";
        assert_eq!(
            summary,
            [
                format!("1 POST {api}/cgwalters-forge/agent-board/issues"),
                "1 addSubIssue https://github.com/cgwalters-forge/agent-board/issues/NEW-1".into(),
                format!("1 POST {api}/cgwalters-forge/agent-board/issues/NEW-1/comments"),
                "2 updateProjectV2ItemFieldValue cgwalters-forge/agent-board#NEW-1 Priority=P1"
                    .into(),
                format!("3 POST {api}/cgwalters-forge/tracker/issues/573/comments"),
                format!("3 PATCH {api}/cgwalters-forge/tracker/issues/573"),
                format!("4 PATCH {api}/cgwalters-forge/agent-board/issues/63"),
                format!("4 POST {api}/cgwalters-forge/agent-board/issues/63/comments"),
                "5 updateProjectV2ItemFieldValue cgwalters-forge/agent-board#63 Priority=P0".into(),
            ]
        );
        assert_eq!(mutations[1]["variables"]["issueId"], "I_epic");
        assert_eq!(
            mutations[0]["body"],
            json!({"title": "epic apply", "body": "Apply epic plans."})
        );
        assert_eq!(mutations[5]["body"]["state_reason"], "not_planned");
        let comment = mutations[2]["body"]["body"].as_str().unwrap();
        assert!(comment.starts_with("```agent-board-plan\n"), "{comment}");
        assert!(comment.contains("\"item\": \"NEW-1\""), "{comment}");
        // An empty plan is valid and changes nothing.
        let empty = EpicPlan::validate(&plan(vec![]), &bounds(), &epic).unwrap();
        assert!(empty.mutations(&epic).is_empty());
        // Slices that stack name a new slice or a current one; their job plans
        // carry it, a new one's number to be filled in by apply.
        let mut on_new = create();
        on_new["stacks_on"] = json!(1);
        let mut on_current = create();
        on_current["stacks_on"] = json!("CGWalters-Forge/agent-board#63");
        let stacked = plan(vec![create(), on_new, on_current]);
        let checked = EpicPlan::validate(&stacked, &bounds(), &epic).unwrap();
        let jobs: Vec<Value> = checked
            .mutations(&epic)
            .iter()
            .filter_map(|m| m["body"]["body"].as_str())
            .filter_map(|b| fenced_json(b, crate::plan::PLAN_FENCE).unwrap())
            .collect();
        let stacks: Vec<&Value> = jobs.iter().map(|j| &j["stacks_on"]).collect();
        assert_eq!(stacks, [&Value::Null, &json!("NEW-1"), &json!(63)]);
    }

    /// A plan written while a sub-issue was open, which closed before apply
    /// (tracker#596), is satisfied there rather than refused on every pass.
    #[test]
    fn closing_an_already_closed_sub_issue_is_satisfied() {
        let epic = epic();
        let mut actions = valid();
        actions.insert(
            1,
            json!({"type": "close_sub_issue", "issue": "CGWalters-Forge/tracker#574", "reason": "Done."}),
        );
        let checked = EpicPlan::validate(&plan(actions), &bounds(), &epic).unwrap();
        assert_eq!(
            checked.actions[1],
            Action::AlreadyClosed {
                issue: IssueRef::parse("cgwalters-forge/tracker#574").unwrap()
            }
        );
        // Listed but no writes, and the `new` of the set_priority after it
        // still names the create before it.
        let mutations = checked.mutations(&epic);
        let of =
            |n: usize| -> Vec<&Value> { mutations.iter().filter(|m| m["action"] == n).collect() };
        assert_eq!(
            of(2),
            [&json!({"action": 2, "satisfied": "cgwalters-forge/tracker#574 is already closed"})]
        );
        assert_eq!(of(3).len(), 1);
        assert_eq!(of(3)[0]["issue"], "cgwalters-forge/agent-board#NEW-1");
        assert_eq!(mutations.len(), 10, "{mutations:?}");
    }

    /// A slice stacks on a sub-issue the same plan closes, in either order:
    /// apply would abandon the pull request the slice builds on and dispatch
    /// would run it unstacked (agent-board#207). The lower slice's rescope,
    /// priority and other slices stacking on it stay allowed.
    #[test]
    fn stacking_on_a_sub_issue_the_plan_closes_is_refused() {
        let lower = "cgwalters-forge/agent-board#63";
        let close = json!({"type": "close_sub_issue", "issue": lower, "reason": "Dropped."});
        let mut stacked = create();
        stacked["stacks_on"] = json!(lower);
        for (actions, error) in [
            (
                vec![close.clone(), stacked.clone()],
                "action 2 (create_sub_issue): cgwalters-forge/agent-board#63 is already the subject of action 1",
            ),
            (
                vec![stacked.clone(), close.clone()],
                "action 2 (close_sub_issue): cgwalters-forge/agent-board#63 is already the subject of action 1",
            ),
            // The earliest of two slices stacked on the closed one is named.
            (
                vec![stacked.clone(), stacked.clone(), close.clone()],
                "action 3 (close_sub_issue): cgwalters-forge/agent-board#63 is already the subject of action 1",
            ),
        ] {
            let message = format!(
                "{:#}",
                EpicPlan::validate(&plan(actions), &bounds(), &epic()).unwrap_err()
            );
            assert!(message.contains(error), "{error}: {message}");
        }
        // A refused slice does not count against the issue it would stack on.
        let mut refused = stacked.clone();
        refused["title"] = json!(" ");
        let message = format!(
            "{:#}",
            EpicPlan::validate(&plan(vec![refused, close.clone()]), &bounds(), &epic())
                .unwrap_err()
        );
        assert!(message.contains("title is empty"), "{message}");
        assert!(!message.contains("already the subject"), "{message}");
        // Rescoped, reprioritised and stacked on twice: all coherent with each
        // other, and a stack on a new slice is not an action on any issue.
        let mut on_new = create();
        on_new["stacks_on"] = json!(1);
        let mut actions = valid();
        actions.extend([stacked.clone(), stacked, on_new]);
        let checked = EpicPlan::validate(&plan(actions), &bounds(), &epic()).unwrap();
        assert_eq!(checked.actions.len(), 8);
    }

    /// Each refusal: the actions after the valid ones, and the error for the
    /// first refused one (its position counts from 7).
    #[test]
    fn action_refusal_table() {
        let long = "x".repeat(TITLE_LIMIT + 1);
        let mut with_repo = create();
        with_repo["plan"]["repo"] = json!("cgwalters-forge/tracker");
        let mut bad_job = create();
        bad_job["plan"]["runner"] = json!("devspace");
        let mut epic_label = create();
        epic_label["labels"] = json!(["coordinator", " Epic "]);
        let mut outside = create();
        outside["repo"] = json!("someone/else");
        let mut unknown = create();
        unknown["assignees"] = json!(["cgwalters"]);
        let mut job_unknown = create();
        job_unknown["plan"]["budget"] = json!(5);
        let mut long_title = create();
        long_title["title"] = json!(long);
        let priority = |issue: &str| json!({"type": "set_priority", "issue": issue, "value": "P1"});
        let stacked = |lower: Value| {
            let mut action = create();
            action["stacks_on"] = lower;
            action
        };
        let mut job_stacked = create();
        job_stacked["plan"]["stacks_on"] = json!(63);
        let mut unplanned = stacked(json!(1));
        unplanned.as_object_mut().unwrap().remove("plan");
        let mut research = stacked(json!(1));
        research["plan"]["kind"] = json!("research");
        let mut tracker_slice = create();
        tracker_slice["repo"] = json!("cgwalters-forge/tracker");
        let mut tracker_stacked = tracker_slice.clone();
        tracker_stacked["stacks_on"] = json!("cgwalters-forge/tracker#573");
        for (extra, error) in [
            // A slice stacks on another of the epic's in its own repository,
            // named by the action, and only for an implement run.
            (
                vec![job_stacked],
                "plan: names stacks_on, which the action names",
            ),
            (vec![unplanned], "stacks_on needs an implement plan"),
            (vec![research], "stacks_on needs an implement plan"),
            (
                vec![stacked(json!(3))],
                "stacks_on names action 3, which does not create a sub-issue with an implement plan",
            ),
            (
                vec![stacked(json!(6))],
                "stacks_on is not the position of an earlier action",
            ),
            (
                vec![stacked(json!(0))],
                "stacks_on is not the position of an earlier action",
            ),
            (
                vec![stacked(json!("cgwalters-forge/agent-board#64"))],
                "is neither new nor a current sub-issue",
            ),
            (
                vec![stacked(json!("cgwalters-forge/tracker#573"))],
                "stacks_on is in cgwalters-forge/tracker, not cgwalters-forge/agent-board",
            ),
            (
                vec![tracker_slice, stacked(json!(6))],
                "stacks_on is in cgwalters-forge/tracker, not cgwalters-forge/agent-board",
            ),
            (vec![stacked(json!("dispatch/63"))], "is not OWNER/REPO#N"),
            (vec![stacked(json!(true))], "stacks_on is neither"),
            // Issues that are neither new nor current sub-issues.
            (
                vec![
                    json!({"type": "close_sub_issue", "issue": "cgwalters-forge/tracker#999", "reason": "Done."}),
                ],
                "cgwalters-forge/tracker#999 is neither new nor a current sub-issue of cgwalters-forge/tracker#572",
            ),
            (
                vec![
                    json!({"type": "set_priority", "issue": "cgwalters-forge/agent-board#64", "value": "P1"}),
                ],
                "is neither new nor a current sub-issue",
            ),
            (
                vec![json!({"type": "set_priority", "new": 4, "value": "P1"})],
                "does not create a sub-issue",
            ),
            (
                vec![json!({"type": "set_priority", "new": 6, "value": "P1"})],
                "not the position of an earlier action",
            ),
            (
                vec![json!({"type": "set_priority", "new": 0, "value": "P1"})],
                "not the position of an earlier action",
            ),
            (
                vec![
                    outside.clone(),
                    json!({"type": "set_priority", "new": 6, "value": "P1"}),
                ],
                "action 7 (set_priority): new names action 6, which is refused",
            ),
            (
                vec![priority("cgwalters-forge/tracker#574")],
                "cgwalters-forge/tracker#574 is a closed sub-issue; a plan may not change it",
            ),
            // Only a close of a closed sub-issue is satisfied; it is checked
            // as any close is, and still counts as the issue's one action.
            (
                vec![
                    json!({"type": "rescope_sub_issue", "issue": "cgwalters-forge/tracker#574", "body": "b", "reason": "r"}),
                ],
                "cgwalters-forge/tracker#574 is a closed sub-issue; a plan may not change it",
            ),
            (
                vec![
                    json!({"type": "close_sub_issue", "issue": "elsewhere/repo#2", "reason": "Moved."}),
                ],
                "repository elsewhere/repo is not in the configured list",
            ),
            (
                vec![
                    json!({"type": "close_sub_issue", "issue": "cgwalters-forge/tracker#574", "reason": "<!-- x -->"}),
                ],
                "reason holds an agent-board fence or a hidden marker",
            ),
            (
                vec![
                    json!({"type": "close_sub_issue", "issue": "cgwalters-forge/tracker#574", "reason": "x"}),
                    json!({"type": "close_sub_issue", "issue": "cgwalters-forge/tracker#574", "reason": "y"}),
                ],
                "action 7 (close_sub_issue): cgwalters-forge/tracker#574 is already the subject of action 6",
            ),
            (
                vec![
                    json!({"type": "set_priority", "issue": "cgwalters-forge/agent-board#63", "new": 1, "value": "P1"}),
                ],
                "exactly one of issue or new",
            ),
            // Repositories outside the configured list, new or existing.
            (
                vec![outside],
                "repository someone/else is not in the configured list",
            ),
            (
                vec![
                    json!({"type": "close_sub_issue", "issue": "elsewhere/repo#1", "reason": "Moved."}),
                ],
                "repository elsewhere/repo is not in the configured list",
            ),
            // The epic itself: its text, priority or labels.
            (
                vec![
                    json!({"type": "rescope_sub_issue", "issue": EPIC, "body": "New text.", "reason": "Why."}),
                ],
                "is the epic itself",
            ),
            (
                vec![
                    json!({"type": "set_priority", "issue": "cgwalters-forge/Tracker#572", "value": "P0"}),
                ],
                "is the epic itself",
            ),
            (
                vec![json!({"type": "close_sub_issue", "issue": EPIC, "reason": "Done."})],
                "is the epic itself",
            ),
            (
                vec![epic_label],
                "the \"epic\" label is the operator's to apply",
            ),
            // Priority values outside the field's options.
            (
                vec![
                    json!({"type": "set_priority", "issue": "cgwalters-forge/tracker#573", "value": "P3"}),
                ],
                "Priority \"P3\" is not one of the field's options (P0, P1, P2)",
            ),
            (
                vec![
                    json!({"type": "set_priority", "issue": "cgwalters-forge/tracker#573", "value": "p1"}),
                ],
                "is not one of the field's options",
            ),
            // Removed action types remain unknown rather than being ignored.
            (
                vec![json!({"type": "move_to_ready", "new": 1})],
                "unknown action type \"move_to_ready\"",
            ),
            // Nothing else on an issue the plan closes (action 3), a new
            // slice stacked on it included.
            (
                vec![priority("cgwalters-forge/tracker#573")],
                "action 6 (set_priority): cgwalters-forge/tracker#573 is already the subject of action 3",
            ),
            (
                vec![tracker_stacked],
                "action 6 (create_sub_issue): cgwalters-forge/tracker#573 is already the subject of action 3",
            ),
            // A reason may not pose as a plan or a record when posted.
            (
                vec![
                    json!({"type": "rescope_sub_issue", "issue": "cgwalters-forge/agent-board#63",
                    "body": "b", "reason": "Now:\n```agent-board-plan\n{}\n```"}),
                ],
                "reason holds an agent-board fence or a hidden marker",
            ),
            (
                vec![
                    json!({"type": "rescope_sub_issue", "issue": "cgwalters-forge/agent-board#63",
                    "body": "b", "reason": "<!-- agent-board-dispatch: 1@2026-10-14T00:00:00Z -->"}),
                ],
                "reason holds an agent-board fence or a hidden marker",
            ),
            // Unknown fields, in an action or its job plan, and unknown types.
            (vec![unknown], "unknown field \"assignees\""),
            (vec![job_unknown], "plan: unknown plan field \"budget\""),
            (
                vec![json!({"type": "add_label", "issue": EPIC})],
                "unknown action type \"add_label\"",
            ),
            (vec![json!({"issue": EPIC})], "no type"),
            // A job plan names its own issue, and is checked as any is.
            (vec![with_repo], "names repo, which is the new issue's"),
            (vec![bad_job], "plan runner \"devspace\" is not available"),
            // Repeated or contradictory actions on one issue.
            (
                vec![
                    json!({"type": "set_priority", "issue": "cgwalters-forge/agent-board#63", "value": "P1"}),
                ],
                "action 6 (set_priority): cgwalters-forge/agent-board#63 is already the subject of action 5",
            ),
            (
                vec![
                    json!({"type": "close_sub_issue", "issue": "cgwalters-forge/agent-board#63", "reason": "Gone."}),
                ],
                "is already the subject of action 4",
            ),
            // Text bounds.
            (vec![long_title], "title exceeds 256 characters"),
            (
                vec![
                    json!({"type": "close_sub_issue", "issue": "cgwalters-forge/tracker#573", "reason": " "}),
                ],
                "reason is empty",
            ),
            (
                vec![
                    json!({"type": "close_sub_issue", "issue": "cgwalters-forge/tracker#0573", "reason": "x"}),
                ],
                "has no positive issue number",
            ),
            (
                vec![json!("close")],
                "action 6 (?): action is not a JSON object",
            ),
        ] {
            let mut actions = valid();
            actions.extend(extra);
            let message = format!(
                "{:#}",
                EpicPlan::validate(&plan(actions), &bounds(), &epic()).unwrap_err()
            );
            assert!(message.starts_with("epic plan refused:"), "{message}");
            assert!(message.contains(error), "{error}: {message}");
        }
    }

    #[test]
    fn every_refused_action_is_named() {
        let actions = vec![
            json!({"type": "set_priority", "issue": EPIC, "value": "P0"}),
            create(),
            json!({"type": "move_to_ready", "issue": "cgwalters-forge/tracker#573"}),
            json!({"type": "close_sub_issue", "issue": "cgwalters-forge/tracker#1", "reason": "x"}),
        ];
        let off = bounds();
        let message = format!(
            "{:#}",
            EpicPlan::validate(&plan(actions), &off, &epic()).unwrap_err()
        );
        let lines: Vec<_> = message.lines().collect();
        assert_eq!(lines.len(), 4, "{message}");
        assert!(
            lines[1].starts_with("  action 1 (set_priority): "),
            "{message}"
        );
        // The removed action is refused.
        assert!(
            lines[2].contains("action 3 (move_to_ready): unknown action type"),
            "{message}"
        );
        assert!(
            lines[3].starts_with("  action 4 (close_sub_issue): "),
            "{message}"
        );
        // A refused action does not count against its issue.
        let priority =
            json!({"type": "set_priority", "issue": "cgwalters-forge/tracker#573", "value": "P1"});
        let actions = vec![
            json!({"type": "set_priority", "issue": "cgwalters-forge/tracker#573", "value": "P3"}),
            json!({"type": "set_priority", "issue": "cgwalters-forge/tracker#573", "value": "P1"}),
            priority,
        ];
        let one = bounds();
        let message = format!(
            "{:#}",
            EpicPlan::validate(&plan(actions), &one, &epic()).unwrap_err()
        );
        let lines: Vec<_> = message.lines().skip(1).collect();
        assert_eq!(lines.len(), 2, "{message}");
        assert!(lines[0].starts_with("  action 1 (set_priority): Priority \"P3\""));
        assert!(lines[1].starts_with("  action 3 (set_priority):"));
        // With no configured bounds, nothing passes.
        let none = EpicPlan::validate(&plan(valid()), &Bounds::default(), &epic());
        assert!(format!("{:#}", none.unwrap_err()).contains("not in the configured list"));
    }

    #[test]
    fn plan_refusal_table() {
        let many: Vec<Value> = (0..=ACTION_LIMIT).map(|_| create()).collect();
        for (key, value, error) in [
            (
                "schema",
                json!("agent-board/epic-plan/v2"),
                "unsupported epic plan schema",
            ),
            ("schema", Value::Null, "epic plan: schema is not a string"),
            (
                "epic",
                json!("cgwalters-forge/tracker#571"),
                "is for cgwalters-forge/tracker#571, but was found on cgwalters-forge/tracker#572",
            ),
            (
                "epic",
                json!("cgwalters-forge/tracker"),
                "is not OWNER/REPO#N",
            ),
            ("actions", json!({}), "actions is not a list"),
            ("actions", json!(many), "more than 20 actions"),
            (
                "priority",
                json!("P0"),
                "unknown epic plan field \"priority\"",
            ),
        ] {
            let mut edited = plan(vec![]);
            edited[key] = value;
            let message = format!(
                "{:#}",
                EpicPlan::validate(&edited, &bounds(), &epic()).unwrap_err()
            );
            assert!(message.contains(error), "{key}: {message}");
        }
        for key in ["schema", "epic", "actions"] {
            let mut missing = plan(vec![]);
            missing.as_object_mut().unwrap().remove(key);
            assert!(
                EpicPlan::validate(&missing, &bounds(), &epic()).is_err(),
                "{key}"
            );
        }
        assert!(EpicPlan::validate(&json!([]), &bounds(), &epic()).is_err());
        let bad = Bounds {
            repositories: vec!["not a repo".into()],
            ..bounds()
        };
        assert!(bad.validate().is_err());
    }

    /// The epic's comments and sub-issues, as GitHub answers the two queries.
    struct Forge {
        comments: Value,
        sub_issues: Value,
        reads: std::cell::Cell<usize>,
    }

    impl Transport for Forge {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            self.reads.set(self.reads.get() + 1);
            assert_eq!(variables["number"], 572);
            if query == SUB_ISSUES_QUERY {
                return Ok(json!({"repository": {"issue": self.sub_issues}}));
            }
            Ok(
                json!({"repository": {"issue": {"comments": {"nodes": self.comments,
                "pageInfo": {"hasNextPage": false, "endCursor": null}}}}}),
            )
        }
    }

    #[test]
    fn newest_epic_plan_from_writers_table() {
        const URL: &str = "https://github.com/cgwalters-forge/tracker/issues/572";
        let fenced = |actions: Vec<Value>| {
            format!("Plan:\n\n```{EPIC_PLAN_FENCE}\n{:#}\n```\n", plan(actions))
        };
        let node = |id: u32, author: &str, editor: Option<&str>, at: &str, body: String| {
            json!({"url": format!("{URL}#issuecomment-{id}"), "body": body, "createdAt": at,
                "lastEditedAt": editor.map(|_| at), "author": {"login": author},
                "editor": editor.map(|login| json!({"login": login}))})
        };
        let subs = json!({"id": "I_epic", "state": "OPEN", "subIssues": {"nodes": [
            {"number": 63, "state": "OPEN", "repository": {"nameWithOwner": "cgwalters-forge/agent-board"}},
            {"number": 573, "state": "OPEN", "repository": {"nameWithOwner": "cgwalters-forge/tracker"}}],
            "pageInfo": {"hasNextPage": false}}});
        let first = || {
            node(
                1,
                "cgwalters-bot",
                None,
                "2026-10-14T10:00:00Z",
                fenced(valid()),
            )
        };
        let close = json!({"type": "close_sub_issue", "issue": "cgwalters-forge/tracker#573", "reason": "x"});
        let mut full = subs.clone();
        full["subIssues"]["pageInfo"]["hasNextPage"] = json!(true);
        // Comments, sub-issues, then the comment and action count chosen, or an error.
        for (comments, sub_issues, expected) in [
            (vec![first()], subs.clone(), Ok((1, 5))),
            // A writer replaces the plan; a visitor's newer one is ignored.
            (
                vec![
                    first(),
                    node(
                        2,
                        "cgwalters",
                        None,
                        "2026-10-14T11:00:00Z",
                        fenced(vec![close.clone()]),
                    ),
                    node(3, "visitor", None, "2026-10-14T12:00:00Z", fenced(vec![])),
                ],
                subs.clone(),
                Ok((2, 1)),
            ),
            // A job plan is not an epic plan.
            (
                vec![node(
                    1,
                    "cgwalters",
                    None,
                    "2026-10-14T10:00:00Z",
                    first()["body"]
                        .as_str()
                        .unwrap()
                        .replace(EPIC_PLAN_FENCE, crate::plan::PLAN_FENCE),
                )],
                subs.clone(),
                Err("no plan on this issue from an accepted writer (agent-board-epic-plan)"),
            ),
            // A foreign edit refuses the plan rather than reviving the one before.
            (
                vec![
                    first(),
                    node(
                        2,
                        "cgwalters",
                        Some("collaborator"),
                        "2026-10-14T11:00:00Z",
                        fenced(vec![]),
                    ),
                ],
                subs.clone(),
                Err("issuecomment-2 was last edited by collaborator"),
            ),
            // So does an invalid newest plan.
            (
                vec![
                    first(),
                    node(
                        2,
                        "cgwalters",
                        None,
                        "2026-10-14T11:00:00Z",
                        fenced(vec![json!({"type": "x"})]),
                    ),
                ],
                subs.clone(),
                Err(
                    "newest epic plan https://github.com/cgwalters-forge/tracker/issues/572#issuecomment-2: epic plan refused:\n  action 1 (x): unknown action type",
                ),
            ),
            // Sub-issues are read as they are now.
            (
                vec![first()],
                json!({"id": "I_epic", "state": "OPEN", "subIssues": {"nodes": [], "pageInfo": {"hasNextPage": false}}}),
                Err("cgwalters-forge/tracker#573 is neither new nor a current sub-issue"),
            ),
            // A closed epic refuses every plan, and an unknown state is not open.
            (
                vec![first()],
                {
                    let mut closed = subs.clone();
                    closed["state"] = json!("CLOSED");
                    closed
                },
                Err("epic is closed; a plan may not change a closed epic's sub-issues"),
            ),
            (
                vec![first()],
                {
                    let mut unknown = subs.clone();
                    unknown.as_object_mut().unwrap().remove("state");
                    unknown
                },
                Err("cgwalters-forge/tracker#572 has no state"),
            ),
            (vec![first()], full, Err("more than 100 sub-issues")),
            // A closed one the plan rescopes is refused; one it closes is
            // satisfied (closing_an_already_closed_sub_issue_is_satisfied).
            (
                vec![first()],
                {
                    let mut closed = subs.clone();
                    closed["subIssues"]["nodes"][0]["state"] = json!("CLOSED");
                    closed
                },
                Err("cgwalters-forge/agent-board#63 is a closed sub-issue"),
            ),
            (
                vec![first()],
                {
                    let mut closed = subs.clone();
                    closed["subIssues"]["nodes"][1]["state"] = json!("CLOSED");
                    closed
                },
                Ok((1, 5)),
            ),
            (
                vec![first()],
                {
                    let mut unknown = subs.clone();
                    unknown["subIssues"]["nodes"][1]["state"] = Value::Null;
                    unknown
                },
                Err("sub-issue cgwalters-forge/tracker#573 has no state"),
            ),
            (
                vec![first()],
                json!({"id": "I_epic"}),
                Err("more than 100 sub-issues, or GitHub did not say"),
            ),
            (vec![first()], Value::Null, Err("is not an issue")),
        ] {
            let forge = Forge {
                comments: json!(comments),
                sub_issues,
                reads: Default::default(),
            };
            let issue = crate::github::parse_github_issue(URL, None, "github.com").unwrap();
            let result =
                newest_epic_plan(&forge, &issue, &["cgwalters", "cgwalters-bot"], &bounds());
            match (result, expected) {
                (Ok((plan, epic, from)), Ok((n, actions))) => {
                    assert!(from.url.ends_with(&format!("-{n}")), "{}", from.url);
                    assert_eq!(plan.actions.len(), actions);
                    assert_eq!(epic.id, "I_epic");
                }
                (Err(error), Err(expected)) => {
                    let error = format!("{error:#}");
                    assert!(error.contains(expected), "{error}");
                }
                (result, expected) => panic!("{:?} != {expected:?}", result.map(|r| r.0)),
            }
        }
        // Invalid bounds are refused before anything is read.
        let forge = Forge {
            comments: json!([first()]),
            sub_issues: subs,
            reads: Default::default(),
        };
        let issue = crate::github::parse_github_issue(URL, None, "github.com").unwrap();
        let bad = Bounds {
            repositories: vec!["a/b?c".into()],
            ..bounds()
        };
        assert!(newest_epic_plan(&forge, &issue, &["cgwalters"], &bad).is_err());
        assert_eq!(forge.reads.get(), 0);
    }
}
