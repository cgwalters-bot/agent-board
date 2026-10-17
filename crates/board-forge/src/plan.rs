//! Job plans: a job proposal written as a fenced block in an issue comment
//! (cgwalters-forge/agent-board#46), and the `workflow_dispatch` request that
//! starts exactly that job on the caller's dispatch.yml. Schema: docs/job-plans.md.
//!
//! A plan carries only what the caller's dispatch.yml (cgwalters-forge/tracker)
//! has an input for, so nothing in it is silently dropped on the way; the
//! caller's bounds still decide whether the run may do what the plan asks.
use crate::batch::{BATCH_LIMIT, batch_line, held};
use crate::github::{API_ROOT, Transport, is_github_login};
use crate::overlap::Decision;
use crate::poll::{Rest, validate_repository, validate_workflow};
use anyhow::{Context, Result, bail, ensure};
use board_core::ItemRef;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub const JOB_PLAN_SCHEMA: &str = "agent-board/job-plan/v1";
/// The info string of the fence that holds a plan: ```` ```agent-board-plan ````.
pub const PLAN_FENCE: &str = "agent-board-plan";
/// The tag of the hidden first line of the comment that records a plan's
/// dispatch: `<!-- agent-board-dispatch: PLAN-ID -->`.
pub const DISPATCH_MARKER: &str = "agent-board-dispatch";
/// Characters of a failed dispatch's error kept in its record.
const ERROR_LIMIT: usize = 200;
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
const FIELDS: &[&str] = &[
    "schema",
    "repo",
    "item",
    "kind",
    "runner",
    "pool",
    "brief",
    "stacks_on",
];

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
    /// An issue in `repo` whose open pull request this run builds on
    /// ([`crate::stack`]): an implement run only, and never the plan's own item.
    pub stacks_on: Option<u64>,
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
        let stacks_on = match object.get("stacks_on") {
            None => None,
            Some(lower) => Some(
                lower
                    .as_u64()
                    .filter(|n| (1..1 << 53).contains(n) && *n != item)
                    .context("plan stacks_on is not another positive issue number")?,
            ),
        };
        // Only an implement run opens a pull request to stack.
        ensure!(
            stacks_on.is_none() || kind == "implement",
            "plan stacks_on needs kind implement"
        );
        Ok(Self {
            repo: repo.into(),
            item,
            kind: kind.into(),
            runner: runner.into(),
            pool: pool.into(),
            brief: brief.into(),
            stacks_on,
        })
    }

    /// The plan from a comment body: `None` without a plan fence. The fence
    /// must open at the start of a line, so a plan quoted in a reply (`> `) or
    /// indented is not one.
    pub fn from_comment(body: &str) -> Result<Option<Self>> {
        fenced_json(body, PLAN_FENCE)?
            .map(|value| Self::from_json(&value))
            .transpose()
    }

    /// The REST request that dispatches this plan, as the GitHub API takes it.
    /// The inputs are dispatch.yml's, and the plan's own; a stacked plan's
    /// `base` is the branch [`crate::stack::resolve`] found, and only a
    /// stacked plan sends one, so a caller without that input is unaffected.
    pub fn dispatch_request(&self, target: &Target, base: Option<&str>) -> Result<Value> {
        target.validate()?;
        ensure!(
            base.is_some() == self.stacks_on.is_some(),
            "a base branch is sent for a stacked plan only, and always for one"
        );
        if let Some(base) = base {
            crate::stack::validate_branch(base)?;
        }
        let Target {
            caller,
            workflow,
            git_ref,
        } = target;
        let mut request = json!({
            "method": "POST",
            "url": format!("{API_ROOT}repos/{caller}/actions/workflows/{workflow}/dispatches"),
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
        });
        if let Some(base) = base {
            request["body"]["inputs"]["base"] = base.into();
        }
        Ok(request)
    }
}

/// Where plans are dispatched: the caller's workflow and the ref it runs
/// from. These come from the caller's own configuration, never from a plan.
pub struct Target {
    /// Caller repository, OWNER/NAME.
    pub caller: String,
    /// Its workflow file that takes a plan's inputs, such as dispatch.yml.
    pub workflow: String,
    pub git_ref: String,
}

impl Target {
    /// Each part goes into an API path that carries the dispatch token.
    pub fn validate(&self) -> Result<()> {
        validate_repository(&self.caller).context("invalid caller repository")?;
        validate_workflow(&self.workflow)?;
        let git_ref = &self.git_ref;
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
    let newest = newest_fenced(comments, writers, PLAN_FENCE)?;
    let plan = JobPlan::from_comment(&newest.body)
        .with_context(|| format!("newest plan {}", newest.url))?
        .context("plan fence vanished")?;
    Ok((plan, newest))
}

/// The comment holding the newest `fence` block by a writer, by the rules of
/// [`newest_plan`], which epic plans share; the caller parses the block.
pub(crate) fn newest_fenced<'a>(
    comments: &'a [Comment],
    writers: &[&str],
    fence: &str,
) -> Result<&'a Comment> {
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
        .filter(|c| trusted(&c.author) && (has_fence(&c.body, fence) || foreign_edit(c)))
        // max_by_key keeps the last of equals: the later comment wins a tie.
        .max_by_key(|c| c.changed)
        .with_context(|| format!("no plan on this issue from an accepted writer ({fence})"))?;
    ensure!(
        !foreign_edit(newest),
        "newest plan {} was last edited by {}, who may not write plans",
        newest.url,
        newest
            .editor
            .as_deref()
            .unwrap_or("an unidentified account")
    );
    Ok(newest)
}

fn has_fence(body: &str, fence: &str) -> bool {
    let open = format!("```{fence}");
    body.lines().any(|line| line.trim_end() == open)
}

/// The JSON of the one `fence` block in a comment body: `None` without one.
/// The fence must open at the start of a line, so a block quoted in a reply
/// (`> `) or indented is not one.
pub(crate) fn fenced_json(body: &str, fence: &str) -> Result<Option<Value>> {
    let open = format!("```{fence}");
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
        found = Some(serde_json::from_str(&json).context("plan is not valid JSON")?);
    }
    Ok(found)
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

/// A plan's identity: its comment's id and when the comment last changed. A
/// writer's edit is a new plan, which may be dispatched; the same plan is not
/// dispatched twice.
pub fn plan_id(comment: &Comment) -> Result<String> {
    let id = comment_id(&comment.url).with_context(|| format!("{} has no id", comment.url))?;
    Ok(format!("{id}@{}", comment.changed.format(&Rfc3339)?))
}

fn comment_id(url: &str) -> Option<u64> {
    url.rsplit_once("#issuecomment-")?.1.parse().ok()
}

fn marker_line(plan: &str) -> String {
    format!("<!-- {DISPATCH_MARKER}: {plan} -->")
}

/// The comments by `logins` that record dispatching `plan`, oldest first.
/// Only the writers' count, so a visitor cannot block a plan with a copied
/// marker, while an operator's dispatch with their own token still holds off
/// the bot's. Whoever may edit or delete comments on
/// the issue can remove a record and so have its plan dispatched again; that
/// is repository write access, which can start the caller's workflows anyway
/// when it is the same repository.
pub(crate) fn records<'a>(
    comments: &'a [Comment],
    logins: &[&str],
    plan: &str,
) -> Vec<(u64, &'a Comment)> {
    let marker = marker_line(plan);
    comments
        .iter()
        .filter(|c| {
            c.author
                .as_deref()
                .is_some_and(|a| logins.iter().any(|l| l.eq_ignore_ascii_case(a)))
                && c.body.lines().next().map(str::trim_end) == Some(marker.as_str())
        })
        // A record without an id is taken for the earliest: it blocks the plan.
        .map(|c| (comment_id(&c.url).unwrap_or(0), c))
        .collect()
}

/// The login of the account a token belongs to.
pub(crate) fn viewer<T: Transport>(transport: &T) -> Result<String> {
    let data = transport.graphql("query { viewer { login } }", json!({}))?;
    let login = data
        .pointer("/viewer/login")
        .and_then(Value::as_str)
        .filter(|l| is_github_login(l))
        .context("GitHub did not name the token's account")?;
    Ok(login.to_owned())
}

/// One write, which must succeed unless its status is `allowed`.
pub(crate) fn send<R: Rest>(
    client: &R,
    method: &str,
    url: &str,
    body: &Value,
    allowed: u16,
) -> Result<(u16, Value)> {
    let (status, response) = client.send(method, url, body)?;
    ensure!(
        (200..300).contains(&status) || status == allowed,
        "{method} {}: GitHub HTTP status {status}: {}",
        url.trim_start_matches(API_ROOT),
        response["message"]
            .as_str()
            .unwrap_or("message unavailable")
    );
    Ok((status, response))
}

/// What a dispatch did: the comment that records it, its run where GitHub
/// names it, what a batch added, and what the overlap check decided.
#[derive(Debug)]
pub struct Dispatched {
    /// `None` when the overlap check holds the run back: nothing is recorded.
    pub record: Option<String>,
    pub run: Option<String>,
    /// The extra issues the run also does, OWNER/REPO#N.
    pub batched: Vec<String>,
    /// Why candidates for the batch were left out.
    pub notes: Vec<String>,
    /// With a `bot`, what [`crate::overlap::decide`] decided.
    pub overlap: Option<Decision>,
}

/// [`crate::input::check`] each issue a run reads: the one holding the plan,
/// the one it targets, and each extra of its batch. A target in a repository
/// the token cannot read collaborators of is refused with the rest.
pub fn check_inputs<T: Transport + Rest>(
    client: &T,
    issue: &ItemRef,
    dest: &ItemRef,
    extras: &[crate::batch::Extra],
) -> Result<()> {
    let mut items = vec![issue, dest];
    items.extend(extras.iter().map(|e| &e.issue));
    items.sort_by_key(|i| (i.repository.to_ascii_lowercase(), i.number));
    items.dedup_by_key(|i| (i.repository.to_ascii_lowercase(), i.number));
    for item in items {
        crate::input::check(client, item).context("not dispatched: --collaborators-only")?;
    }
    Ok(())
}

/// A plan as [`stack`] leaves it: the plan to dispatch, the base its run
/// starts from, and the sentence its record carries.
pub type Stacked = (JobPlan, Option<crate::stack::Base>, String);

/// What a plan's `stacks_on` makes of it ([`crate::stack`]): on the lower
/// slice's open pull request, the plan with the stack in its brief and that
/// pull request's branch as its base; with the lower slice closed, the plan
/// unstacked; with neither, `Err` with the wait. A plan that stacks on
/// nothing is returned as it is, and reads nothing.
pub fn stack<T: Transport>(
    client: &T,
    dest: &ItemRef,
    plan: JobPlan,
    bot: Option<&str>,
) -> Result<std::result::Result<Stacked, Decision>> {
    let Some(lower) = plan.stacks_on else {
        return Ok(Ok((plan, None, String::new())));
    };
    // Whose pull requests count is the caller's configuration, never the plan's.
    let bot = bot.context(
        "a plan that stacks_on another issue needs --bot, whose pull request its run starts from",
    )?;
    Ok(match crate::stack::resolve(client, dest, lower, bot)? {
        crate::stack::Stack::Wait(why) => Err(Decision::Wait(why)),
        crate::stack::Stack::Unstacked(why) => Ok((
            JobPlan {
                stacks_on: None,
                ..plan
            },
            None,
            format!(" Not stacked: {why}."),
        )),
        crate::stack::Stack::On(base) => {
            let sentence = format!(" Stacked on {} (`{}`).", base.url, base.branch);
            let brief = crate::stack::brief(&plan.brief, dest, lower, &base);
            Ok((JobPlan { brief, ..plan }, Some(base), sentence))
        }
    })
}

/// One issue's record of a dispatch: the comment and the plan it records.
struct Record {
    issue: ItemRef,
    plan: String,
    plan_url: String,
    /// Its hidden lines, which every edit keeps.
    header: String,
    id: u64,
    url: String,
}

/// Start the job that the newest accepted plan on `issue` proposes, once.
///
/// `client` reads the issue and writes the one comment that records the
/// dispatch, and must be a writer's. `dispatcher` starts
/// the run. The record goes first and is read back: GitHub cannot
/// compare-and-swap a comment, so of two passes that read the same plan, both
/// post a record and only the one with the earlier goes on. A pass that dies
/// after its record, or a dispatch that fails, leaves the plan recorded and
/// not dispatched again, since an ambiguous dispatch must not be retried
/// blindly (docs/design.md); a writer edits or replaces the plan to try again.
///
/// With `batch` above zero the run also does up to that many extra issues
/// ([`crate::batch`]), each recorded and read back the same way before the
/// one run starts. A conflict on the primary starts nothing; a conflict on an
/// extra only leaves that extra out, and its record says so. An extra whose
/// read-back fails is left out too, but its record is released (its marker
/// removed), since it is known not to be in the run and nothing confirmed
/// another claim: a later pass may dispatch it.
///
/// With a `bot`, the issue is first checked against the bot's open pull
/// requests and the runs in flight ([`crate::overlap`]): a wait returns before
/// anything is written, and any other decision goes into every record.
///
/// With `collaborators`, every issue the run reads is checked with
/// [`crate::input::check`] before anything is written, and one that fails
/// refuses the whole dispatch.
#[allow(clippy::too_many_arguments)]
pub fn dispatch<C: Transport + Rest, D: Rest>(
    client: &C,
    dispatcher: &D,
    issue: &ItemRef,
    writers: &[&str],
    target: &Target,
    batch: usize,
    bot: Option<&str>,
    collaborators: bool,
) -> Result<Dispatched> {
    target.validate()?;
    ensure!(
        batch <= BATCH_LIMIT,
        "a batch has at most {BATCH_LIMIT} extra issues"
    );
    if let Some(bot) = bot {
        ensure!(is_github_login(bot), "invalid login {bot:?}");
    }
    let comments = issue_comments(client, issue)?;
    let (plan, from) = newest_plan(&comments, writers)?;
    let id = plan_id(from)?;
    // Records count only from writers, so every dispatching account must be
    // one: two accounts that each read only their own records would both
    // dispatch, as would a new account after a token is rotated.
    let login = viewer(client)?;
    ensure!(
        writers.iter().any(|w| w.eq_ignore_ascii_case(&login)),
        "{login} is not a plan writer; dispatch with the operator's or the planner's token"
    );
    if let Some((_, record)) = held(client, &comments, writers, &id, issue)?.first() {
        bail!(
            "the plan in {} has a dispatch record already, {}; edit or replace the plan to dispatch it again",
            from.url,
            record.url
        );
    }
    // The plan may change another repository than the issue it is on: its
    // pull requests and runs are that repository's.
    let dest = ItemRef {
        repository: plan.repo.clone(),
        number: plan.item,
        ..issue.clone()
    };
    let (plan, base, stacked) = match stack(client, &dest, plan, bot)? {
        Ok(stack) => stack,
        Err(wait) => {
            return Ok(Dispatched {
                record: None,
                run: None,
                batched: vec![],
                notes: vec![],
                overlap: Some(wait),
            });
        }
    };
    // A stacked run shares the files of the stack below it by design: its
    // plan, not a guess from paths, decided how it runs.
    let overlap = bot
        .filter(|_| base.is_none())
        .map(|bot| crate::overlap::decide(client, issue, &dest, &plan.brief, bot, target));
    if let Some(Decision::Wait(_)) = overlap {
        // Unrecorded, so the next pass decides again on fresh state.
        return Ok(Dispatched {
            record: None,
            run: None,
            batched: vec![],
            notes: vec![],
            overlap,
        });
    }
    let note = overlap
        .as_ref()
        .map(|d| format!(" {}", d.sentence()))
        .unwrap_or_default()
        + &stacked;
    // A batch's other issues would land on the lower slice's branch too.
    let batch = if base.is_some() { 0 } else { batch };
    let chosen = crate::batch::select(client, issue, &plan, writers, batch)?;
    if collaborators {
        check_inputs(client, issue, &dest, &chosen.extras)?;
    }
    let request = |extras: &[&crate::batch::Extra]| {
        JobPlan {
            brief: crate::batch::brief(&plan, extras),
            ..plan.clone()
        }
        .dispatch_request(target, base.as_ref().map(|b| b.branch.as_str()))
    };
    // Built before anything is written, so a bad request posts no record.
    request(&chosen.extras.iter().collect::<Vec<_>>())?;
    let post = |on: &ItemRef, plan: &str, plan_url: &str, header: String, text: String| {
        let (_, posted) = send(
            client,
            "POST",
            &format!(
                "{API_ROOT}repos/{}/issues/{}/comments",
                on.repository, on.number
            ),
            &json!({"body": format!("{header}\n{text}{note}")}),
            0,
        )?;
        anyhow::Ok(Record {
            issue: on.clone(),
            plan: plan.into(),
            plan_url: plan_url.into(),
            header,
            id: posted["id"].as_u64().context("the new comment has no id")?,
            url: posted["html_url"]
                .as_str()
                .context("the new comment has no URL")?
                .to_owned(),
        })
    };
    let edit = |record: &Record, text: String| {
        send(
            client,
            "PATCH",
            &format!(
                "{API_ROOT}repos/{}/issues/comments/{}",
                record.issue.repository, record.id
            ),
            &json!({"body": format!("{}\n{text}{note}", record.header)}),
            0,
        )
        .map(drop)
    };
    // Read again without a cache. Our own record counts even where the list
    // does not show it yet: only an earlier one stops this dispatch. This
    // relies on GitHub's reads showing comments posted before them; a caller
    // that runs dispatch in one concurrency group does not need it.
    // `Ok(Some(why))` is a confirmed conflict; `Err` a read that failed.
    let check = |record: &Record| -> Result<Option<String>> {
        let again = issue_comments(client, &record.issue)?;
        // A writer's edit since the first read is a new plan, for the next pass.
        let (_, now) = newest_plan(&again, writers)?;
        if plan_id(now)? != record.plan {
            return Ok(Some(
                "the plan changed while it was being dispatched".into(),
            ));
        }
        Ok(held(client, &again, writers, &record.plan, &record.issue)?
            .into_iter()
            .find(|(n, _)| *n < record.id)
            .map(|(_, first)| format!("{} recorded this plan first", first.url)))
    };
    let mut records = vec![post(
        issue,
        &id,
        &from.url,
        marker_line(&id),
        format!("Dispatching the plan in {}.", from.url),
    )?];
    // Extras left out, by index into `chosen.extras`, why, and whether their
    // record is released (the read-back failed rather than found a conflict).
    let mut dropped: Vec<(usize, String, bool)> = Vec::new();
    let started = (|| -> Result<Value> {
        for extra in &chosen.extras {
            records.push(post(
                &extra.issue,
                &extra.id,
                &extra.from.url,
                format!("{}\n{}", marker_line(&extra.id), batch_line(issue)),
                format!(
                    "Dispatching the plan in {} with {}.",
                    extra.from.url,
                    issue.display()
                ),
            )?);
        }
        if let Some(why) = check(&records[0])? {
            bail!(why);
        }
        // Another pass taking one extra first leaves only that extra out.
        for (n, record) in records.iter().enumerate().skip(1) {
            match check(record) {
                Ok(None) => {}
                Ok(Some(why)) => dropped.push((n - 1, why, false)),
                Err(why) => dropped.push((n - 1, format!("{why:#}"), true)),
            }
        }
        let taken: Vec<_> = (chosen.extras.iter().enumerate())
            .filter(|(n, _)| !dropped.iter().any(|(d, ..)| d == n))
            .map(|(_, e)| e)
            .collect();
        let request = request(&taken)?;
        let url = request["url"].as_str().context("dispatch request URL")?;
        let mut body = request["body"].clone();
        body["return_run_details"] = true.into();
        // The run id comes back only where the API knows `return_run_details`;
        // elsewhere it is refused with a 422, and asked again without.
        let (status, run) = send(dispatcher, "POST", url, &body, 422)?;
        if status != 422 {
            return Ok(run);
        }
        body.as_object_mut().map(|b| b.remove("return_run_details"));
        Ok(send(dispatcher, "POST", url, &body, 0)?.1)
    })();
    let run = match started {
        Ok(run) => run,
        Err(error) => {
            let why: String = format!("{error:#}").chars().take(ERROR_LIMIT).collect();
            let mut noted = Ok(());
            for record in &records {
                noted = noted.and(edit(record, format!(
                    "The plan in {} was not dispatched, or not knowingly ({why}). It is not tried again; edit or replace the plan to dispatch it.",
                    record.plan_url
                )));
            }
            return Err(match noted {
                Ok(()) => error,
                Err(also) => error.context(format!("its record was not updated: {also:#}")),
            });
        }
    };
    let run = run["workflow_run_id"]
        .as_u64()
        .map(|n| format!("https://github.com/{}/actions/runs/{n}", target.caller));
    let mut notes = chosen.notes;
    for (n, record) in records.iter().enumerate() {
        let left = n
            .checked_sub(1)
            .and_then(|e| dropped.iter().find(|(d, ..)| *d == e));
        let text = match (left, &run) {
            (Some((_, why, true)), _) => {
                let why: String = why.chars().take(ERROR_LIMIT).collect();
                notes.push(format!("{}: {why}", record.issue.display()));
                // Without its marker the comment no longer holds the plan.
                send(
                    client,
                    "PATCH",
                    &format!(
                        "{API_ROOT}repos/{}/issues/comments/{}",
                        record.issue.repository, record.id
                    ),
                    &json!({"body": format!(
                        "The plan in {} was left out of the run for {}: it could not be checked ({why}). A later dispatch may take it.{note}",
                        record.plan_url,
                        issue.display()
                    )}),
                    0,
                )
                .with_context(|| {
                    format!(
                        "dispatched {}, but {} still holds a plan it left out",
                        run.as_deref().unwrap_or("the plan"),
                        record.url
                    )
                })?;
                continue;
            }
            (Some((_, why, false)), _) => {
                let why: String = why.chars().take(ERROR_LIMIT).collect();
                notes.push(format!("{}: {why}", record.issue.display()));
                format!(
                    "The plan in {} was left out of the run for {} ({why}). It is not tried again; edit or replace the plan to dispatch it.",
                    record.plan_url,
                    issue.display()
                )
            }
            (None, Some(run)) if n > 0 => format!(
                "Dispatched the plan in {} with {}: [run]({run}).",
                record.plan_url,
                issue.display()
            ),
            (None, Some(run)) => {
                format!("Dispatched the plan in {}: [run]({run}).", record.plan_url)
            }
            (None, None) => format!(
                "Dispatched the plan in {}{} to {} {}; GitHub did not name the run.",
                record.plan_url,
                match n {
                    0 => String::new(),
                    _ => format!(" with {}", issue.display()),
                },
                target.caller,
                target.workflow
            ),
        };
        // The run started: say so, rather than only that the record is stale.
        edit(record, text).with_context(|| {
            format!(
                "dispatched {}, but {} still says it is starting",
                run.as_deref().unwrap_or("the plan"),
                record.url
            )
        })?;
    }
    Ok(Dispatched {
        record: Some(records[0].url.clone()),
        run,
        batched: (chosen.extras.iter().enumerate())
            .filter(|(n, _)| !dropped.iter().any(|(d, ..)| d == n))
            .map(|(_, e)| e.issue.display())
            .collect(),
        notes,
        overlap,
    })
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
            // The plan names the slice it stacks on, never a branch.
            ("stacks_on", Value::Null, "another positive issue number"),
            ("stacks_on", json!(0), "another positive issue number"),
            ("stacks_on", json!("3664"), "another positive issue number"),
            (
                "stacks_on",
                json!("dispatch/x"),
                "another positive issue number",
            ),
            ("stacks_on", json!(3665), "another positive issue number"),
        ] {
            let mut edited = plan();
            edited[key] = value;
            let message = format!("{:#}", JobPlan::from_json(&edited).unwrap_err());
            assert!(message.contains(error), "{key}: {message}");
        }
        let mut stacked = plan();
        stacked["stacks_on"] = json!(3664);
        assert_eq!(JobPlan::from_json(&stacked).unwrap().stacks_on, Some(3664));
        stacked["kind"] = json!("research");
        let message = JobPlan::from_json(&stacked).unwrap_err().to_string();
        assert!(message.contains("needs kind implement"), "{message}");
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
            plan.dispatch_request(&target(), None).unwrap(),
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
        let tracker = "cgwalters-forge/tracker";
        for (caller, workflow, git_ref) in [
            ("cgwalters-forge/tracker?x=1", "dispatch.yml", "main"),
            ("cgwalters-forge", "dispatch.yml", "main"),
            (tracker, "dispatch.yml", ""),
            (tracker, "dispatch.yml", "--force"),
            (tracker, "dispatch.yml", "a..b"),
            (tracker, "dispatch.yml", "main branch"),
            (tracker, "", "main"),
            (tracker, "../dispatch.yml", "main"),
            (tracker, "dispatch.yml?x=1", "main"),
            (tracker, "dispatch.sh", "main"),
        ] {
            let target = Target {
                caller: caller.into(),
                workflow: workflow.into(),
                git_ref: git_ref.into(),
            };
            assert!(
                plan.dispatch_request(&target, None).is_err(),
                "{caller} {workflow} {git_ref}"
            );
        }
        // Only a stacked plan sends a base, always one, and only a dispatch/ branch.
        let stacked = JobPlan {
            stacks_on: Some(3664),
            ..plan.clone()
        };
        let request = stacked
            .dispatch_request(&target(), Some("dispatch/3664"))
            .unwrap();
        assert_eq!(request["body"]["inputs"]["base"], "dispatch/3664");
        for (plan, base) in [
            (&plan, Some("dispatch/3664")),
            (&stacked, None),
            (&stacked, Some("main")),
        ] {
            assert!(plan.dispatch_request(&target(), base).is_err(), "{base:?}");
        }
    }

    fn target() -> Target {
        Target {
            caller: "cgwalters-forge/tracker".into(),
            workflow: "dispatch.yml".into(),
            git_ref: "main".into(),
        }
    }

    /// An issue's comments on GitHub, as far as dispatch reads and writes
    /// them, and the caller's workflow.
    struct Forge {
        /// GraphQL comment nodes, in creation order.
        comments: RefCell<Vec<Value>>,
        viewer: &'static str,
        /// The dispatch API's answers in turn; each request is recorded.
        answers: RefCell<Vec<(u16, Value)>>,
        dispatches: RefCell<Vec<Value>>,
        /// What goes wrong: "race" (another pass posts its record just before
        /// ours), "plan edited" (by a writer, as ours is posted), "record
        /// fails", "read-back fails" or "final edit fails".
        twist: &'static str,
        reads: std::cell::Cell<usize>,
        /// The comments of the primary's epic siblings, by issue number,
        /// for a batch ("extra race": another pass records #541 first;
        /// "extra unreadable": reading #541 back after its batch record fails).
        others: RefCell<std::collections::BTreeMap<u64, Vec<Value>>>,
        /// The answer to the batch's epic read; null when none is expected,
        /// "fail" when it fails.
        family: Value,
        closed: RefCell<Vec<u64>>,
        /// Issues an open pull request closes.
        pulled: RefCell<Vec<u64>>,
        /// For the overlap check, `{"text", "pulls", "runs"}`: the issue's text,
        /// the open pull requests and the titles of runs in flight; null when
        /// none is expected.
        overlap: Value,
        /// Items the collaborator check read, OWNER/REPO#N.
        checked: RefCell<Vec<String>>,
        /// For a stacked plan, the lower issue's state and the open pull
        /// requests ([`crate::stack::resolve`]); null when none is expected.
        stack: Value,
    }

    const ISSUE: &str = "https://github.com/cgwalters-forge/tracker/issues/540";
    const OPERATOR: &str = "cgwalters";
    const PLANNER: &str = "cgwalters-bot";

    fn node(id: u64, author: &str, at: &str, body: &str) -> Value {
        json!({"url": format!("{ISSUE}#issuecomment-{id}"), "body": body, "createdAt": at,
            "lastEditedAt": null, "author": {"login": author}, "editor": null})
    }

    fn record(plan: &str) -> String {
        format!("<!-- {DISPATCH_MARKER}: {plan} -->\nDispatched.")
    }

    impl Forge {
        fn new(comments: Vec<Value>, answers: Vec<(u16, Value)>) -> Self {
            Self {
                comments: RefCell::new(comments),
                viewer: PLANNER,
                answers: RefCell::new(answers),
                dispatches: RefCell::new(vec![]),
                twist: "",
                reads: Default::default(),
                others: Default::default(),
                family: Value::Null,
                closed: Default::default(),
                pulled: Default::default(),
                overlap: Value::Null,
                checked: Default::default(),
                stack: Value::Null,
            }
        }

        fn post(&self, author: &str, body: &str) -> u64 {
            let mut comments = self.comments.borrow_mut();
            let id = 100 + comments.len() as u64;
            comments.push(node(id, author, "2026-10-14T09:00:00Z", body));
            id
        }

        fn bodies(&self) -> Vec<String> {
            let comments = self.comments.borrow();
            comments
                .iter()
                .map(|c| c["body"].as_str().unwrap().to_owned())
                .collect()
        }
    }

    impl Transport for Forge {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            if query.contains("viewer") {
                return Ok(json!({"viewer": {"login": self.viewer}}));
            }
            if query.contains("issueOrPullRequest") {
                // Locked and written only by the operator, unless "unlocked".
                let repo = format!(
                    "{}/{}",
                    variables["owner"].as_str().unwrap(),
                    variables["name"].as_str().unwrap()
                );
                self.checked
                    .borrow_mut()
                    .push(format!("{repo}#{}", variables["number"]));
                return Ok(
                    json!({"repository": {"issueOrPullRequest": {"__typename": "Issue",
                    "locked": self.twist != "unlocked", "author": {"__typename": "User", "login": OPERATOR},
                    "userContentEdits": {"nodes": [], "pageInfo": {"hasNextPage": false}},
                    "comments": {"nodes": [], "pageInfo": {"hasNextPage": false}}}}}),
                );
            }
            if query.contains("subIssues") {
                assert!(!self.family.is_null(), "unexpected epic read");
                ensure!(self.family != "fail", "epic unreadable");
                return Ok(self.family.clone());
            }
            if query.contains("headRefName") {
                assert!(!self.stack.is_null(), "unexpected stack read");
                assert_eq!(
                    (&variables["name"], &variables["number"]),
                    (&json!("ostree"), &json!(3664))
                );
                return Ok(self.stack.clone());
            }
            if query.contains("pullRequests(") {
                assert!(!self.overlap.is_null(), "unexpected overlap read");
                // The plan's repository, not the issue's.
                assert_eq!(
                    (&variables["owner"], &variables["name"]),
                    (&json!("ostreedev"), &json!("ostree"))
                );
                return Ok(
                    json!({"repository": {"pullRequests": {"nodes": self.overlap["pulls"]}}}),
                );
            }
            let number = variables["number"].as_u64().unwrap();
            if query.contains("{ title body }") {
                // The task text is the issue's own.
                assert_eq!((&variables["name"], number), (&json!("tracker"), 540));
                return Ok(
                    json!({"repository": {"issue": {"title": "", "body": self.overlap["text"]}}}),
                );
            }
            if query.contains("includeClosedPrs: false") {
                let state = if self.pulled.borrow().contains(&number) {
                    "OPEN"
                } else {
                    "MERGED"
                };
                return Ok(
                    json!({"repository": {"issue": {"closedByPullRequestsReferences": {"nodes": [{"state": state}]}}}}),
                );
            }
            if query.contains("{ state }") {
                let closed = self.closed.borrow().contains(&number);
                let state = if closed { "CLOSED" } else { "OPEN" };
                return Ok(json!({"repository": {"issue": {"state": state}}}));
            }
            assert_eq!(query, COMMENTS_QUERY);
            if number != 540 {
                let starting = |c: &Value| {
                    c["body"].as_str().unwrap().ends_with(
                        "Dispatching the plan in https://github.com/cgwalters-forge/tracker/issues/541#issuecomment-541000 with cgwalters-forge/tracker#540.",
                    )
                };
                ensure!(
                    !(self.twist == "extra unreadable"
                        && number == 541
                        && self.others.borrow()[&541].iter().any(starting)),
                    "GitHub transport failed"
                );
                return Ok(json!({"repository": {"issue": {"comments": {
                    "nodes": self.others.borrow()[&number],
                    "pageInfo": {"hasNextPage": false, "endCursor": null}}}}}));
            }
            self.reads.set(self.reads.get() + 1);
            ensure!(
                !(self.twist == "read-back fails" && self.reads.get() == 2),
                "GitHub transport failed"
            );
            Ok(json!({"repository": {"issue": {"comments": {
                "nodes": *self.comments.borrow(),
                "pageInfo": {"hasNextPage": false, "endCursor": null}}}}}))
        }
    }

    impl Rest for Forge {
        fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<crate::poll::RestResponse> {
            if url.contains("/collaborators/") {
                return Ok(crate::poll::RestResponse {
                    status: 200,
                    etag: None,
                    last_modified: None,
                    link: None,
                    body: json!({"permission": "admin"}),
                });
            }
            assert!(!self.overlap.is_null(), "unexpected GET {url}");
            let runs: Vec<Value> = (self.overlap["runs"].as_array().unwrap().iter())
                .filter(|_| url.contains("status=in_progress"))
                .map(|t| json!({"display_title": t}))
                .collect();
            Ok(crate::poll::RestResponse {
                status: 200,
                etag: None,
                last_modified: None,
                link: None,
                body: json!({"workflow_runs": runs}),
            })
        }

        fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
            let comments =
                "https://api.github.com/repos/cgwalters-forge/tracker/issues/540/comments";
            let edits = "https://api.github.com/repos/cgwalters-forge/tracker/issues/comments/";
            let text = body["body"].as_str().unwrap_or_default();
            if (method, url) == ("POST", comments) {
                if self.twist == "record fails" {
                    return Ok((403, json!({"message": "denied"})));
                }
                if self.twist == "plan edited" {
                    let mut comments = self.comments.borrow_mut();
                    comments[0]["lastEditedAt"] = json!("2026-10-14T08:59:00Z");
                    comments[0]["editor"] = json!({"login": OPERATOR});
                }
                if self.twist == "race" {
                    self.post(self.viewer, &record("7@2026-10-13T10:00:00Z"));
                }
                let id = self.post(self.viewer, text);
                return Ok((
                    201,
                    json!({"id": id, "html_url": format!("{ISSUE}#issuecomment-{id}")}),
                ));
            }
            let other = url
                .strip_prefix("https://api.github.com/repos/cgwalters-forge/tracker/issues/")
                .and_then(|u| u.strip_suffix("/comments")?.parse::<u64>().ok());
            if let (Some(n), "POST") = (other, method) {
                let mut others = self.others.borrow_mut();
                let comments = others.get_mut(&n).unwrap();
                let mut post = |author: &str, body: &str| {
                    let id = 1000 * n + comments.len() as u64;
                    let url = format!(
                        "https://github.com/cgwalters-forge/tracker/issues/{n}#issuecomment-{id}"
                    );
                    comments.push(
                        json!({"url": url, "body": body, "createdAt": "2026-10-14T09:00:00Z",
                        "lastEditedAt": null, "author": {"login": author}, "editor": null}),
                    );
                    (id, url)
                };
                if self.twist == "extra race" && n == 541 {
                    post(
                        OPERATOR,
                        &record(&format!("{}@2026-10-13T10:00:00Z", 1000 * n)),
                    );
                }
                let (id, url) = post(self.viewer, text);
                return Ok((201, json!({"id": id, "html_url": url})));
            }
            if let (Some(id), "PATCH") = (url.strip_prefix(edits), method) {
                if let Some(node) = self
                    .others
                    .borrow_mut()
                    .values_mut()
                    .flatten()
                    .find(|c| c["url"].as_str().unwrap().ends_with(&format!("-{id}")))
                {
                    node["body"] = text.into();
                    node["lastEditedAt"] = json!("2026-10-14T09:00:01Z");
                    node["editor"] = json!({"login": self.viewer});
                    return Ok((200, json!({})));
                }
                if self.twist == "final edit fails" && text.contains("\nDispatched the plan") {
                    return Ok((500, json!({"message": "down"})));
                }
                let mut comments = self.comments.borrow_mut();
                let node = comments
                    .iter_mut()
                    .find(|c| c["url"].as_str().unwrap().ends_with(&format!("-{id}")))
                    .unwrap();
                node["body"] = text.into();
                node["lastEditedAt"] = json!("2026-10-14T09:00:01Z");
                node["editor"] = json!({"login": self.viewer});
                return Ok((200, json!({})));
            }
            assert_eq!(
                (method, url),
                (
                    "POST",
                    "https://api.github.com/repos/cgwalters-forge/tracker/actions/workflows/dispatch.yml/dispatches"
                )
            );
            self.dispatches.borrow_mut().push(body.clone());
            Ok(self.answers.borrow_mut().remove(0))
        }
    }

    #[test]
    fn dispatch_records_and_starts_each_plan_once() {
        let fenced = format!("Plan:\n\n```{PLAN_FENCE}\n{}\n```\n", plan());
        let planned = || node(7, PLANNER, "2026-10-13T10:00:00Z", &fenced);
        let current = record("7@2026-10-13T10:00:00Z");
        let recorded = |author: &str, body: &str| node(8, author, "2026-10-13T11:00:00Z", body);
        let run = (200, json!({"workflow_run_id": 99}));
        let run_url = "https://github.com/cgwalters-forge/tracker/actions/runs/99";
        let not_tried = "It is not tried again; edit or replace the plan to dispatch it.";
        #[rustfmt::skip]
        // Name, comments, dispatch answers, what goes wrong; then the dispatch
        // requests sent, the run or error, and the end of the last comment.
        type Case<'a> = (&'a str, Vec<Value>, Vec<(u16, Value)>, &'static str, usize, Result<&'a str, &'a str>, String);
        #[rustfmt::skip]
        let cases: Vec<Case> = vec![
            ("fresh", vec![planned()], vec![run.clone()], "", 1, Ok(run_url),
                format!("Dispatched the plan in {ISSUE}#issuecomment-7: [run]({run_url}).")),
            ("recorded already", vec![planned(), recorded(PLANNER, &current)], vec![], "", 0,
                Err("has a dispatch record already, https://github.com/cgwalters-forge/tracker/issues/540#issuecomment-8"),
                current.clone()),
            // Logins compare without case.
            ("recorded already as Login", vec![planned(), recorded("CGWalters-Bot", &current)],
                vec![], "", 0, Err("has a dispatch record already"), current.clone()),
            // A writer's record counts whatever token dispatches next.
            ("recorded by the operator", vec![planned(), recorded(OPERATOR, &current)],
                vec![], "", 0, Err("has a dispatch record already"), current.clone()),
            // A record copied by anyone else does not block the plan.
            ("copied record", vec![planned(), recorded("visitor", &current)],
                vec![run.clone()], "", 1, Ok(run_url), format!("[run]({run_url}).")),
            // A record of the plan before its last edit is not this plan's.
            ("edited since", vec![planned(), recorded(PLANNER, &record("7@2026-10-13T09:00:00Z"))],
                vec![run.clone()], "", 1, Ok(run_url), format!("[run]({run_url}).")),
            // A quoted record is not one.
            ("quoted record", vec![planned(), recorded(PLANNER, &format!("> {current}"))],
                vec![run.clone()], "", 1, Ok(run_url), format!("[run]({run_url}).")),
            // Another pass posted its record first: this one starts nothing.
            ("race", vec![planned()], vec![], "race", 0,
                Err("https://github.com/cgwalters-forge/tracker/issues/540#issuecomment-101 recorded this plan first"),
                format!("(https://github.com/cgwalters-forge/tracker/issues/540#issuecomment-101 recorded this plan first). {not_tried}")),
            // Without run details, asked again without them.
            ("no run details", vec![planned()], vec![(422, json!({})), (204, Value::Null)], "", 2, Ok("-"),
                "to cgwalters-forge/tracker dispatch.yml; GitHub did not name the run.".into()),
            ("dispatch fails", vec![planned()], vec![(500, json!({"message": "boom"}))], "", 1,
                Err("GitHub HTTP status 500: boom"),
                format!("(POST repos/cgwalters-forge/tracker/actions/workflows/dispatch.yml/dispatches: GitHub HTTP status 500: boom). {not_tried}")),
            ("still refused without run details", vec![planned()], vec![(422, json!({})), (422, json!({}))], "", 2,
                Err("GitHub HTTP status 422"), not_tried.into()),
            // Nothing is sent without a record, nor on a record not read back.
            ("record fails", vec![planned()], vec![], "record fails", 0,
                Err("GitHub HTTP status 403: denied"), fenced.clone()),
            ("read-back fails", vec![planned()], vec![], "read-back fails", 0,
                Err("GitHub transport failed"), format!("(GitHub transport failed). {not_tried}")),
            ("plan edited", vec![planned()], vec![], "plan edited", 0,
                Err("the plan changed while it was being dispatched"),
                format!("(the plan changed while it was being dispatched). {not_tried}")),
            // The run started: the error says so, and the plan is recorded.
            ("final edit fails", vec![planned()], vec![run.clone()], "final edit fails", 1,
                Err("dispatched https://github.com/cgwalters-forge/tracker/actions/runs/99, but https://github.com/cgwalters-forge/tracker/issues/540#issuecomment-101 still says it is starting"),
                "Dispatching the plan in https://github.com/cgwalters-forge/tracker/issues/540#issuecomment-7.".into()),
            ("no plan", vec![node(7, "visitor", "2026-10-13T10:00:00Z", &fenced)], vec![], "", 0,
                Err("no plan on this issue"), fenced.clone()),
        ];
        let issue = crate::github::parse_github_issue(ISSUE, None, "github.com").unwrap();
        let writers = [OPERATOR, PLANNER];
        for (name, comments, answers, twist, sent, expected, last) in cases {
            let before = comments.len();
            let forge = Forge {
                twist,
                ..Forge::new(comments, vec![])
            };
            // The run is started with the dispatch token only.
            let dispatcher = Forge::new(vec![], answers);
            let result = dispatch(
                &forge,
                &dispatcher,
                &issue,
                &writers,
                &target(),
                0,
                None,
                false,
            );
            assert!(forge.dispatches.borrow().is_empty(), "{name}");
            assert!(dispatcher.comments.borrow().is_empty(), "{name}");
            assert_eq!(dispatcher.reads.get(), 0, "{name}");
            let dispatches = dispatcher.dispatches.borrow();
            assert_eq!(dispatches.len(), sent, "{name}");
            for request in dispatches.iter() {
                assert_eq!(request["inputs"]["task"], plan()["brief"], "{name}");
            }
            match (result, expected) {
                (Ok(done), Ok(run)) => {
                    assert_eq!(done.run.as_deref().unwrap_or("-"), run, "{name}");
                    assert_eq!(
                        done.record,
                        Some(format!("{ISSUE}#issuecomment-{}", 100 + before))
                    );
                    assert_eq!(dispatches[0]["return_run_details"], true, "{name}");
                    assert_eq!(forge.bodies().len(), before + 1, "{name}");
                }
                (Err(error), Err(expected)) => {
                    let error = format!("{error:#}");
                    assert!(error.contains(expected), "{name}: {error}");
                }
                (result, expected) => panic!("{name}: {result:?} != {expected:?}"),
            }
            let bodies = forge.bodies();
            let comment = bodies.last().unwrap();
            assert!(comment.ends_with(&last), "{name}: {comment}");
            // However it ended, once recorded the plan is not dispatched again.
            // (An edited plan is a new one, which may be dispatched.)
            if !["record fails", "plan edited"].contains(&twist) && name != "no plan" {
                let again = dispatch(
                    &forge,
                    &dispatcher,
                    &issue,
                    &writers,
                    &target(),
                    0,
                    None,
                    false,
                );
                let again = format!("{:#}", again.unwrap_err());
                assert!(again.contains("record already"), "{name}: {again}");
                assert_eq!(dispatcher.dispatches.borrow().len(), sent, "{name}");
            }
        }
        // Nothing is written as an account GitHub does not name, nor as one
        // whose records would not count.
        for (viewer, error) in [
            ("not a login", "did not name"),
            ("dispatcher", "dispatcher is not a plan writer"),
        ] {
            let forge = Forge {
                viewer,
                ..Forge::new(vec![planned()], vec![])
            };
            let result = dispatch(&forge, &forge, &issue, &writers, &target(), 0, None, false);
            let message = result.unwrap_err().to_string();
            assert!(message.contains(error), "{message}");
            assert_eq!(forge.bodies().len(), 1);
        }
        // An invalid target is refused before anything is read.
        let forge = Forge::new(vec![planned()], vec![]);
        let bad = Target {
            workflow: "x/../dispatch.yml".into(),
            ..target()
        };
        assert!(dispatch(&forge, &forge, &issue, &[OPERATOR], &bad, 0, None, false).is_err());
        assert_eq!(forge.reads.get(), 0);
        // With the collaborator check, the issue holding the plan and the one
        // it targets are checked, and a refusal writes and starts nothing.
        for (twist, sent) in [("", 1), ("unlocked", 0)] {
            let forge = Forge {
                twist,
                ..Forge::new(vec![planned()], vec![])
            };
            let dispatcher = Forge::new(vec![], vec![run.clone()]);
            let result = dispatch(
                &forge,
                &dispatcher,
                &issue,
                &writers,
                &target(),
                0,
                None,
                true,
            );
            assert_eq!(
                *forge.checked.borrow(),
                ["cgwalters-forge/tracker#540", "ostreedev/ostree#3665"][..sent + 1]
            );
            assert_eq!(dispatcher.dispatches.borrow().len(), sent, "{twist}");
            match result {
                Ok(_) => assert_eq!(forge.bodies().len(), 2),
                Err(error) => {
                    let error = format!("{error:#}");
                    assert!(error.contains("not dispatched: --collaborators-only: cgwalters-forge/tracker#540: the conversation is not locked"), "{error}");
                    assert_eq!(forge.bodies().len(), 1);
                }
            }
        }
    }

    #[test]
    fn batched_dispatch_records_each_issue_and_frees_unfinished_ones() {
        let tracker = "https://github.com/cgwalters-forge/tracker/issues";
        let planned = |n: u64, kind: &str| {
            let mut p = plan();
            p["repo"] = json!("cgwalters-forge/tracker");
            p["item"] = json!(n);
            p["kind"] = json!(kind);
            p["brief"] = json!(format!("Do #{n}."));
            body(&p)
        };
        let comment = |n: u64, id: u64, body: String| {
            json!({"url": format!("{tracker}/{n}#issuecomment-{id}"), "body": body,
                "createdAt": "2026-10-13T10:00:00Z", "lastEditedAt": null,
                "author": {"login": PLANNER}, "editor": null})
        };
        let sub = |n: u64, labels: &[&str]| {
            json!({"number": n, "state": "OPEN", "body": "",
                "repository": {"nameWithOwner": "cgwalters-forge/tracker"},
                "labels": {"nodes": labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>(),
                    "totalCount": labels.len()},
                "issueDependenciesSummary": {"blockedBy": 0},
                "closedByPullRequestsReferences": {"totalCount": 0}})
        };
        let plan_id = |n: u64| format!("{}@2026-10-13T10:00:00Z", 1000 * n);
        // The epic's other sub-issues: #542 is recorded already, #543 has a
        // research plan, #544 is not small, and #545 was left over by a batch
        // whose primary #539 is closed, which frees it.
        let siblings = || {
            let mut others = std::collections::BTreeMap::new();
            for n in 541..=546 {
                let kind = if n == 543 { "research" } else { "implement" };
                others.insert(n, vec![comment(n, 1000 * n, planned(n, kind))]);
            }
            let held = record(&plan_id(542));
            others.get_mut(&542).unwrap().push(comment(542, 5421, held));
            let left = format!(
                "<!-- {DISPATCH_MARKER}: {} -->\n<!-- {}: cgwalters-forge/tracker#539 -->\nDispatched.",
                plan_id(545),
                crate::batch::BATCH_MARKER
            );
            others.get_mut(&545).unwrap().push(comment(545, 5451, left));
            others
        };
        let family = |primary: &[&str]| {
            let mut nodes = vec![sub(540, primary)];
            nodes.extend((541..=546).map(|n| sub(n, if n == 544 { &[] } else { &["small"] })));
            let mut issue = sub(540, primary);
            issue["parent"] =
                json!({"subIssues": {"nodes": nodes, "pageInfo": {"hasNextPage": false}}});
            json!({"repository": {"issue": issue}})
        };
        let run_url = "https://github.com/cgwalters-forge/tracker/actions/runs/99";
        let issue = crate::github::parse_github_issue(ISSUE, None, "github.com").unwrap();
        let writers = [OPERATOR, PLANNER];
        // Name, batch size, what goes wrong, the primary's labels; then the
        // extras the run does, or the error.
        type Case<'a> = (
            &'a str,
            usize,
            &'a str,
            &'a [&'a str],
            Result<&'a [u64], &'a str>,
        );
        #[rustfmt::skip]
        let cases: &[Case] = &[
            ("off", 0, "", &["small"], Ok(&[])),
            ("three", 3, "", &["small"], Ok(&[541, 545, 546])),
            ("two", 2, "", &["small"], Ok(&[541, 545])),
            ("primary not small", 3, "", &["P2"], Ok(&[])),
            ("primary opted out", 3, "", &["small", "no-batch"], Ok(&[])),
            // Another pass taking an extra first leaves only that one out.
            ("extra race", 2, "extra race", &["small"], Ok(&[545])),
            // An extra that cannot be read back is left out and released.
            ("extra unreadable", 2, "extra unreadable", &["small"], Ok(&[545])),
            // An unreadable epic runs the issue alone.
            ("epic fails", 3, "epic fails", &["small"], Ok(&[])),
            // Another pass taking the primary first starts nothing.
            ("primary race", 2, "race", &["small"],
                Err("issues/540#issuecomment-101 recorded this plan first")),
        ];
        for (name, batch, twist, labels, expected) in cases {
            let forge = Forge {
                twist,
                others: RefCell::new(siblings()),
                family: if *batch == 0 {
                    Value::Null
                } else if *twist == "epic fails" {
                    json!("fail")
                } else {
                    family(labels)
                },
                closed: RefCell::new(vec![539]),
                ..Forge::new(
                    vec![node(
                        7,
                        PLANNER,
                        "2026-10-13T10:00:00Z",
                        &planned(540, "implement"),
                    )],
                    vec![],
                )
            };
            let dispatcher = Forge::new(vec![], vec![(200, json!({"workflow_run_id": 99}))]);
            let result = dispatch(
                &forge,
                &dispatcher,
                &issue,
                &writers,
                &target(),
                *batch,
                None,
                false,
            );
            let others = forge.others.borrow();
            let last = |n: u64| {
                others[&n].last().unwrap()["body"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            };
            match (result, expected) {
                (Ok(done), Ok(extras)) => {
                    let task = &dispatcher.dispatches.borrow()[0]["inputs"]["task"];
                    let task = task.as_str().unwrap();
                    assert!(task.starts_with("Do #540."), "{name}: {task}");
                    assert_eq!(
                        task.contains("one commit per issue"),
                        !extras.is_empty(),
                        "{name}"
                    );
                    let names: Vec<String> = extras
                        .iter()
                        .map(|n| format!("cgwalters-forge/tracker#{n}"))
                        .collect();
                    assert_eq!(done.batched, names, "{name}");
                    for n in 541..=546 {
                        let section = format!("## cgwalters-forge/tracker#{n}\n\nDo #{n}.");
                        assert_eq!(task.contains(&section), extras.contains(&n), "{name} #{n}");
                    }
                    for n in *extras {
                        let header = format!(
                            "<!-- {DISPATCH_MARKER}: {} -->\n<!-- agent-board-batch: cgwalters-forge/tracker#540 -->\n",
                            plan_id(*n)
                        );
                        let text = last(*n);
                        assert!(text.starts_with(&header), "{name}: {text}");
                        assert!(
                            text.ends_with(&format!(
                                "with cgwalters-forge/tracker#540: [run]({run_url})."
                            )),
                            "{name}: {text}"
                        );
                    }
                    if *name == "extra race" {
                        let text = last(541);
                        assert!(text.contains("was left out of the run for cgwalters-forge/tracker#540 (https://github.com/cgwalters-forge/tracker/issues/541#issuecomment-541001 recorded this plan first)"), "{name}: {text}");
                    }
                    if *name == "extra unreadable" {
                        let text = last(541);
                        assert_eq!(
                            text,
                            "The plan in https://github.com/cgwalters-forge/tracker/issues/541#issuecomment-541000 was left out of the run for cgwalters-forge/tracker#540: it could not be checked (GitHub transport failed). A later dispatch may take it.",
                            "{name}"
                        );
                        assert!(
                            done.notes
                                .join("\n")
                                .contains("#541: GitHub transport failed"),
                            "{name}"
                        );
                        // Released, so a later pass dispatches it.
                        drop(others);
                        let extra = crate::github::parse_github_issue(
                            &format!("{tracker}/541"),
                            None,
                            "github.com",
                        )
                        .unwrap();
                        let again = Forge::new(vec![], vec![(200, json!({"workflow_run_id": 99}))]);
                        let freed =
                            dispatch(&forge, &again, &extra, &writers, &target(), 0, None, false);
                        assert_eq!(freed.unwrap().run.as_deref(), Some(run_url), "{name}");
                        continue;
                    }
                    if *name == "epic fails" {
                        let notes = done.notes.join("\n");
                        assert!(
                            notes.contains("the epic could not be read: epic unreadable"),
                            "{name}: {notes}"
                        );
                    }
                    if *name == "three" {
                        let notes = done.notes.join("\n");
                        assert!(notes.contains("#542: recorded already"), "{name}: {notes}");
                        assert!(
                            notes.contains("#543: not an implement plan"),
                            "{name}: {notes}"
                        );
                    }
                }
                (Err(error), Err(expected)) => {
                    let error = format!("{error:#}");
                    assert!(error.contains(expected), "{name}: {error}");
                    assert!(dispatcher.dispatches.borrow().is_empty(), "{name}");
                    // Every record this pass posted says it started nothing.
                    for text in [forge.bodies().last().unwrap().clone(), last(541), last(545)] {
                        assert!(
                            text.ends_with("edit or replace the plan to dispatch it."),
                            "{name}: {text}"
                        );
                    }
                }
                (result, expected) => panic!("{name}: {result:?} != {expected:?}"),
            }
        }
        // An extra left open is held while its batch's primary is open, and
        // free for a run of its own once the primary is closed.
        let forge = Forge {
            others: RefCell::new(siblings()),
            family: family(&["small"]),
            ..Forge::new(
                vec![node(
                    7,
                    PLANNER,
                    "2026-10-13T10:00:00Z",
                    &planned(540, "implement"),
                )],
                vec![],
            )
        };
        let dispatcher = Forge::new(vec![], vec![(200, json!({"workflow_run_id": 99})); 2]);
        dispatch(
            &forge,
            &dispatcher,
            &issue,
            &writers,
            &target(),
            1,
            None,
            false,
        )
        .unwrap();
        let extra =
            crate::github::parse_github_issue(&format!("{tracker}/541"), None, "github.com")
                .unwrap();
        let held = dispatch(
            &forge,
            &dispatcher,
            &extra,
            &writers,
            &target(),
            0,
            None,
            false,
        )
        .unwrap_err();
        assert!(
            held.to_string().contains("has a dispatch record already"),
            "{held}"
        );
        // Closed early, while the batch's pull request that closes #541 is open,
        // the primary frees nothing.
        forge.closed.borrow_mut().push(540);
        forge.pulled.borrow_mut().push(541);
        let held = dispatch(
            &forge,
            &dispatcher,
            &extra,
            &writers,
            &target(),
            0,
            None,
            false,
        )
        .unwrap_err();
        assert!(
            held.to_string().contains("has a dispatch record already"),
            "{held}"
        );
        forge.pulled.borrow_mut().clear();
        let freed = dispatch(
            &forge,
            &dispatcher,
            &extra,
            &writers,
            &target(),
            0,
            None,
            false,
        )
        .unwrap();
        assert_eq!(freed.run.as_deref(), Some(run_url));
        assert_eq!(dispatcher.dispatches.borrow().len(), 2);
        // A configured batch past the bound is refused before anything is read.
        assert!(
            dispatch(
                &forge,
                &dispatcher,
                &issue,
                &writers,
                &target(),
                6,
                None,
                false
            )
            .is_err()
        );
    }

    #[test]
    fn dispatch_checks_overlap_with_the_bot() {
        // A tracker issue whose plan changes ostreedev/ostree#3665: the pull
        // requests compared are ostree's (the fake asserts it).
        let p = plan();
        let planned = node(7, PLANNER, "2026-10-13T10:00:00Z", &body(&p));
        let pull = |n: u64| {
            json!({"number": n, "author": {"login": "agentic-bot"},
                "files": {"nodes": [{"path": "src/safe_outputs.rs"}]}})
        };
        let issue = crate::github::parse_github_issue(ISSUE, None, "github.com").unwrap();
        let writers = [OPERATOR, PLANNER];
        // Name, open pull requests; then whether it ran, and what its record says.
        for (name, pulls, ran, says) in [
            (
                "independent",
                json!([]),
                true,
                "Run independently: no open pull request of agentic-bot changes a path it names.",
            ),
            ("waits for one", json!([pull(9)]), false, ""),
            ("waits for two", json!([pull(9), pull(8)]), false, ""),
        ] {
            let forge = Forge {
                overlap: json!({"text": "Fix safe_outputs.rs.", "pulls": pulls, "runs": []}),
                ..Forge::new(vec![planned.clone()], vec![])
            };
            let dispatcher = Forge::new(vec![], vec![(200, json!({"workflow_run_id": 99}))]);
            let done = dispatch(
                &forge,
                &dispatcher,
                &issue,
                &writers,
                &target(),
                0,
                Some("agentic-bot"),
                false,
            )
            .unwrap();
            let dispatches = dispatcher.dispatches.borrow();
            assert_eq!(dispatches.len(), usize::from(ran), "{name}");
            assert_eq!(done.record.is_some(), ran, "{name}");
            if !ran {
                // A wait writes nothing, so the next pass decides again.
                assert_eq!(forge.bodies().len(), 1, "{name}");
                assert!(matches!(done.overlap, Some(Decision::Wait(_))), "{name}");
                continue;
            }
            let record = forge.bodies().last().unwrap().clone();
            assert!(
                record.ends_with(&format!(
                    "[run](https://github.com/cgwalters-forge/tracker/actions/runs/99). {says}"
                )),
                "{name}: {record}"
            );
            let task = dispatches[0]["inputs"]["task"].as_str().unwrap();
            assert_eq!(task, p["brief"].as_str().unwrap(), "{name}");
        }
        // The bot's login goes into a query: it must be one.
        let forge = Forge::new(vec![planned], vec![]);
        assert!(
            dispatch(
                &forge,
                &forge,
                &issue,
                &writers,
                &target(),
                0,
                Some("a b"),
                false
            )
            .is_err()
        );
        assert_eq!(forge.reads.get(), 0);
    }

    #[test]
    fn stacked_plan_dispatches_on_the_lower_slices_branch() {
        // Two slices of ostree: #3665's plan stacks on #3664, whose run opened
        // #9 from dispatch/3664, which changes the same file.
        let mut p = plan();
        p["stacks_on"] = json!(3664);
        let planned = node(7, PLANNER, "2026-10-13T10:00:00Z", &body(&p));
        let lower = json!({"number": 9, "author": {"login": "agentic-bot"},
            "headRefName": "dispatch/3664", "body": "Work.\n\nCloses #3664",
            "isCrossRepository": false, "files": {"nodes": [{"path": "src/safe_outputs.rs"}]}});
        let mut forked = lower.clone();
        forked["isCrossRepository"] = true.into();
        let none_yet = "it stacks on ostreedev/ostree#3664, which has no open pull request of agentic-bot from a dispatch/ branch of ostreedev/ostree among the newest 50";
        let stack = |state: &str, pulls: Value| json!({"repository": {"issue": {"state": state}, "pullRequests": {"nodes": pulls}}});
        let issue = crate::github::parse_github_issue(ISSUE, None, "github.com").unwrap();
        let writers = [OPERATOR, PLANNER];
        // Name, the lower issue's state and pull requests; then the base sent
        // (none: nothing ran) and what the record ends with, or the wait.
        for (name, state, pulls, base, says) in [
            (
                "on the lower pull request",
                "OPEN",
                json!([lower]),
                Some("dispatch/3664"),
                "Stacked on https://github.com/ostreedev/ostree/pull/9 (`dispatch/3664`).",
            ),
            (
                "no lower pull request yet",
                "OPEN",
                json!([]),
                None,
                none_yet,
            ),
            // Its base would be a branch of the fork, which no pull request
            // of ostree can target.
            (
                "lower pull request from the fork",
                "OPEN",
                json!([forked]),
                None,
                none_yet,
            ),
            (
                "lower slice closed",
                "CLOSED",
                json!([]),
                Some(""),
                "Not stacked: ostreedev/ostree#3664 is closed, so it runs on the default branch.",
            ),
        ] {
            let forge = Forge {
                stack: stack(state, pulls.clone()),
                // Read only for an unstacked run, which then runs alone.
                family: json!("fail"),
                overlap: json!({"text": "Fix safe_outputs.rs.", "pulls": pulls, "runs": []}),
                ..Forge::new(vec![planned.clone()], vec![])
            };
            let dispatcher = Forge::new(vec![], vec![(200, json!({"workflow_run_id": 99}))]);
            let done = dispatch(
                &forge,
                &dispatcher,
                &issue,
                &writers,
                &target(),
                3,
                Some("agentic-bot"),
                false,
            )
            .unwrap();
            let dispatches = dispatcher.dispatches.borrow();
            let Some(base) = base else {
                // Nothing is written, so the next pass decides again.
                assert!(dispatches.is_empty(), "{name}");
                assert_eq!(forge.bodies().len(), 1, "{name}");
                assert_eq!(done.overlap, Some(Decision::Wait(says.into())), "{name}");
                continue;
            };
            let inputs = &dispatches[0]["inputs"];
            let task = inputs["task"].as_str().unwrap();
            let record = forge.bodies().last().unwrap().clone();
            assert!(record.ends_with(says), "{name}: {record}");
            if base.is_empty() {
                assert!(inputs.get("base").is_none(), "{name}");
                assert_eq!(task, p["brief"], "{name}");
                continue;
            }
            // Its pull request says what it stacks on; the lower one's files
            // are not an overlap to wait for, and nothing is batched onto it.
            assert_eq!(inputs["base"], base, "{name}");
            assert!(task.starts_with(p["brief"].as_str().unwrap()), "{name}");
            assert!(
                task.contains("stacks on ostreedev/ostree#9 (https://github.com/ostreedev/ostree/pull/9), the pull request for #3664: the run starts from its branch `dispatch/3664`"),
                "{name}: {task}"
            );
            assert!(task.contains("\"Stacks on #9.\""), "{name}: {task}");
            assert!(done.notes.is_empty() && done.batched.is_empty(), "{name}");
        }
        // Whose pull request it stacks on is the caller's to say: without
        // --bot a stacked plan is refused before anything is written.
        let forge = Forge::new(vec![planned], vec![]);
        let error = dispatch(&forge, &forge, &issue, &writers, &target(), 0, None, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("needs --bot"), "{error}");
        assert_eq!(forge.bodies().len(), 1);
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
