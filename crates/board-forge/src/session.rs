//! Session batches: what one per-repository session run proposes, written
//! as one fenced block in a comment on its plan's item (agent-board#830).
//! Schema: docs/sessions.md and docs/session-batch.schema.json.
//!
//! A session is the per-repository maintainer's first job (tracker#1356):
//! one `session` run reads a repository's open issues and pull requests,
//! the board's items and holds, the poll's maintainer report and the code
//! once, and decides in batch what many cold one-comment triages decided
//! one item at a time. Its batch is a proposal, as an epic plan is: any
//! run that comments as the planner can write one, so the bounds checked
//! here, from the caller's configuration, and the cap on each kind of
//! action are what limit it. Nothing here applies a batch: the pass does,
//! once, through [`crate::session_apply`] (agent-board#845), and
//! `agent-board check session-batch` validates one offline.
use crate::epic::{BODY_LIMIT, IssueRef, REASON_LIMIT, plain, text};
use crate::pass::check_domains;
use crate::plan::{JobPlan, SESSION_KIND, fenced_json_as};
use crate::poll::validate_repository;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub const SESSION_BATCH_SCHEMA: &str = "agent-board/session-batch/v1";
/// The info string of the fence that holds a session batch.
pub const SESSION_FENCE: &str = "agent-board-session";
/// Actions in one batch, the design's launch profile's cap on one run's
/// outputs (docs/design.md#3-agentic-job-bounds-and-delegated-operations).
pub const ACTION_LIMIT: usize = 20;
/// Issues one batch closes: a session that misreads a repository closes at
/// most this many before a person sees its batch.
pub const CLOSE_LIMIT: usize = 5;
/// Issues one batch rewrites the body of.
pub const RESCOPE_LIMIT: usize = 5;
/// Priorities one batch sets, the profile's cap on project updates.
pub const PRIORITY_LIMIT: usize = 10;
/// Issues one batch attaches to epics.
pub const ATTACH_LIMIT: usize = 5;
/// Job plans one batch posts: each is a run the pass may start, so this
/// bounds the inference one session can commit the pools to.
pub const PLAN_LIMIT: usize = 5;
/// Questions to the operator in one batch; each costs their attention.
pub const ASK_LIMIT: usize = 3;
/// Characters of one question: one line, as the operator reads it in a
/// queue, not a page.
pub const QUESTION_LIMIT: usize = 500;
/// The most of a candidate comment body the offline check reads.
pub const BODY_MAX_BYTES: usize = 1024 * 1024;

/// Action types, the fields each takes besides `type`, and the most of
/// each in one batch.
const ACTIONS: &[(&str, &[&str], usize)] = &[
    (CLOSE, &["issue", "reason"], CLOSE_LIMIT),
    (RESCOPE, &["issue", "body", "reason"], RESCOPE_LIMIT),
    (SET_PRIORITY, &["issue", "value"], PRIORITY_LIMIT),
    (ATTACH, &["issue", "epic"], ATTACH_LIMIT),
    (POST_PLAN, &["issue", "plan"], PLAN_LIMIT),
    (ASK, &["domain", "question"], ASK_LIMIT),
];
const CLOSE: &str = "close_issue";
const RESCOPE: &str = "rescope_issue";
const SET_PRIORITY: &str = "set_priority";
const ATTACH: &str = "attach_to_epic";
const POST_PLAN: &str = "post_plan";
const ASK: &str = "ask_operator";
/// What an attach's `epic` is to the batch: a target that may not also be
/// closed by it, and that several attaches may share.
const EPIC: &str = "epic";
/// Kinds a posted plan may ask for. A fix plan needs the pull request it
/// rewrites, which a session does not read; a session plan would let one
/// session start the next, spending without a pass's decision.
const PLAN_KINDS: &[&str] = &["implement", "triage", "research"];

/// What a batch may touch, from the caller's own configuration (never from
/// a batch or a dispatcher's inputs). Empty bounds allow nothing.
#[derive(Clone, Debug, Default)]
pub struct Bounds {
    /// Repositories, OWNER/NAME, a session may run on and whose epics a
    /// batch may attach issues to.
    pub repositories: Vec<String>,
    /// The board's Priority field's options.
    pub priorities: Vec<String>,
    /// The caller's `operatorAsk.domains`: the only subjects a batch may
    /// ask the operator about.
    pub domains: Vec<String>,
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
        check_domains(&self.domains, "configured ask domains")
    }

    fn allowed(&self, repo: &str) -> Result<()> {
        ensure!(
            (self.repositories.iter()).any(|r| r.eq_ignore_ascii_case(repo)),
            "{repo} is not a configured repository"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Comment the reason, close as not planned.
    Close { issue: IssueRef, reason: String },
    /// Replace the body, comment the reason.
    Rescope {
        issue: IssueRef,
        body: String,
        reason: String,
    },
    /// Set the board's Priority.
    SetPriority { issue: IssueRef, value: String },
    /// Link the issue as a sub-issue of an epic.
    Attach { issue: IssueRef, epic: IssueRef },
    /// Post a job plan on the issue; the pass starts it as any plan.
    Plan { issue: IssueRef, plan: JobPlan },
    /// One line for the operator, within a configured domain.
    Ask { domain: String, question: String },
}

impl Action {
    /// Its `type` in a batch.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Close { .. } => CLOSE,
            Self::Rescope { .. } => RESCOPE,
            Self::SetPriority { .. } => SET_PRIORITY,
            Self::Attach { .. } => ATTACH,
            Self::Plan { .. } => POST_PLAN,
            Self::Ask { .. } => ASK,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionBatch {
    /// The repository the session ran on, OWNER/NAME: every issue it acts
    /// on is there, and an applier checks it against the item the batch
    /// was found on.
    pub repo: String,
    pub actions: Vec<Action>,
}

impl SessionBatch {
    /// Validate one batch object against `bounds`. Errors in the batch as a
    /// whole stop at the first; each action is checked in full and every
    /// refused action named, so a session sees all at once.
    pub fn validate(value: &Value, bounds: &Bounds) -> Result<Self> {
        let object = value
            .as_object()
            .context("session batch is not a JSON object")?;
        for key in object.keys() {
            ensure!(
                ["schema", "repo", "actions"].contains(&key.as_str()),
                "unknown session batch field {key:?}"
            );
        }
        ensure!(
            text(object, "schema")? == SESSION_BATCH_SCHEMA,
            "unsupported session batch schema (expected {SESSION_BATCH_SCHEMA})"
        );
        let repo = text(object, "repo")?;
        validate_repository(repo)?;
        bounds.allowed(repo)?;
        let actions = object
            .get("actions")
            .context("no actions")?
            .as_array()
            .context("actions is not a list")?;
        ensure!(
            actions.len() <= ACTION_LIMIT,
            "a batch has at most {ACTION_LIMIT} actions"
        );
        let mut checker = Checker {
            bounds,
            repo,
            counts: BTreeMap::new(),
            targets: BTreeMap::new(),
        };
        let mut checked = Vec::with_capacity(actions.len());
        let mut refused = Vec::new();
        for (k, action) in actions.iter().enumerate() {
            match checker.action(action) {
                Ok(action) => checked.push(action),
                Err(error) => refused.push(format!("action {}: {error:#}", k + 1)),
            }
        }
        ensure!(refused.is_empty(), "{}", refused.join("; "));
        Ok(Self {
            repo: repo.into(),
            actions: checked,
        })
    }

    /// The batch in a comment body: `None` without a session fence. The
    /// fence must open at the start of a line, so a batch quoted in a reply
    /// is not one.
    pub fn from_comment(body: &str, bounds: &Bounds) -> Result<Option<Self>> {
        fenced_json_as(body, SESSION_FENCE, "session batch")?
            .map(|value| Self::validate(&value, bounds))
            .transpose()
    }

    /// Each action as typed JSON, as the offline check prints it: what an
    /// applier would do, in the batch's order, never a write. A plan's
    /// absent pool or effort is left out, as the format leaves it out.
    pub fn preview(&self) -> Vec<Value> {
        (self.actions.iter())
            .map(|action| match action {
                Action::Close { issue, reason } => {
                    json!({"type": CLOSE, "issue": issue.to_string(), "reason": reason})
                }
                Action::Rescope {
                    issue,
                    body,
                    reason,
                } => json!({
                    "type": RESCOPE, "issue": issue.to_string(), "body": body, "reason": reason
                }),
                Action::SetPriority { issue, value } => {
                    json!({"type": SET_PRIORITY, "issue": issue.to_string(), "value": value})
                }
                Action::Attach { issue, epic } => json!({
                    "type": ATTACH, "issue": issue.to_string(), "epic": epic.to_string()
                }),
                Action::Plan { issue, plan } => {
                    let mut job = json!({"kind": plan.kind, "brief": plan.brief});
                    if !plan.pool.is_empty() {
                        job["pool"] = plan.pool.as_str().into();
                    }
                    if let Some(effort) = plan.effort {
                        job["effort"] = effort.as_str().into();
                    }
                    json!({"type": POST_PLAN, "issue": issue.to_string(), "plan": job})
                }
                Action::Ask { domain, question } => {
                    json!({"type": ASK, "domain": domain, "question": question})
                }
            })
            .collect()
    }
}

struct Checker<'a> {
    bounds: &'a Bounds,
    /// The session's repository, where every issue it acts on is.
    repo: &'a str,
    /// Actions of each type so far.
    counts: BTreeMap<&'static str, usize>,
    /// The types of action on each issue (lower-cased) so far.
    targets: BTreeMap<String, Vec<&'static str>>,
}

impl Checker<'_> {
    /// An issue the action changes: in the session's own repository, so a
    /// session on one repository cannot triage every configured one.
    fn issue(&self, object: &Map<String, Value>) -> Result<IssueRef> {
        let issue = IssueRef::parse(text(object, "issue")?)?;
        ensure!(
            issue.repo.eq_ignore_ascii_case(self.repo),
            "{issue} is not in {}, the session's repository",
            self.repo
        );
        Ok(issue)
    }

    /// One action of `kind` on `issue`: the same twice would race itself,
    /// and anything beside a close acts on an issue the batch closes.
    fn once(&mut self, kind: &'static str, issue: &IssueRef) -> Result<()> {
        let kinds = self
            .targets
            .entry(issue.to_string().to_lowercase())
            .or_default();
        ensure!(
            !kinds.contains(&kind),
            "{issue} already has a {kind} action in this batch"
        );
        ensure!(
            kind != CLOSE || kinds.is_empty(),
            "{issue} has another action in this batch, which its close would undo"
        );
        ensure!(
            !kinds.contains(&CLOSE),
            "{issue} is closed by an earlier action in this batch"
        );
        kinds.push(kind);
        Ok(())
    }

    /// An attach's epic: in any configured repository, as epics live in
    /// the tracker, and neither closed by the batch before nor after.
    fn epic(&mut self, object: &Map<String, Value>) -> Result<IssueRef> {
        let epic = IssueRef::parse(text(object, "epic")?)?;
        self.bounds.allowed(&epic.repo)?;
        let kinds = self
            .targets
            .entry(epic.to_string().to_lowercase())
            .or_default();
        ensure!(
            !kinds.contains(&CLOSE),
            "epic {epic} is closed by an earlier action in this batch"
        );
        if !kinds.contains(&EPIC) {
            kinds.push(EPIC);
        }
        Ok(epic)
    }

    fn action(&mut self, value: &Value) -> Result<Action> {
        let object = value.as_object().context("action is not a JSON object")?;
        let kind = text(object, "type")?;
        let (kind, fields, limit) = *ACTIONS
            .iter()
            .find(|(name, _, _)| *name == kind)
            .with_context(|| format!("unknown action type {kind:?}"))?;
        for key in object.keys() {
            ensure!(
                key == "type" || fields.contains(&key.as_str()),
                "unknown field {key:?}"
            );
        }
        let count = self.counts.entry(kind).or_default();
        *count += 1;
        ensure!(
            *count <= limit,
            "a batch has at most {limit} {kind} actions"
        );
        Ok(match kind {
            CLOSE => {
                let issue = self.issue(object)?;
                let reason = plain(object, "reason", REASON_LIMIT)?.to_owned();
                self.once(kind, &issue)?;
                Action::Close { issue, reason }
            }
            RESCOPE => {
                let issue = self.issue(object)?;
                let body = plain(object, "body", BODY_LIMIT)?.to_owned();
                let reason = plain(object, "reason", REASON_LIMIT)?.to_owned();
                self.once(kind, &issue)?;
                Action::Rescope {
                    issue,
                    body,
                    reason,
                }
            }
            SET_PRIORITY => {
                let issue = self.issue(object)?;
                let value = text(object, "value")?;
                ensure!(
                    self.bounds.priorities.iter().any(|p| p == value),
                    "Priority {value:?} is not one of the field's options ({})",
                    self.bounds.priorities.join(", ")
                );
                self.once(kind, &issue)?;
                Action::SetPriority {
                    issue,
                    value: value.into(),
                }
            }
            ATTACH => {
                let issue = self.issue(object)?;
                let epic = IssueRef::parse(text(object, "epic")?)?;
                ensure!(
                    issue.to_string().to_lowercase() != epic.to_string().to_lowercase(),
                    "{issue} cannot be its own epic"
                );
                self.once(kind, &issue)?;
                let epic = self.epic(object)?;
                Action::Attach { issue, epic }
            }
            POST_PLAN => {
                let issue = self.issue(object)?;
                let plan = object.get("plan").context("no plan")?;
                let plan = job_plan(plan, &issue).context("plan")?;
                self.once(kind, &issue)?;
                Action::Plan { issue, plan }
            }
            ASK => {
                let domain = text(object, "domain")?;
                ensure!(
                    self.bounds.domains.iter().any(|d| d == domain),
                    "domain {domain:?} is not one of the configured ask domains ({})",
                    self.bounds.domains.join(", ")
                );
                let question = plain(object, "question", QUESTION_LIMIT)?;
                ensure!(
                    !question.contains(['\n', '\r']),
                    "question must be one line"
                );
                Action::Ask {
                    domain: domain.into(),
                    question: question.into(),
                }
            }
            other => bail!("action type {other:?} has no check"),
        })
    }
}

/// A posted plan's object, with the issue it is posted on as its target.
fn job_plan(value: &Value, issue: &IssueRef) -> Result<JobPlan> {
    let mut object = value.as_object().context("is not a JSON object")?.clone();
    for key in ["repo", "item"] {
        ensure!(
            !object.contains_key(key),
            "names {key}, which is the issue's"
        );
    }
    ensure!(
        !object.contains_key("stacks_on"),
        "names stacks_on; a session's plan builds on the default branch"
    );
    object.insert("repo".into(), issue.repo.as_str().into());
    object.insert("item".into(), issue.number.into());
    let plan = JobPlan::from_json(&Value::Object(object))?;
    ensure!(
        PLAN_KINDS.contains(&plan.kind.as_str()),
        "kind {:?} is not one of {}; a {SESSION_KIND} may not start another, and a fix needs its pull request",
        plan.kind,
        PLAN_KINDS.join(", ")
    );
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PLAN_BRIEF_LIMIT, POOL_LIMIT};

    const FIXTURE: &str = include_str!("../../../fixtures/pass/session-batch.md");
    const REPO: &str = "cgwalters-forge/agent-board";
    const ISSUE: &str = "cgwalters-forge/agent-board#830";
    const TRACKER_EPIC: &str = "cgwalters-forge/tracker#1356";

    fn bounds() -> Bounds {
        Bounds {
            repositories: vec![REPO.into(), "cgwalters-forge/tracker".into()],
            priorities: vec!["P0".into(), "P1".into(), "P2".into()],
            domains: vec!["credentials".into(), "budget".into()],
        }
    }

    fn close(issue: &str) -> Value {
        json!({"type": CLOSE, "issue": issue, "reason": "Done by #12."})
    }

    fn attach(issue: &str, epic: &str) -> Value {
        json!({"type": ATTACH, "issue": issue, "epic": epic})
    }

    fn plan() -> Value {
        json!({"type": POST_PLAN, "issue": ISSUE, "plan": {
            "schema": "agent-board/job-plan/v1", "kind": "implement", "runner": "hosted",
            "brief": "Implement the next slice."}})
    }

    fn batch(actions: Vec<Value>) -> Value {
        json!({"schema": SESSION_BATCH_SCHEMA, "repo": REPO, "actions": actions})
    }

    #[test]
    fn fixture_batch_and_its_preview_match_the_schema() {
        let checked = SessionBatch::from_comment(FIXTURE, &bounds())
            .unwrap()
            .expect("the fixture holds a batch");
        assert_eq!(checked.repo, REPO);
        let preview = checked.preview();
        let types: Vec<_> = preview.iter().map(|a| a["type"].clone()).collect();
        assert_eq!(
            types,
            ACTIONS
                .iter()
                .map(|(name, _, _)| json!(name))
                .collect::<Vec<_>>(),
            "the fixture shows one action of each type, in the table's order"
        );
        assert_eq!(
            preview[4]["plan"],
            json!({"kind": "implement", "brief": "Implement the second slice of agent-board#830: apply an accepted session batch once, with a receipt on its item."})
        );
        assert_eq!(preview[5]["domain"], "budget");
        assert_eq!(
            SessionBatch::from_comment("No batch here.", &bounds()).unwrap(),
            None
        );
        let twice = format!("{FIXTURE}\n{FIXTURE}");
        let message = format!(
            "{:#}",
            SessionBatch::from_comment(&twice, &bounds()).unwrap_err()
        );
        assert_eq!(message, "comment holds more than one session batch");

        let schema: Value =
            serde_json::from_str(include_str!("../../../docs/session-batch.schema.json")).unwrap();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["schema"]["const"],
            SESSION_BATCH_SCHEMA
        );
        assert_eq!(schema["properties"]["actions"]["maxItems"], ACTION_LIMIT);
        // The static caps and the text checks the schema carries itself,
        // so a schema-only consumer refuses what the checker refuses; the
        // bounds and the relations between actions remain the checker's.
        let caps: Vec<_> = schema["properties"]["actions"]["allOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|cap| {
                assert_eq!(cap["minContains"], 0, "{cap}");
                (
                    cap["contains"]["properties"]["type"]["const"].clone(),
                    cap["maxContains"].clone(),
                )
            })
            .collect();
        let limits: Vec<_> = ACTIONS
            .iter()
            .map(|(name, _, limit)| (json!(name), json!(limit)))
            .collect();
        assert_eq!(caps, limits, "the schema caps every action type");
        let plain = &schema["$defs"]["plain"]["not"]["pattern"];
        for (text, limit) in [
            ("reason", REASON_LIMIT),
            ("body", BODY_LIMIT),
            ("question", QUESTION_LIMIT),
        ] {
            assert_eq!(schema["$defs"][text]["maxLength"], limit);
            assert_eq!(schema["$defs"][text]["$ref"], "#/$defs/plain", "{text}");
        }
        for refused in [
            "```agent-board-plan\n{}\n```",
            "<!-- agent-board-dispatch: 1@x -->",
        ] {
            let refused = refused.to_owned();
            assert!(
                plain
                    .as_str()
                    .unwrap()
                    .split('|')
                    .any(|needle| refused.contains(needle)),
                "{plain} refuses {refused:?}"
            );
            let one =
                json!({"type": "close_issue", "issue": format!("{REPO}#1"), "reason": refused});
            assert!(SessionBatch::validate(&batch(vec![one]), &bounds()).is_err());
        }
        let job = &schema["$defs"]["job"]["properties"];
        assert_eq!(job["kind"]["enum"], json!(PLAN_KINDS));
        assert_eq!(job["brief"]["maxLength"], PLAN_BRIEF_LIMIT);
        assert_eq!(job["pool"]["maxLength"], POOL_LIMIT);
        let variants = schema["$defs"]["action"]["oneOf"].as_array().unwrap();
        let documented: Vec<_> = variants
            .iter()
            .map(|v| v["properties"]["type"]["const"].clone())
            .collect();
        assert_eq!(documented, types, "the schema lists every action type");
        for (variant, (_, fields, _)) in variants.iter().zip(ACTIONS) {
            let mut keys: Vec<_> = variant["properties"]
                .as_object()
                .unwrap()
                .keys()
                .filter(|k| *k != "type")
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            let mut expected = fields.to_vec();
            expected.sort_unstable();
            assert_eq!(keys, expected, "{}", variant["properties"]["type"]);
        }
    }

    #[test]
    fn bounds_validation_table() {
        for (name, bounds, valid) in [
            ("the test bounds", bounds(), true),
            ("empty bounds", Bounds::default(), true),
            (
                "a malformed repository",
                Bounds {
                    repositories: vec!["owner".into()],
                    ..bounds()
                },
                false,
            ),
            (
                "a blank Priority",
                Bounds {
                    priorities: vec![" ".into()],
                    ..bounds()
                },
                false,
            ),
            (
                "a domain with capitals",
                Bounds {
                    domains: vec!["Budget".into()],
                    ..bounds()
                },
                false,
            ),
            (
                "a repeated domain",
                Bounds {
                    domains: vec!["budget".into(), "budget".into()],
                    ..bounds()
                },
                false,
            ),
        ] {
            assert_eq!(bounds.validate().is_ok(), valid, "{name}");
        }
    }

    #[test]
    fn batch_refusal_table() {
        let mut other_schema = batch(vec![]);
        other_schema["schema"] = json!("agent-board/session-batch/v2");
        let mut unknown = batch(vec![]);
        unknown["summary"] = json!("All well.");
        let mut outside = batch(vec![]);
        outside["repo"] = json!("someone/else");
        let mut malformed = batch(vec![]);
        malformed["repo"] = json!("cgwalters-forge");
        let mut null_repo = batch(vec![]);
        null_repo["repo"] = Value::Null;
        let mut no_actions = batch(vec![]);
        no_actions.as_object_mut().unwrap().remove("actions");
        let mut not_a_list = batch(vec![]);
        not_a_list["actions"] = json!({});
        let many: Vec<_> = (1..=ACTION_LIMIT as u64 + 1)
            .map(|n| json!({"type": SET_PRIORITY, "issue": format!("{REPO}#{n}"), "value": "P1"}))
            .collect();
        for (name, value, error) in [
            ("a list", json!([batch(vec![])]), "not a JSON object"),
            (
                "another schema",
                other_schema,
                "unsupported session batch schema",
            ),
            (
                "an unknown field",
                unknown,
                "unknown session batch field \"summary\"",
            ),
            (
                "a repository outside the bounds",
                outside,
                "someone/else is not a configured repository",
            ),
            ("a malformed repository", malformed, "invalid repository"),
            ("a null repository", null_repo, "repo is not a string"),
            ("no actions", no_actions, "no actions"),
            (
                "actions that are not a list",
                not_a_list,
                "actions is not a list",
            ),
            ("too many actions", batch(many), "at most 20 actions"),
        ] {
            let message = format!(
                "{:#}",
                SessionBatch::validate(&value, &bounds()).unwrap_err()
            );
            assert!(message.contains(error), "{name}: {message}");
        }
        let empty = SessionBatch::validate(&batch(vec![]), &bounds()).unwrap();
        assert!(empty.actions.is_empty() && empty.preview().is_empty());
        // GitHub names repositories without regard to case; the batch's
        // spelling is kept.
        let mut cased = batch(vec![close("CGWalters-Forge/Agent-Board#1")]);
        cased["repo"] = json!("CGWalters-Forge/Agent-Board");
        let checked = SessionBatch::validate(&cased, &bounds()).unwrap();
        assert_eq!(checked.repo, "CGWalters-Forge/Agent-Board");
        assert_eq!(
            checked.preview()[0]["issue"],
            "CGWalters-Forge/Agent-Board#1"
        );
    }

    #[test]
    fn action_refusal_table() {
        let edited = |mut action: Value, key: &str, value: Value| {
            action[key] = value;
            action
        };
        let rescope =
            json!({"type": RESCOPE, "issue": ISSUE, "body": "New scope.", "reason": "Narrowed."});
        let priority = json!({"type": SET_PRIORITY, "issue": ISSUE, "value": "P1"});
        let ask = json!({"type": ASK, "domain": "budget", "question": "May the pools grow?"});
        let planned = |key: &str, value: Value| {
            let mut action = plan();
            action["plan"][key] = value;
            action
        };
        let mut unplanned = plan();
        unplanned.as_object_mut().unwrap().remove("plan");
        let closes: Vec<_> = (1..=CLOSE_LIMIT as u64 + 1)
            .map(|n| close(&format!("{REPO}#{n}")))
            .collect();
        let marker = json!("<!-- agent-board-dispatch: 1@now -->");
        let fence = json!("```agent-board-plan\n{}\n```");
        #[rustfmt::skip]
        let cases: Vec<(&str, Vec<Value>, &str)> = vec![
            ("not an object", vec![json!("close")], "action 1: action is not a JSON object"),
            ("an unknown type", vec![json!({"type": "merge_pull_request"})], "unknown action type \"merge_pull_request\""),
            ("a null type", vec![json!({"type": null})], "type is not a string"),
            ("an unknown field", vec![edited(close(ISSUE), "labels", json!(["bug"]))], "unknown field \"labels\""),
            ("no issue", vec![json!({"type": CLOSE, "reason": "Done."})], "no issue"),
            ("a null reason", vec![edited(close(ISSUE), "reason", Value::Null)], "reason is not a string"),
            ("an issue in another configured repository", vec![close(TRACKER_EPIC)], "cgwalters-forge/tracker#1356 is not in cgwalters-forge/agent-board, the session's repository"),
            ("an issue outside the bounds", vec![close("someone/else#1")], "someone/else#1 is not in cgwalters-forge/agent-board"),
            ("an issue number of zero", vec![close("cgwalters-forge/agent-board#0")], "no positive issue number"),
            ("a blank reason", vec![edited(close(ISSUE), "reason", json!(" "))], "reason is empty"),
            ("a long reason", vec![edited(close(ISSUE), "reason", json!("x".repeat(REASON_LIMIT + 1)))], "reason exceeds"),
            ("a fence in a reason", vec![edited(close(ISSUE), "reason", fence.clone())], "reason holds an agent-board fence or a hidden marker"),
            ("a marker in a reason", vec![edited(close(ISSUE), "reason", marker.clone())], "reason holds an agent-board fence or a hidden marker"),
            ("a plan fence in a body", vec![edited(rescope.clone(), "body", fence)], "body holds an agent-board fence or a hidden marker"),
            ("a marker in a body", vec![edited(rescope.clone(), "body", marker.clone())], "body holds an agent-board fence or a hidden marker"),
            ("a long body", vec![edited(rescope.clone(), "body", json!("x".repeat(BODY_LIMIT + 1)))], "body exceeds"),
            ("a Priority that is no option", vec![edited(priority.clone(), "value", json!("P9"))], "Priority \"P9\" is not one of the field's options (P0, P1, P2)"),
            ("an issue as its own epic", vec![attach(ISSUE, &ISSUE.to_uppercase())], "cannot be its own epic"),
            ("an epic outside the bounds", vec![attach(ISSUE, "someone/else#1")], "someone/else is not a configured repository"),
            ("an epic the batch closed", vec![close("cgwalters-forge/agent-board#7"), attach(ISSUE, "cgwalters-forge/agent-board#7")], "action 2: epic cgwalters-forge/agent-board#7 is closed by an earlier action"),
            ("a close of an epic attached to", vec![attach(ISSUE, "cgwalters-forge/agent-board#7"), close("cgwalters-forge/agent-board#7")], "action 2: cgwalters-forge/agent-board#7 has another action in this batch, which its close would undo"),
            ("no plan", vec![unplanned], "no plan"),
            ("a plan naming its repo", vec![planned("repo", json!("cgwalters-forge/tracker"))], "plan: names repo, which is the issue's"),
            ("a plan naming its item", vec![planned("item", json!(7))], "plan: names item, which is the issue's"),
            ("a stacked plan", vec![planned("stacks_on", json!(829))], "names stacks_on"),
            ("a fix plan", vec![planned("kind", json!("fix"))], "kind \"fix\" is not one of implement, triage, research"),
            ("a session plan", vec![planned("kind", json!(SESSION_KIND))], "a session may not start another"),
            ("a devspace plan", vec![planned("runner", json!("devspace"))], "only hosted"),
            ("a plan with a budget", vec![planned("budget", json!(5))], "unknown plan field \"budget\""),
            ("a domain that is not configured", vec![edited(ask.clone(), "domain", json!("upstream"))], "domain \"upstream\" is not one of the configured ask domains (credentials, budget)"),
            ("a question of two lines", vec![edited(ask.clone(), "question", json!("Which pool?\nAnd when?"))], "question must be one line"),
            ("a long question", vec![edited(ask.clone(), "question", json!("x".repeat(QUESTION_LIMIT + 1)))], "question exceeds"),
            ("a question with a marker", vec![edited(ask.clone(), "question", marker)], "question holds an agent-board fence or a hidden marker"),
            ("the same close twice", vec![close(ISSUE), close(&ISSUE.to_uppercase())], "action 2: CGWALTERS-FORGE/AGENT-BOARD#830 already has a close_issue action"),
            ("a close after a rescope", vec![rescope.clone(), close(ISSUE)], "action 2: cgwalters-forge/agent-board#830 has another action in this batch, which its close would undo"),
            ("a Priority after a close", vec![close(ISSUE), priority.clone()], "action 2: cgwalters-forge/agent-board#830 is closed by an earlier action"),
            ("too many closes", closes, "action 6: a batch has at most 5 close_issue actions"),
        ];
        for (name, actions, error) in cases {
            let message = format!(
                "{:#}",
                SessionBatch::validate(&batch(actions), &bounds()).unwrap_err()
            );
            assert!(message.contains(error), "{name}: {message}");
        }
        // Without configured options or domains, nothing may be set or asked.
        let none = Bounds {
            priorities: vec![],
            domains: vec![],
            ..bounds()
        };
        for (action, error) in [(priority, "options ()"), (ask, "domains ()")] {
            let message = format!(
                "{:#}",
                SessionBatch::validate(&batch(vec![action]), &none).unwrap_err()
            );
            assert!(message.contains(error), "{message}");
        }
    }

    #[test]
    fn every_refused_action_is_named() {
        let actions = vec![
            close("cgwalters-forge/agent-board#1"),
            json!({"type": "reopen_issue", "issue": "cgwalters-forge/agent-board#2"}),
            plan(),
            json!({"type": SET_PRIORITY, "issue": "cgwalters-forge/agent-board#3", "value": "P7"}),
        ];
        let message = format!(
            "{:#}",
            SessionBatch::validate(&batch(actions), &bounds()).unwrap_err()
        );
        assert_eq!(
            message,
            "action 2: unknown action type \"reopen_issue\"; \
             action 4: Priority \"P7\" is not one of the field's options (P0, P1, P2)"
        );
    }

    #[test]
    fn a_priority_a_plan_and_attaches_may_share_targets() {
        let mut priority = json!({"type": SET_PRIORITY, "issue": ISSUE, "value": "P0"});
        let mut heavy = plan();
        heavy["plan"]["pool"] = json!("claude");
        heavy["plan"]["effort"] = json!("heavy");
        let actions = vec![
            priority.clone(),
            heavy,
            attach(ISSUE, TRACKER_EPIC),
            attach("cgwalters-forge/agent-board#703", TRACKER_EPIC),
        ];
        let checked = SessionBatch::validate(&batch(actions), &bounds()).unwrap();
        assert_eq!(checked.actions.len(), 4);
        let Action::Plan { issue, plan: job } = &checked.actions[1] else {
            panic!("{:?}", checked.actions[1]);
        };
        assert_eq!((issue.repo.as_str(), issue.number), (REPO, 830));
        assert_eq!((job.repo.as_str(), job.item), (REPO, 830));
        assert_eq!(
            checked.preview()[1]["plan"],
            json!({"kind": "implement", "pool": "claude", "effort": "heavy", "brief": "Implement the next slice."})
        );
        priority["value"] = json!("P1");
        let message = format!(
            "{:#}",
            SessionBatch::validate(&batch(vec![plan(), priority.clone(), priority]), &bounds())
                .unwrap_err()
        );
        assert!(
            message.contains(
                "action 3: cgwalters-forge/agent-board#830 already has a set_priority action"
            ),
            "{message}"
        );
    }
}
