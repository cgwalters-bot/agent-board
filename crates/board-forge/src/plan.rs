//! Job plans: a job proposal written as a fenced block in an issue comment
//! (cgwalters-forge/agent-board#46), and the `workflow_dispatch` request that
//! starts exactly that job on the caller's dispatch.yml. Schema: docs/job-plans.md.
//!
//! A plan carries only what the caller's dispatch.yml (cgwalters-forge/tracker)
//! has an input for, so nothing in it is silently dropped on the way; the
//! caller's bounds still decide whether the run may do what the plan asks.
use crate::github::{API_ROOT, Transport, is_github_login};
use crate::poll::validate_repository;
use anyhow::{Context, Result, bail, ensure};
use board_core::ItemRef;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub const JOB_PLAN_SCHEMA: &str = "agent-board/job-plan/v1";
/// The info string of the fence that holds a plan: ```` ```agent-board-plan ````.
pub const PLAN_FENCE: &str = "agent-board-plan";
/// The caller workflow a plan is dispatched to.
pub const DISPATCH_WORKFLOW: &str = "dispatch.yml";
/// Profiles a plan may ask for. dispatch.yml also has review and fix, which
/// take a pull request and belong to the poll, not to an issue's plan.
pub const PLAN_KINDS: &[&str] = &["implement", "triage", "research"];
/// Runners the caller can provide. dispatch.yml has no devspace runner yet;
/// a plan asking for one is refused here rather than run on a hosted one.
pub const PLAN_RUNNERS: &[&str] = &["hosted"];
/// Characters of a brief. GitHub caps a dispatch's inputs at 65,535
/// characters together, and the profile is put in front of the brief.
pub const PLAN_BRIEF_LIMIT: usize = 16_000;
/// Comment pages read for one issue (100 each). An issue with more is refused,
/// since the newest plan could be in the part not read.
pub const PLAN_COMMENT_PAGES: usize = 10;
const POOL_LIMIT: usize = 32;
const FIELDS: &[&str] = &["schema", "repo", "item", "kind", "runner", "pool", "brief"];

const COMMENTS_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      comments(first: 100, after: $after) {
        nodes { url body createdAt lastEditedAt author { login } editor { login } }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}"#;

#[derive(Clone, Debug, PartialEq)]
pub struct JobPlan {
    /// Target repository, OWNER/NAME.
    pub repo: String,
    /// Issue number in the target repository.
    pub item: u64,
    pub kind: String,
    pub runner: String,
    /// Inference pool; empty leaves it to the caller's variables.
    pub pool: String,
    /// The task, given to the run after the kind's profile.
    pub brief: String,
}

impl JobPlan {
    /// Validate one plan object. Unknown fields and nulls are refused: a field
    /// this version does not carry must not look as if it were honoured.
    pub fn from_json(value: &Value) -> Result<Self> {
        let object = value.as_object().context("plan is not a JSON object")?;
        for key in object.keys() {
            ensure!(FIELDS.contains(&key.as_str()), "unknown plan field {key:?}");
        }
        let text = |key: &str| -> Result<&str> {
            object
                .get(key)
                .with_context(|| format!("plan has no {key}"))?
                .as_str()
                .with_context(|| format!("plan {key} is not a string"))
        };
        ensure!(
            text("schema")? == JOB_PLAN_SCHEMA,
            "unsupported plan schema (expected {JOB_PLAN_SCHEMA})"
        );
        let repo = text("repo")?;
        validate_repository(repo)?;
        let item = object
            .get("item")
            .context("plan has no item")?
            .as_u64()
            .filter(|n| (1..1 << 53).contains(n))
            .context("plan item is not a positive issue number")?;
        let kind = text("kind")?;
        ensure!(
            PLAN_KINDS.contains(&kind),
            "plan kind {kind:?} is not one of {}",
            PLAN_KINDS.join(", ")
        );
        let runner = text("runner")?;
        ensure!(
            PLAN_RUNNERS.contains(&runner),
            "plan runner {runner:?} is not available: the dispatch caller has only {}",
            PLAN_RUNNERS.join(", ")
        );
        let pool = match object.get("pool") {
            None => "",
            Some(_) => text("pool")?,
        };
        // The caller checks the name against its pools.json; this keeps it a name.
        ensure!(
            pool.len() <= POOL_LIMIT && pool.bytes().all(|c| c.is_ascii_lowercase()),
            "plan pool must be a lowercase name"
        );
        let brief = text("brief")?;
        ensure!(!brief.trim().is_empty(), "plan brief is empty");
        ensure!(
            brief.chars().count() <= PLAN_BRIEF_LIMIT,
            "plan brief exceeds {PLAN_BRIEF_LIMIT} characters"
        );
        Ok(Self {
            repo: repo.into(),
            item,
            kind: kind.into(),
            runner: runner.into(),
            pool: pool.into(),
            brief: brief.into(),
        })
    }

    /// The plan from a comment body: `None` without a plan fence. The fence
    /// must open at the start of a line, so a plan quoted in a reply (`> `) or
    /// indented is not one.
    pub fn from_comment(body: &str) -> Result<Option<Self>> {
        let open = format!("```{PLAN_FENCE}");
        let mut lines = body.lines();
        let mut found = None;
        while let Some(line) = lines.next() {
            if line.trim_end() != open {
                continue;
            }
            ensure!(found.is_none(), "comment holds more than one plan");
            let mut json = String::new();
            let mut closed = false;
            for line in lines.by_ref() {
                if line.trim_end() == "```" {
                    closed = true;
                    break;
                }
                json.push_str(line);
                json.push('\n');
            }
            ensure!(closed, "plan fence is not closed");
            let value: Value = serde_json::from_str(&json).context("plan is not valid JSON")?;
            found = Some(Self::from_json(&value)?);
        }
        Ok(found)
    }

    /// The REST request that dispatches this plan, as the GitHub API takes it.
    /// The inputs are dispatch.yml's, and the plan's own: nothing is added.
    pub fn dispatch_request(&self, caller: &str, git_ref: &str) -> Result<Value> {
        validate_caller(caller, git_ref)?;
        Ok(json!({
            "method": "POST",
            "url": format!("{API_ROOT}repos/{caller}/actions/workflows/{DISPATCH_WORKFLOW}/dispatches"),
            "body": {
                "ref": git_ref,
                "inputs": {
                    "task": self.brief,
                    "repo": self.repo,
                    // workflow_dispatch inputs are strings.
                    "item": self.item.to_string(),
                    "kind": self.kind,
                    "pool": self.pool,
                },
            },
        }))
    }
}

/// The caller repository and the ref its dispatch.yml runs from.
pub fn validate_caller(caller: &str, git_ref: &str) -> Result<()> {
    validate_repository(caller).context("invalid caller repository")?;
    ensure!(
        !git_ref.is_empty()
            && !git_ref.starts_with('-')
            && !git_ref.contains("..")
            && git_ref
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)),
        "invalid caller ref"
    );
    Ok(())
}

/// An issue comment, as far as plans need it.
#[derive(Clone, Debug)]
pub struct Comment {
    pub url: String,
    pub body: String,
    /// Absent for a deleted account.
    pub author: Option<String>,
    /// Whether it was edited (`lastEditedAt` is set), kept apart from `editor`,
    /// which GitHub leaves null when it cannot name the editing account.
    pub edited: bool,
    /// Who last edited it, if anyone: whoever may edit the comment, not only its author.
    pub editor: Option<String>,
    /// When it last changed: its last edit, else its creation.
    pub changed: OffsetDateTime,
}

/// The plan an issue's comments settle on, with the comment it came from.
///
/// Only comments by `writers` count (the operator and the planning run's
/// login); anyone else's plan is ignored, so a visitor cannot dispatch work or
/// block a dispatch. Of those, the most recently written or edited plan wins,
/// which is how a writer replaces a plan. It must be valid and, if edited,
/// edited by a writer too: GitHub lets anyone with write access edit others'
/// comments, so a collaborator could otherwise rewrite the planner's plan. A
/// newest plan that fails either check is an error, never a reason to fall
/// back to an older plan its writer meant to replace. That includes a writer's
/// comment edited by someone else to no longer hold a plan: it is weighed
/// before fences are, so removing a plan's fence cannot revive the one before.
pub fn newest_plan<'a>(
    comments: &'a [Comment],
    writers: &[&str],
) -> Result<(JobPlan, &'a Comment)> {
    for writer in writers {
        ensure!(is_github_login(writer), "invalid login {writer:?}");
    }
    let trusted = |login: &Option<String>| {
        login
            .as_deref()
            .is_some_and(|l| writers.iter().any(|w| w.eq_ignore_ascii_case(l)))
    };
    // An edit counts as a writer's only when GitHub names a writer as its editor.
    let foreign_edit = |c: &Comment| (c.edited || c.editor.is_some()) && !trusted(&c.editor);
    let newest = comments
        .iter()
        .filter(|c| trusted(&c.author) && (has_fence(&c.body) || foreign_edit(c)))
        // max_by_key keeps the last of equals: the later comment wins a tie.
        .max_by_key(|c| c.changed)
        .context("no plan on this issue from an accepted writer")?;
    ensure!(
        !foreign_edit(newest),
        "newest plan {} was last edited by {}, who may not write plans",
        newest.url,
        newest
            .editor
            .as_deref()
            .unwrap_or("an unidentified account")
    );
    let plan = JobPlan::from_comment(&newest.body)
        .with_context(|| format!("newest plan {}", newest.url))?
        .context("plan fence vanished")?;
    Ok((plan, newest))
}

fn has_fence(body: &str) -> bool {
    let open = format!("```{PLAN_FENCE}");
    body.lines().any(|line| line.trim_end() == open)
}

fn login(node: &Value, key: &str) -> Option<String> {
    node[key]["login"].as_str().map(str::to_owned)
}

fn comment(node: &Value) -> Result<Comment> {
    let field = |key: &str| {
        node[key]
            .as_str()
            .with_context(|| format!("comment has no {key}"))
    };
    let (changed, edited) = match &node["lastEditedAt"] {
        Value::String(at) => (at.as_str(), true),
        Value::Null => (field("createdAt")?, false),
        _ => bail!("comment lastEditedAt is not a time"),
    };
    Ok(Comment {
        url: field("url")?.into(),
        body: field("body")?.into(),
        author: login(node, "author"),
        edited,
        editor: login(node, "editor"),
        changed: OffsetDateTime::parse(changed, &Rfc3339).context("comment time")?,
    })
}

/// Every comment on an issue, in order. A pull request is refused: plans
/// live on issues.
pub fn issue_comments<T: Transport>(transport: &T, issue: &ItemRef) -> Result<Vec<Comment>> {
    ensure!(
        issue.host == crate::github::DEFAULT_HOST,
        "plans are read only from github.com"
    );
    validate_repository(&issue.repository)?;
    let (owner, name) = issue
        .repository
        .split_once('/')
        .context("issue repository")?;
    let mut comments = Vec::new();
    let mut after = Value::Null;
    for _ in 0..PLAN_COMMENT_PAGES {
        let data = transport.graphql(
            COMMENTS_QUERY,
            json!({"owner": owner, "name": name, "number": issue.number, "after": after}),
        )?;
        let connection = data
            .pointer("/repository/issue/comments")
            .filter(|c| c.is_object())
            .with_context(|| format!("{} is not an issue", issue.display()))?;
        for node in connection["nodes"]
            .as_array()
            .context("comments have no nodes")?
        {
            comments.push(comment(node)?);
        }
        match connection["pageInfo"]["hasNextPage"].as_bool() {
            Some(false) => return Ok(comments),
            Some(true) => {
                after = connection["pageInfo"]["endCursor"].clone();
                ensure!(after.is_string(), "comments page has no cursor");
            }
            None => bail!("comments page has no pageInfo"),
        }
    }
    bail!(
        "{} has more than {} comments; its newest plan could be unread",
        issue.display(),
        PLAN_COMMENT_PAGES * 100
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn plan() -> Value {
        json!({
            "schema": JOB_PLAN_SCHEMA,
            "repo": "ostreedev/ostree",
            "item": 3665,
            "kind": "implement",
            "runner": "hosted",
            "pool": "claude",
            "brief": "Follow up on the review of ostreedev/ostree#3665.",
        })
    }

    fn body(plan: &Value) -> String {
        format!("Plan for this issue:\n\n```{PLAN_FENCE}\n{plan:#}\n```\n")
    }

    fn at(time: &str) -> OffsetDateTime {
        OffsetDateTime::parse(time, &Rfc3339).unwrap()
    }

    fn comment(n: u32, author: &str, editor: Option<&str>, changed: &str, body: String) -> Comment {
        Comment {
            url: format!("https://github.com/cgwalters-forge/tracker/issues/540#issuecomment-{n}"),
            body,
            author: Some(author.into()),
            edited: editor.is_some(),
            editor: editor.map(Into::into),
            changed: at(changed),
        }
    }

    #[test]
    fn plan_validation_table() {
        let valid = JobPlan::from_json(&plan()).unwrap();
        assert_eq!(valid.item, 3665);
        let mut no_pool = plan();
        no_pool.as_object_mut().unwrap().remove("pool");
        assert_eq!(JobPlan::from_json(&no_pool).unwrap().pool, "");
        // Each edit of a valid plan, and the error it must give.
        let long = "x".repeat(PLAN_BRIEF_LIMIT + 1);
        for (key, value, error) in [
            (
                "schema",
                json!("agent-board/job-plan/v2"),
                "unsupported plan schema",
            ),
            ("schema", Value::Null, "schema is not a string"),
            ("repo", json!("ostreedev/ostree/../x"), "invalid repository"),
            ("repo", json!("ostreedev"), "invalid repository"),
            ("item", json!(0), "positive issue number"),
            ("item", json!(-1), "positive issue number"),
            ("item", json!("3665"), "positive issue number"),
            ("item", json!(1.5), "positive issue number"),
            ("kind", json!("fix"), "is not one of"),
            ("kind", json!("Implement"), "is not one of"),
            ("runner", json!("devspace"), "only hosted"),
            (
                "runner",
                json!({"devspace": {"cores": 8}}),
                "runner is not a string",
            ),
            ("pool", Value::Null, "pool is not a string"),
            ("pool", json!("../claude"), "lowercase name"),
            ("brief", json!("  \n"), "brief is empty"),
            ("brief", json!(long), "exceeds"),
            // Fields the caller has no input for are refused, not dropped.
            ("budget", json!(500), "unknown plan field \"budget\""),
            ("base", json!("main"), "unknown plan field \"base\""),
        ] {
            let mut edited = plan();
            edited[key] = value;
            let message = format!("{:#}", JobPlan::from_json(&edited).unwrap_err());
            assert!(message.contains(error), "{key}: {message}");
        }
        for key in ["schema", "repo", "item", "kind", "runner", "brief"] {
            let mut missing = plan();
            missing.as_object_mut().unwrap().remove(key);
            let error = JobPlan::from_json(&missing).unwrap_err().to_string();
            assert_eq!(error, format!("plan has no {key}"));
        }
        assert!(JobPlan::from_json(&json!([plan()])).is_err());
    }

    #[test]
    fn plan_fence_table() {
        let open = format!("```{PLAN_FENCE}");
        let plan_text = format!("{:#}", plan());
        for (body, expected) in [
            ("No plan here.".to_owned(), Ok(false)),
            (body(&plan()), Ok(true)),
            (format!("{open}  \r\n{plan_text}\n```"), Ok(true)),
            // Quoted, indented or differently labelled fences are not plans.
            (format!("> {open}\n> {plan_text}\n> ```"), Ok(false)),
            (format!("    {open}\n{plan_text}\n```"), Ok(false)),
            (format!("```json\n{plan_text}\n```"), Ok(false)),
            (format!("{open}\n{plan_text}\n"), Err("not closed")),
            (
                format!("{}\n{}", body(&plan()), body(&plan())),
                Err("more than one plan"),
            ),
            (format!("{open}\n{{\"schema\":\n```"), Err("not valid JSON")),
            (format!("{open}\n```"), Err("not valid JSON")),
        ] {
            match (JobPlan::from_comment(&body), expected) {
                (Ok(found), Ok(expected)) => assert_eq!(found.is_some(), expected, "{body}"),
                (Err(error), Err(expected)) => {
                    assert!(format!("{error:#}").contains(expected), "{body}: {error:#}")
                }
                (result, _) => panic!("{body}: {result:?}"),
            }
        }
    }

    #[test]
    fn newest_plan_from_writers_table() {
        const OPERATOR: &str = "cgwalters";
        const PLANNER: &str = "cgwalters-bot";
        let mut replaced = plan();
        replaced["kind"] = json!("research");
        let mut invalid = plan();
        invalid["runner"] = json!("devspace");
        let planned = || comment(1, PLANNER, None, "2026-10-13T10:00:00Z", body(&plan()));
        // Comments in issue order, then the comment number and kind expected, or an error.
        for (comments, expected) in [
            (vec![planned()], Ok((1, "implement"))),
            (
                vec![
                    comment(1, "someone", None, "2026-10-13T10:00:00Z", body(&plan())),
                    comment(2, PLANNER, None, "2026-10-13T09:00:00Z", "No plan.".into()),
                ],
                Err("no plan on this issue"),
            ),
            // The operator replaces a plan by replying with another.
            (
                vec![
                    planned(),
                    comment(2, OPERATOR, None, "2026-10-13T11:00:00Z", body(&replaced)),
                ],
                Ok((2, "research")),
            ),
            // Logins compare as GitHub does, without case.
            (
                vec![
                    planned(),
                    comment(
                        2,
                        "CGWalters",
                        None,
                        "2026-10-13T11:00:00Z",
                        body(&replaced),
                    ),
                ],
                Ok((2, "research")),
            ),
            // Or by editing an older one, which makes it the newest.
            (
                vec![
                    comment(
                        1,
                        PLANNER,
                        Some(OPERATOR),
                        "2026-10-13T12:00:00Z",
                        body(&replaced),
                    ),
                    comment(2, PLANNER, None, "2026-10-13T11:00:00Z", body(&plan())),
                ],
                Ok((1, "research")),
            ),
            // Anyone else's plan, however new, is ignored.
            (
                vec![
                    planned(),
                    comment(2, "visitor", None, "2026-10-13T11:00:00Z", body(&replaced)),
                    comment(3, "visitor", None, "2026-10-13T12:00:00Z", body(&invalid)),
                ],
                Ok((1, "implement")),
            ),
            // Equal times: the later comment wins.
            (
                vec![
                    planned(),
                    comment(2, OPERATOR, None, "2026-10-13T10:00:00Z", body(&replaced)),
                ],
                Ok((2, "research")),
            ),
            // A writer's newest plan that is invalid, or edited by someone
            // else, stops dispatch rather than reviving the plan it replaced.
            (
                vec![
                    planned(),
                    comment(2, OPERATOR, None, "2026-10-13T11:00:00Z", body(&invalid)),
                ],
                Err("issuecomment-2: plan runner \"devspace\" is not available"),
            ),
            (
                vec![
                    planned(),
                    comment(
                        2,
                        PLANNER,
                        Some("collaborator"),
                        "2026-10-13T11:00:00Z",
                        body(&replaced),
                    ),
                ],
                Err("last edited by collaborator"),
            ),
            // An edit whose editor GitHub cannot name is not a writer's.
            (
                vec![
                    planned(),
                    Comment {
                        edited: true,
                        ..comment(2, PLANNER, None, "2026-10-13T11:00:00Z", body(&replaced))
                    },
                ],
                Err("last edited by an unidentified account"),
            ),
            // Nor can an outsider's edit removing the newest plan's fence
            // revive the plan it replaced.
            (
                vec![
                    planned(),
                    comment(
                        2,
                        PLANNER,
                        Some("collaborator"),
                        "2026-10-13T11:00:00Z",
                        body(&replaced).replace(PLAN_FENCE, "json"),
                    ),
                ],
                Err("issuecomment-2 was last edited by collaborator"),
            ),
            // An outsider's edit of a writer's older comment is overtaken by
            // a newer plan.
            (
                vec![
                    comment(
                        1,
                        PLANNER,
                        Some("collaborator"),
                        "2026-10-13T09:00:00Z",
                        "No plan.".into(),
                    ),
                    comment(2, OPERATOR, None, "2026-10-13T11:00:00Z", body(&plan())),
                ],
                Ok((2, "implement")),
            ),
            (
                vec![Comment {
                    author: None,
                    ..planned()
                }],
                Err("no plan on this issue"),
            ),
        ] {
            let result = newest_plan(&comments, &[OPERATOR, PLANNER]);
            match (result, expected) {
                (Ok((plan, from)), Ok((n, kind))) => {
                    assert!(from.url.ends_with(&format!("-{n}")), "{}", from.url);
                    assert_eq!(plan.kind, kind);
                }
                (Err(error), Err(expected)) => {
                    let error = format!("{error:#}");
                    assert!(error.contains(expected), "{error}");
                }
                (result, expected) => panic!("{:?} != {expected:?}", result.map(|r| r.0)),
            }
        }
        // Only the operator: the planner's plan no longer counts.
        assert!(newest_plan(&[planned()], &[OPERATOR]).is_err());
        assert!(newest_plan(&[planned()], &["bad login"]).is_err());
    }

    #[test]
    fn dispatch_request_carries_only_the_plan() {
        let plan = JobPlan::from_json(&plan()).unwrap();
        assert_eq!(
            plan.dispatch_request("cgwalters-forge/tracker", "main")
                .unwrap(),
            json!({
                "method": "POST",
                "url": "https://api.github.com/repos/cgwalters-forge/tracker/actions/workflows/dispatch.yml/dispatches",
                "body": {"ref": "main", "inputs": {
                    "task": "Follow up on the review of ostreedev/ostree#3665.",
                    "repo": "ostreedev/ostree",
                    "item": "3665",
                    "kind": "implement",
                    "pool": "claude",
                }},
            })
        );
        for (caller, git_ref) in [
            ("cgwalters-forge/tracker?x=1", "main"),
            ("cgwalters-forge", "main"),
            ("cgwalters-forge/tracker", ""),
            ("cgwalters-forge/tracker", "--force"),
            ("cgwalters-forge/tracker", "a..b"),
            ("cgwalters-forge/tracker", "main branch"),
        ] {
            assert!(
                plan.dispatch_request(caller, git_ref).is_err(),
                "{caller} {git_ref}"
            );
        }
    }

    #[test]
    fn issue_comments_pages_and_refusals() {
        /// Serves `pages` in turn and records each cursor asked for.
        struct Pages {
            pages: Vec<Value>,
            cursors: RefCell<Vec<Value>>,
        }
        impl Transport for Pages {
            fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
                assert_eq!(query, COMMENTS_QUERY);
                assert_eq!(variables["owner"], "cgwalters-forge");
                let mut cursors = self.cursors.borrow_mut();
                cursors.push(variables["after"].clone());
                Ok(self.pages[(cursors.len() - 1).min(self.pages.len() - 1)].clone())
            }
        }
        let node = json!({
            "url": "https://github.com/cgwalters-forge/tracker/issues/540#issuecomment-1",
            "body": body(&plan()), "createdAt": "2026-10-13T10:00:00Z", "lastEditedAt": null,
            "author": {"login": "cgwalters-bot"}, "editor": null,
        });
        let page = |next: bool, nodes: Value| {
            json!({"repository": {"issue": {"comments": {"nodes": nodes,
                "pageInfo": {"hasNextPage": next, "endCursor": if next { json!("c") } else { Value::Null }}}}}})
        };
        let mut edited = node.clone();
        edited["lastEditedAt"] = json!("2026-10-13T12:00:00Z");
        edited["editor"] = json!({"login": "cgwalters"});
        let issue = ItemRef {
            host: "github.com".into(),
            repository: "cgwalters-forge/tracker".into(),
            number: 540,
        };
        let read = |pages: Vec<Value>| {
            let transport = Pages {
                pages,
                cursors: RefCell::new(vec![]),
            };
            let result = issue_comments(&transport, &issue);
            (result, transport.cursors.into_inner().len())
        };
        let (comments, requests) = read(vec![
            page(true, json!([node])),
            page(false, json!([edited])),
        ]);
        let comments = comments.unwrap();
        assert_eq!(requests, 2);
        assert_eq!(comments[1].changed, at("2026-10-13T12:00:00Z"));
        assert_eq!(comments[1].editor.as_deref(), Some("cgwalters"));
        assert!(comments[1].edited);
        assert_eq!(comments[0].editor, None);
        assert!(!comments[0].edited);
        // An edit is kept as a fact even when GitHub cannot name its editor.
        let mut ghost = edited.clone();
        ghost["editor"] = Value::Null;
        let comments = read(vec![page(false, json!([ghost]))]).0.unwrap();
        assert!(comments[0].edited && comments[0].editor.is_none());
        for (pages, error, requests) in [
            (
                vec![page(true, json!([]))],
                "more than 1000 comments",
                PLAN_COMMENT_PAGES,
            ),
            (
                vec![json!({"repository": {"issue": null}})],
                "is not an issue",
                1,
            ),
            (
                vec![json!({"repository": {"issue": {"comments": {"nodes": []}}}})],
                "no pageInfo",
                1,
            ),
            (
                vec![page(false, json!([{"url": "u"}]))],
                "comment has no createdAt",
                1,
            ),
            (
                vec![page(false, json!([{"url": "u", "lastEditedAt": 1}]))],
                "lastEditedAt is not a time",
                1,
            ),
        ] {
            let (result, made) = read(pages);
            let message = format!("{:#}", result.unwrap_err());
            assert!(message.contains(error), "{message}");
            assert_eq!(made, requests);
        }
        for issue in [
            ItemRef {
                host: "github.example".into(),
                ..issue.clone()
            },
            ItemRef {
                repository: "a/b?c".into(),
                ..issue.clone()
            },
        ] {
            let transport = Pages {
                pages: vec![],
                cursors: RefCell::new(vec![]),
            };
            assert!(issue_comments(&transport, &issue).is_err());
            assert!(transport.cursors.borrow().is_empty());
        }
    }
}
