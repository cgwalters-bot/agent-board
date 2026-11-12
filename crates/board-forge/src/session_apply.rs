//! Applying an accepted session batch once, with a receipt on its anchor
//! (agent-board#845, docs/sessions.md#apply). The pass finds the batch
//! answering a day's session plan, checks it against the caller's bounds
//! ([`crate::session`]), and applies it here: the receipt is posted as the
//! claim before any write, as an epic plan's is ([`crate::epic_apply`]),
//! and the day's receipts, from the configured due time on, claim its one
//! apply, whichever plan and record they follow: a run posting as the bot
//! can replace the day's plan and record on the anchor, not the day. What
//! it writes is the batch's own typed actions, each a plain forge write
//! under the batch's caps, on issues resolved as such before any write;
//! nothing in a batch widens that. An action on an issue that changed
//! since the session read it (its `updated_at` is not the action's) is
//! skipped, with the reason on the receipt, rather than written from the
//! session's stale view (agent-board#849).
use crate::epic::IssueRef;
use crate::epic_apply::{link, node_id, set_option};
use crate::github::{API_ROOT, Transport};
use crate::plan::{
    Comment, JOB_PLAN_SCHEMA, JobPlan, PLAN_FENCE, comment_id, fenced_json, has_fence,
    issue_comments, newest_fenced, no_fenced, plan_id, send,
};
use crate::poll::Rest;
use crate::session::{Action, Bounds, SESSION_FENCE, SessionBatch, stamp};
use anyhow::{Context, Result, ensure};
use board_core::ItemRef;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// The receipt's fence info string. Exact, so it is not a batch's.
pub const APPLY_FENCE: &str = "agent-board-session-apply";
/// The schema of a receipt's machine detail.
pub const RECEIPT_SCHEMA: &str = "agent-board/session-apply/v1";
/// What a plan a batch posts says first; the batch's address follows.
pub const PLAN_INTRO: &str =
    "A session proposed this plan in its batch, which the board pass applied.";
/// The most requests one action takes: its issue's identity, then a
/// Priority's node id, project add, item lookup and field write.
pub const ACTION_REQUESTS: usize = 5;

/// An issue's identity and version, as the apply resolves each target
/// before writing: `issue` is null for a pull request's number or none;
/// `updatedAt` is compared with the version each action was decided from.
pub(crate) const IDENTITY_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { issue(number: $number) { number updatedAt repository { nameWithOwner } } } }";
/// The requests an apply takes besides its actions and its read-back's
/// pages: the claim, the receipt's update, the board's ids and one to
/// spare.
pub const APPLY_REQUESTS: usize = 4;

/// The board's ids a `set_priority` writes by: the project, its Priority
/// field and each option's id by name, from the board, not a batch.
#[derive(Clone, Debug, PartialEq)]
pub struct Priorities {
    pub project: String,
    pub field: String,
    pub options: BTreeMap<String, String>,
}

/// What an apply writes under: whose receipts claim and from when, who is
/// asked, and the board's ids where the batch sets a Priority.
pub struct Trusted<'a> {
    /// The operator, whom a batch's questions are for.
    pub operator: &'a str,
    /// Whose receipts count as claims: the operator and the bot that
    /// applies, as for epic plans.
    pub claimers: [&'a str; 2],
    /// When the day's sessions were due, from the caller's configuration:
    /// a claimer's receipt posted from then on claims the day's one apply.
    pub since: OffsetDateTime,
    pub priorities: Option<&'a Priorities>,
}

/// The batch answering a session plan: the newest [`SESSION_FENCE`]
/// comment by a writer after `record`, the plan's dispatch record, by
/// comment id. `None` where there is none; an error where the newest was
/// edited by anyone else, which a person settles, as for a plan.
pub fn answer<'a>(
    comments: &'a [Comment],
    writers: &[&str],
    record: u64,
) -> Result<Option<&'a Comment>> {
    let start = (comments.iter())
        .position(|c| comment_id(&c.url) == Some(record))
        .map_or(comments.len(), |at| at + 1);
    let later = &comments[start..];
    if no_fenced(later, writers, SESSION_FENCE) {
        return Ok(None);
    }
    newest_fenced(later, writers, SESSION_FENCE)
        .map(Some)
        .context("its batch")
}

/// The batch in `comment`, checked against `bounds` and against `anchor`,
/// the issue it was found on: its `repo` is the anchor's, so a batch
/// posted on one repository's anchor cannot triage another's, and no
/// action names the anchor, the session's own issue, which a close, a
/// new body or a plan there would take from the pass's sessions.
pub fn check(comment: &Comment, anchor: &ItemRef, bounds: &Bounds) -> Result<SessionBatch> {
    let batch = SessionBatch::from_comment(&comment.body, bounds)?
        .context("session batch fence vanished")?;
    ensure!(
        batch.repo.eq_ignore_ascii_case(&anchor.repository),
        "its repo {} is not {}, the anchor's repository",
        batch.repo,
        anchor.repository
    );
    let own = |issue: &IssueRef| {
        issue.repo.eq_ignore_ascii_case(&anchor.repository) && issue.number == anchor.number
    };
    for (index, action) in batch.actions.iter().enumerate() {
        let named = match action {
            Action::Close { issue, .. }
            | Action::Rescope { issue, .. }
            | Action::SetPriority { issue, .. }
            | Action::Plan { issue, .. } => own(issue),
            Action::Attach { issue, epic, .. } => own(issue) || own(epic),
            Action::Ask { .. } => false,
        };
        ensure!(
            !named,
            "action {} names the anchor {}, the session's own issue",
            index + 1,
            anchor.display()
        );
    }
    Ok(batch)
}

// ---- Receipts ----

/// How far an apply got, as its receipt says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// Posted before the first write and not finished: under way, or cut
    /// short before it could say how it ended.
    Claimed,
    Applied,
    /// A request failed; what it did is listed, and may be incomplete.
    Stopped,
}

/// One batch action as far as its writes were confirmed. The writes are
/// named `comment`, `patch` (a close or a new body), `priority` and
/// `link`; a question's is `receipt`, the receipt itself. An action
/// skipped because its issue changed since the session read it has
/// `skipped` saying so and no writes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Confirmed {
    /// Its 1-based position in the batch.
    pub action: usize,
    #[serde(rename = "type")]
    pub kind: String,
    /// The issue it changed, `OWNER/REPO#N`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    /// The Priority it set, the epic it attached to, the kind of plan it
    /// posted or the domain it asked in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Why nothing was written: the issue's `updated_at` now and as the
    /// session read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    #[serde(default)]
    pub writes: Vec<String>,
}

impl Confirmed {
    /// Whether every write its kind makes was confirmed.
    fn whole(&self) -> bool {
        needs(&self.kind)
            .iter()
            .all(|w| self.writes.iter().any(|made| made == w))
    }
}

/// The writes an action of `kind` makes, in order, by the names
/// [`Confirmed`] records them.
fn needs(kind: &str) -> &'static [&'static str] {
    match kind {
        "close_issue" => &["comment", "patch"],
        "rescope_issue" => &["patch", "comment"],
        "set_priority" => &["priority"],
        "attach_to_epic" => &["link"],
        "post_plan" => &["comment"],
        "ask_operator" => &["receipt"],
        _ => &["?"],
    }
}

/// The machine detail of a receipt, its fenced [`APPLY_FENCE`] block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Detail {
    pub schema: String,
    /// The batch it claims: `COMMENT-ID@CHANGE-TIME` ([`plan_id`]).
    pub batch: String,
    pub outcome: Outcome,
    #[serde(default)]
    pub actions: Vec<Confirmed>,
}

impl Detail {
    fn new(batch: &str, outcome: Outcome) -> Self {
        Self {
            schema: RECEIPT_SCHEMA.into(),
            batch: batch.into(),
            outcome,
            actions: Vec::new(),
        }
    }
}

/// An apply's receipt on the anchor: one comment, posted as the claim
/// before any write and updated once the apply ends.
#[derive(Clone, Debug, PartialEq)]
pub struct Receipt {
    pub url: String,
    pub id: u64,
    pub changed: OffsetDateTime,
    pub detail: Detail,
}

/// The receipts on an anchor by `claimers`, in comment order, and the URLs
/// of those that look like receipts but cannot be read: a fence that does
/// not parse, another schema, no comment id, or an edit by anyone else.
/// Each unreadable one may claim any batch, so none is applied past it.
/// Unknown fields are ignored, so a later build's receipts still claim.
pub fn receipts(comments: &[Comment], claimers: &[&str]) -> (Vec<Receipt>, Vec<String>) {
    let claimer = |login: Option<&str>| {
        login.is_some_and(|a| claimers.iter().any(|w| w.eq_ignore_ascii_case(a)))
    };
    let (mut out, mut unread) = (Vec::new(), Vec::new());
    for comment in (comments.iter())
        .filter(|c| claimer(c.author.as_deref()) && has_fence(&c.body, APPLY_FENCE))
    {
        let foreign =
            (comment.edited || comment.editor.is_some()) && !claimer(comment.editor.as_deref());
        let detail = fenced_json(&comment.body, APPLY_FENCE)
            .ok()
            .flatten()
            .and_then(|value| serde_json::from_value::<Detail>(value).ok())
            .filter(|d| d.schema == RECEIPT_SCHEMA);
        match (detail, comment_id(&comment.url)) {
            (Some(detail), Some(id)) if !foreign => out.push(Receipt {
                url: comment.url.clone(),
                id,
                changed: comment.changed,
                detail,
            }),
            _ => unread.push(comment.url.clone()),
        }
    }
    (out, unread)
}

/// One line per action, saying what it did: `Closed OWNER/REPO#N as not
/// planned`, why it was skipped, or how far it got.
pub fn readable(detail: &Detail) -> Vec<String> {
    (detail.actions.iter())
        .map(|c| {
            let issue = c.issue.as_deref().unwrap_or("an issue");
            let about = c.detail.as_deref().unwrap_or("?");
            if let Some(why) = &c.skipped {
                return format!("Skipped action {} ({}) on {issue}: {why}", c.action, c.kind);
            }
            match (c.kind.as_str(), c.whole()) {
                ("close_issue", true) => format!("Closed {issue} as not planned"),
                ("rescope_issue", true) => format!("Rescoped {issue}"),
                ("set_priority", true) => format!("Set {issue} to {about}"),
                ("attach_to_epic", true) => format!("Attached {issue} to {about}"),
                ("post_plan", true) => format!("Posted a plan ({about}) on {issue}"),
                ("ask_operator", _) => format!("Asked a question ({about}), below"),
                (kind, whole) => {
                    let made = match (whole, c.writes.is_empty()) {
                        (true, _) => "done".to_owned(),
                        (false, true) => "stopped before any write".to_owned(),
                        (false, false) => format!("stopped after its {}", c.writes.join(", ")),
                    };
                    format!("Action {} ({kind}) on {issue}: {made}", c.action)
                }
            }
        })
        .collect()
}

/// A batch's questions, by domain, in order.
fn questions(batch: &SessionBatch) -> Vec<(&str, &str)> {
    (batch.actions.iter())
        .filter_map(|a| match a {
            Action::Ask { domain, question } => Some((domain.as_str(), question.as_str())),
            _ => None,
        })
        .collect()
}

fn receipt_body(
    detail: &Detail,
    batch_url: &str,
    asked: &[(&str, &str)],
    operator: &str,
    at: &str,
) -> Result<String> {
    let batch = format!("[this session batch]({batch_url})");
    let header = match detail.outcome {
        Outcome::Claimed => format!(
            "Applying {batch}. Do not replay it after a failure: this receipt says what was done."
        ),
        Outcome::Applied => format!("Applied {batch} at {at}."),
        Outcome::Stopped => format!(
            "Stopped applying {batch} at {at} after a failed request; what it did may be incomplete. Inspect it before the next session."
        ),
    };
    let lines: String = (readable(detail).iter())
        .map(|l| format!("- {l}\n"))
        .collect();
    let lines = match lines.is_empty() {
        true => String::new(),
        false => format!("{lines}\n"),
    };
    let asks = match asked.is_empty() {
        true => String::new(),
        false => format!(
            "Questions for @{operator}:\n{}\n",
            (asked.iter())
                .map(|(domain, question)| format!("- ({domain}) {question}\n"))
                .collect::<String>()
        ),
    };
    let json = serde_json::to_string_pretty(detail)?;
    Ok(format!(
        "{header}\n\n{lines}{asks}```{APPLY_FENCE}\n{json}\n```\n"
    ))
}

/// What applying `batch` would do, in words, and the write steps it
/// takes: the receipt, and each action that writes.
pub fn planned(batch: &SessionBatch) -> (Vec<String>, usize) {
    let lines: Vec<String> = (batch.actions.iter())
        .map(|action| match action {
            Action::Close { issue, .. } => format!("Close {issue} as not planned"),
            Action::Rescope { issue, .. } => format!("Rescope {issue}"),
            Action::SetPriority { issue, value, .. } => format!("Set {issue} to {value}"),
            Action::Attach { issue, epic, .. } => format!("Attach {issue} to {epic}"),
            Action::Plan { issue, plan, .. } => format!("Post a plan ({}) on {issue}", plan.kind),
            Action::Ask { domain, .. } => format!("Ask the operator ({domain})"),
        })
        .collect();
    let writes = (batch.actions.iter())
        .filter(|a| !matches!(a, Action::Ask { .. }))
        .count();
    (lines, 1 + writes)
}

/// Applies `batch`, found in `from` on `anchor`, once: the receipt is
/// posted as the claim before any write, read back so that an earlier
/// claim of the day (any batch's, from `trusted.since` on) or a changed
/// batch stops it with nothing written, and updated when the apply ends.
/// Each issue the actions name is resolved as an issue before the first
/// write, and an action whose issue changed since the session read it is
/// skipped, said on the receipt. Stops at the first failed request,
/// keeping the claim: a cut-short apply is a person's to inspect, never
/// replayed. Callers serialize passes, as GitHub comments are not a lock.
/// Returns the receipt's lines and a `Receipt:` line.
pub fn apply<C: Transport + Rest>(
    client: &C,
    anchor: &ItemRef,
    from: &Comment,
    batch: &SessionBatch,
    trusted: &Trusted,
    now: OffsetDateTime,
) -> Result<Vec<String>> {
    let id = plan_id(from)?;
    // Every Priority option has an id before anything is written.
    for action in &batch.actions {
        if let Action::SetPriority { value, .. } = action {
            let priorities =
                (trusted.priorities).context("there is no board to set Priority on")?;
            ensure!(
                priorities.options.contains_key(value),
                "the board's Priority field has no option {value:?}"
            );
        }
    }
    let asked = questions(batch);
    let dated = now.format(&Rfc3339)?;
    let path = format!(
        "{API_ROOT}repos/{}/issues/{}/comments",
        anchor.repository, anchor.number
    );
    let mut detail = Detail::new(&id, Outcome::Claimed);
    let claim = receipt_body(&detail, &from.url, &asked, trusted.operator, &dated)?;
    let (_, posted) = send(client, "POST", &path, &json!({"body": claim}), 0)?;
    let mine = posted["id"]
        .as_u64()
        .context("apply receipt has no id; nothing applied")?;
    let receipt_url = posted["html_url"].as_str().map(str::to_owned);
    let edit = format!(
        "{API_ROOT}repos/{}/issues/comments/{mine}",
        anchor.repository
    );
    let result = (|| -> Result<()> {
        let again = issue_comments(client, anchor)?;
        let current = (again.iter())
            .find(|c| c.url == from.url)
            .context("the batch comment vanished; nothing applied")?;
        ensure!(
            plan_id(current)? == id,
            "the batch changed since it was read; nothing applied"
        );
        // The day's receipts, whichever batch each claims: one apply a
        // day, so an earlier claim wins; an earlier day's are its own.
        let since: Vec<Comment> = (again.iter())
            .filter(|c| c.created >= trusted.since)
            .cloned()
            .collect();
        let (claims, unread) = receipts(&since, &trusted.claimers);
        ensure!(
            unread.is_empty(),
            "{} looks like an apply receipt but cannot be read; nothing applied",
            unread.join(", ")
        );
        ensure!(
            claims.iter().any(|r| r.id == mine),
            "the claim did not land as the bot's; nothing applied"
        );
        ensure!(
            !claims.iter().any(|r| r.id < mine),
            "an earlier apply claim won; nothing applied"
        );
        let versions = resolve(client, batch)?;
        execute(
            client,
            batch,
            &versions,
            trusted,
            &from.url,
            &mut detail.actions,
        )
    })();
    detail.outcome = match &result {
        Ok(()) => Outcome::Applied,
        Err(_) => Outcome::Stopped,
    };
    // Never copy API error text into a bot-authored comment: it could
    // forge a fence or marker under this login.
    let lines = readable(&detail);
    let confirmed = lines.join("\n");
    let body = receipt_body(&detail, &from.url, &asked, trusted.operator, &dated)?;
    send(client, "PATCH", &edit, &json!({"body": body}), 0).with_context(|| {
        format!(
            "could not update the apply receipt; its claim remains, do not replay. Confirmed writes:\n{confirmed}"
        )
    })?;
    result.with_context(|| {
        format!(
            "session apply stopped; confirmed writes:\n{confirmed}\nClaim retained; do not replay."
        )
    })?;
    Ok((lines.into_iter())
        .chain(receipt_url.map(|url| format!("Receipt: {url}")))
        .collect())
}

/// Resolves every issue the batch's actions name as a GitHub issue before
/// any write: the REST paths a close, a new body or a comment take
/// address pull requests by the same numbers, so a batch naming a pull
/// request would otherwise close it or replace its description as the
/// bot. A number that is no issue, or one GitHub resolves to another
/// issue than named, stops the apply with nothing written. Epics are
/// resolved as issues by their own node-id read. Returns each issue's
/// `updatedAt` now, by the batch's spelling of it, read once here and
/// not again after the apply's own writes to it moved it on.
fn resolve<C: Transport>(
    client: &C,
    batch: &SessionBatch,
) -> Result<BTreeMap<String, OffsetDateTime>> {
    let mut versions = BTreeMap::new();
    for action in &batch.actions {
        let Some((issue, _)) = action.target() else {
            continue;
        };
        if versions.contains_key(&issue.to_string()) {
            continue;
        }
        let (owner, name) = issue.repo.split_once('/').context("issue repository")?;
        let data = client
            .graphql(
                IDENTITY_QUERY,
                json!({"owner": owner, "name": name, "number": issue.number}),
            )
            .with_context(|| format!("resolving {issue}; nothing applied"))?;
        let node = data
            .pointer("/repository/issue")
            .filter(|n| n.is_object())
            .with_context(|| format!("{issue} is not an issue; nothing applied"))?;
        let number = node["number"].as_u64();
        let repo = node["repository"]["nameWithOwner"].as_str();
        ensure!(
            number == Some(issue.number)
                && repo.is_some_and(|r| r.eq_ignore_ascii_case(&issue.repo)),
            "{issue} resolves to {}#{}, not itself; nothing applied",
            repo.unwrap_or("?"),
            number.map_or("?".to_owned(), |n| n.to_string())
        );
        let updated = (node["updatedAt"].as_str())
            .with_context(|| format!("{issue} has no updatedAt; nothing applied"))?;
        let updated = OffsetDateTime::parse(updated, &Rfc3339)
            .with_context(|| format!("{issue} has an invalid updatedAt; nothing applied"))?;
        versions.insert(issue.to_string(), updated);
    }
    Ok(versions)
}

/// Each action's writes, in the batch's order, recorded as each is
/// confirmed; the first failed request ends it. An action whose issue's
/// `updated_at` in `versions` is not the one it was decided from is
/// skipped and recorded as such: the session's view of that issue is
/// stale, and the next session reads the new text.
fn execute<C: Transport + Rest>(
    client: &C,
    batch: &SessionBatch,
    versions: &BTreeMap<String, OffsetDateTime>,
    trusted: &Trusted,
    batch_url: &str,
    confirmed: &mut Vec<Confirmed>,
) -> Result<()> {
    let from = format!("From [this session batch]({batch_url}).");
    for (index, action) in batch.actions.iter().enumerate() {
        let about = match action {
            Action::Close { .. } | Action::Rescope { .. } => None,
            Action::SetPriority { value, .. } => Some(value.clone()),
            Action::Attach { epic, .. } => Some(epic.to_string()),
            Action::Plan { plan, .. } => Some(plan.kind.clone()),
            Action::Ask { domain, .. } => Some(domain.clone()),
        };
        // No version means the issue was never resolved: a guard that
        // fails open would write from a view nothing checked.
        let skipped = match action.target() {
            Some((issue, read)) => {
                let now = (versions.get(&issue.to_string()))
                    .with_context(|| format!("{issue} was not resolved before writing"))?;
                (*now != read).then(|| {
                    format!(
                        "updated {}, not {} as the session read it",
                        stamp(*now),
                        stamp(read)
                    )
                })
            }
            None => None,
        };
        confirmed.push(Confirmed {
            action: index + 1,
            kind: action.kind().into(),
            issue: action.target().map(|(issue, _)| issue.to_string()),
            detail: about,
            skipped,
            writes: Vec::new(),
        });
        let at = confirmed.len() - 1;
        if confirmed[at].skipped.is_some() {
            continue;
        }
        let wrote = |confirmed: &mut Vec<Confirmed>, write: &str| {
            confirmed[at].writes.push(write.into());
        };
        match action {
            Action::Close { issue, reason, .. } => {
                comment(client, issue, &format!("{reason}\n\n{from}"))?;
                wrote(confirmed, "comment");
                let body = json!({"state": "closed", "state_reason": "not_planned"});
                send(client, "PATCH", &issue_url(issue), &body, 0)?;
                wrote(confirmed, "patch");
            }
            Action::Rescope {
                issue,
                body,
                reason,
                ..
            } => {
                send(
                    client,
                    "PATCH",
                    &issue_url(issue),
                    &json!({"body": body}),
                    0,
                )?;
                wrote(confirmed, "patch");
                comment(client, issue, &format!("{reason}\n\n{from}"))?;
                wrote(confirmed, "comment");
            }
            Action::SetPriority { issue, value, .. } => {
                let priorities = (trusted.priorities).context("no board ids")?;
                let option = priorities.options.get(value).context("no option id")?;
                set_option(
                    client,
                    issue,
                    (&priorities.project, &priorities.field, option),
                )?;
                wrote(confirmed, "priority");
            }
            Action::Attach { issue, epic, .. } => {
                let parent = node_id(client, epic)?;
                link(client, &parent, issue)?;
                wrote(confirmed, "link");
            }
            Action::Plan { issue, plan, .. } => {
                comment(client, issue, &plan_comment(plan, &from))?;
                wrote(confirmed, "comment");
            }
            Action::Ask { .. } => wrote(confirmed, "receipt"),
        }
    }
    Ok(())
}

fn issue_url(issue: &IssueRef) -> String {
    format!("{API_ROOT}repos/{}/issues/{}", issue.repo, issue.number)
}

/// Posts `body` on `issue`.
fn comment<R: Rest>(client: &R, issue: &IssueRef, body: &str) -> Result<()> {
    let url = format!("{}/comments", issue_url(issue));
    send(client, "POST", &url, &json!({"body": body}), 0).map(drop)
}

/// The comment that posts `plan`, a job plan the pass starts as any plan
/// of the bot's; `from` names the batch it came from.
fn plan_comment(plan: &JobPlan, from: &str) -> String {
    let mut job = json!({
        "schema": JOB_PLAN_SCHEMA,
        "repo": plan.repo,
        "item": plan.item,
        "kind": plan.kind,
        "runner": plan.runner,
        "brief": plan.brief,
    });
    if !plan.pool.is_empty() {
        job["pool"] = plan.pool.as_str().into();
    }
    if let Some(effort) = plan.effort {
        job["effort"] = effort.as_str().into();
    }
    if let Some(target) = &plan.target_repo {
        job["target_repo"] = target.as_str().into();
    }
    format!("{PLAN_INTRO} {from}\n\n```{PLAN_FENCE}\n{job:#}\n```\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SESSION_BATCH_SCHEMA;
    use serde_json::Value;
    use std::cell::RefCell;

    const REPO: &str = "example/work";
    const BOT: &str = "bot";
    const OPERATOR: &str = "operator";
    const FIXTURE: &str = include_str!("../../../fixtures/pass/session-batch.md");

    fn anchor() -> ItemRef {
        crate::github::parse_github_issue("example/work#40", None, "github.com").unwrap()
    }

    fn bounds() -> Bounds {
        Bounds {
            repositories: vec![REPO.into(), "example/epics".into()],
            priorities: vec!["P0".into(), "P1".into()],
            domains: vec!["budget".into()],
        }
    }

    fn priorities() -> Priorities {
        Priorities {
            project: "PVT_1".into(),
            field: "F_priority".into(),
            options: [("P0", "O_0"), ("P1", "O_1")]
                .into_iter()
                .map(|(n, id)| (n.to_owned(), id.to_owned()))
                .collect(),
        }
    }

    fn at(stamp: &str) -> OffsetDateTime {
        OffsetDateTime::parse(stamp, &Rfc3339).unwrap()
    }

    /// A comment node on the anchor, as the forge returns it.
    fn node(id: u64, author: &str, editor: Option<&str>, body: &str) -> Value {
        json!({
            "url": format!("https://github.com/example/work/issues/40#issuecomment-{id}"),
            "body": body, "createdAt": "2026-11-09T06:10:00Z",
            "lastEditedAt": editor.map(|_| "2026-11-09T06:20:00Z"),
            "author": {"login": author}, "editor": editor.map(|e| json!({"login": e})),
        })
    }

    fn batch_body(actions: Value) -> String {
        let batch = json!({"schema": SESSION_BATCH_SCHEMA, "repo": REPO, "actions": actions});
        format!("Summary.\n\n```{SESSION_FENCE}\n{batch}\n```\n")
    }

    /// When the session read each issue, which the forge answers as each
    /// issue's `updatedAt` unless [`Forge::changed`] names it.
    const READ: &str = "2026-11-09T06:05:00Z";
    /// The `updatedAt` of an issue that changed since the session read it.
    const CHANGED: &str = "2026-11-09T06:45:00Z";

    fn actions() -> Value {
        json!([
            {"type": "close_issue", "issue": "example/work#1", "updated_at": READ, "reason": "Done by #2."},
            {"type": "rescope_issue", "issue": "example/work#3", "updated_at": READ, "body": "Less.", "reason": "Split."},
            {"type": "set_priority", "issue": "example/work#4", "updated_at": READ, "value": "P1"},
            {"type": "attach_to_epic", "issue": "example/work#5", "updated_at": READ, "epic": "example/epics#7"},
            {"type": "post_plan", "issue": "example/work#6", "updated_at": READ, "plan": {
                "schema": JOB_PLAN_SCHEMA, "kind": "implement", "runner": "hosted", "brief": "Next."}},
            {"type": "ask_operator", "domain": "budget", "question": "More?"}
        ])
    }

    fn comments(nodes: &[Value]) -> Vec<Comment> {
        nodes
            .iter()
            .map(|n| crate::plan::comment(n).unwrap())
            .collect()
    }

    /// The anchor's comments: the plan (1), its record (2) and then
    /// `nodes`, read with every comment posted since; writes are kept,
    /// and the one whose method and URL or mutation hold `fail` fails.
    struct Forge {
        nodes: RefCell<Vec<Value>>,
        writes: RefCell<Vec<(String, String, Value)>>,
        fail: Option<&'static str>,
        /// The posted receipt's author, the bot unless another token.
        login: &'static str,
        /// The numbers that are pull requests, which resolve to no issue.
        pulls: Vec<u64>,
        /// The issues updated at [`CHANGED`], since the session read them.
        changed: Vec<u64>,
    }

    impl Forge {
        fn new(nodes: Vec<Value>) -> Self {
            let mut all = vec![
                node(1, BOT, None, "```agent-board-plan\n{}\n```"),
                node(2, BOT, None, "<!-- agent-board-dispatch: 1@now -->"),
            ];
            all.extend(nodes);
            Self {
                nodes: RefCell::new(all),
                writes: RefCell::new(Vec::new()),
                fail: None,
                login: BOT,
                pulls: Vec::new(),
                changed: Vec::new(),
            }
        }

        fn written(&self) -> Vec<String> {
            (self.writes.borrow().iter())
                .map(|(m, u, _)| format!("{m} {}", u.trim_start_matches(API_ROOT)))
                .collect()
        }

        fn receipt(&self) -> String {
            let nodes = self.nodes.borrow();
            let receipt = nodes
                .iter()
                .rev()
                .find(|n| has_fence(n["body"].as_str().unwrap_or_default(), APPLY_FENCE));
            receipt
                .map(|n| n["body"].as_str().unwrap().to_owned())
                .unwrap()
        }
    }

    impl Transport for Forge {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            if query.contains("comments(first") {
                return Ok(json!({"repository": {"issue": {"comments": {
                    "nodes": *self.nodes.borrow(), "pageInfo": {"hasNextPage": false}}}}}));
            }
            if query.contains("issue(number: $number) { id } }") {
                return Ok(
                    json!({"repository": {"issue": {"id": format!("I_{}", variables["number"])}}}),
                );
            }
            if query == IDENTITY_QUERY {
                let number = variables["number"].as_u64().unwrap();
                let updated = match self.changed.contains(&number) {
                    true => CHANGED,
                    false => READ,
                };
                let issue = match self.pulls.contains(&number) {
                    true => Value::Null,
                    false => {
                        json!({"number": number, "updatedAt": updated, "repository": {"nameWithOwner":
                        format!("{}/{}", variables["owner"].as_str().unwrap(), variables["name"].as_str().unwrap())}})
                    }
                };
                return Ok(json!({"repository": {"issue": issue}}));
            }
            let name = [
                "addSubIssue",
                "addProjectV2ItemById",
                "updateProjectV2ItemFieldValue",
            ]
            .into_iter()
            .find(|m| query.contains(m))
            .context("unexpected query")?;
            ensure!(self.fail != Some(name), "injected failure at {name}");
            self.writes
                .borrow_mut()
                .push(("GRAPHQL".into(), name.into(), variables));
            Ok(json!({"addProjectV2ItemById": {"item": {"id": "PVTI_4"}},
                "updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "PVTI_4"}},
                "addSubIssue": {"issue": {"id": "I_7"}}}))
        }
    }

    impl Rest for Forge {
        fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<crate::poll::RestResponse> {
            anyhow::bail!("GET {url}")
        }

        fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
            let what = format!("{method} {}", url.trim_start_matches(API_ROOT));
            ensure!(
                self.fail.is_none_or(|f| !what.contains(f)),
                "injected failure at {what}"
            );
            self.writes
                .borrow_mut()
                .push((method.into(), url.into(), body.clone()));
            let id = 100 + self.writes.borrow().len() as u64;
            if method == "POST" && what.ends_with("/issues/40/comments") {
                let posted = node(id, self.login, None, body["body"].as_str().unwrap());
                self.nodes.borrow_mut().push(posted.clone());
                return Ok((201, json!({"id": id, "html_url": posted["url"]})));
            }
            if method == "PATCH" && what.contains("/issues/comments/") {
                let edited: u64 = what.rsplit('/').next().unwrap().parse()?;
                for n in self.nodes.borrow_mut().iter_mut() {
                    if n["url"].as_str().unwrap().ends_with(&format!("-{edited}")) {
                        n["body"] = body["body"].clone();
                        n["lastEditedAt"] = json!("2026-11-09T07:00:00Z");
                        n["editor"] = json!({"login": self.login});
                    }
                }
            }
            Ok((200, json!({})))
        }
    }

    fn run(forge: &Forge) -> Result<Vec<String>> {
        let comments = comments(&forge.nodes.borrow());
        let from = answer(&comments, &[OPERATOR, BOT], 2).unwrap().unwrap();
        let batch = check(from, &anchor(), &bounds()).unwrap();
        let priorities = priorities();
        let trusted = Trusted {
            operator: OPERATOR,
            claimers: [OPERATOR, BOT],
            since: at(DUE),
            priorities: Some(&priorities),
        };
        apply(
            forge,
            &anchor(),
            from,
            &batch,
            &trusted,
            at("2026-11-09T07:00:00Z"),
        )
    }

    /// When the day's sessions were due: before every comment here.
    const DUE: &str = "2026-11-09T06:00:00Z";

    #[test]
    fn answer_table() {
        let batch = batch_body(json!([]));
        for (name, nodes, want) in [
            ("no batch", vec![], Ok(None)),
            (
                "the bot's after the record",
                vec![node(3, BOT, None, &batch)],
                Ok(Some(3)),
            ),
            (
                "the operator's after the record",
                vec![node(3, OPERATOR, None, &batch)],
                Ok(Some(3)),
            ),
            (
                "a visitor's",
                vec![node(3, "visitor", None, &batch)],
                Ok(None),
            ),
            (
                "the newest wins",
                vec![node(3, BOT, None, &batch), node(4, BOT, None, &batch)],
                Ok(Some(4)),
            ),
            (
                "one edited by a visitor",
                vec![node(3, BOT, Some("visitor"), &batch)],
                Err(
                    "its batch: newest plan https://github.com/example/work/issues/40#issuecomment-3 was last edited by visitor",
                ),
            ),
            (
                "one edited by the bot",
                vec![node(3, BOT, Some(BOT), &batch)],
                Ok(Some(3)),
            ),
        ] {
            let forge = Forge::new(nodes);
            let comments = comments(&forge.nodes.borrow());
            let got = answer(&comments, &[OPERATOR, BOT], 2)
                .map(|c| c.and_then(|c| comment_id(&c.url)))
                .map_err(|e| format!("{e:#}"));
            match want {
                Ok(id) => assert_eq!(got, Ok(id), "{name}"),
                Err(error) => assert!(
                    got.as_ref().is_err_and(|e| e.contains(error)),
                    "{name}: {got:?}"
                ),
            }
        }
        // A batch before the record answers an earlier plan, not this one.
        let forge = Forge::new(vec![]);
        forge
            .nodes
            .borrow_mut()
            .insert(1, node(0, BOT, None, &batch));
        let comments = comments(&forge.nodes.borrow());
        assert!(answer(&comments, &[OPERATOR, BOT], 2).unwrap().is_none());
    }

    #[test]
    fn check_refuses_another_repository_or_malformed_batches() {
        let elsewhere = batch_body(json!([])).replace(REPO, "example/epics");
        let malformed = batch_body(json!([{"type": "close_issue"}]));
        let unknown = format!("```{SESSION_FENCE}\n{{\"schema\": \"x\"}}\n```");
        for (body, error) in [
            (
                elsewhere.as_str(),
                "its repo example/epics is not example/work, the anchor's repository",
            ),
            (malformed.as_str(), "action 1: no issue"),
            (unknown.as_str(), "unsupported session batch schema"),
        ] {
            let comment = crate::plan::comment(&node(3, BOT, None, body)).unwrap();
            let got = format!("{:#}", check(&comment, &anchor(), &bounds()).unwrap_err());
            assert!(got.contains(error), "{got}");
        }
        // No action names the anchor, the session's own issue.
        for action in [
            json!({"type": "close_issue", "issue": "example/work#40", "updated_at": READ, "reason": "Done."}),
            json!({"type": "set_priority", "issue": "example/WORK#40", "updated_at": READ, "value": "P0"}),
            json!({"type": "attach_to_epic", "issue": "example/work#5", "updated_at": READ, "epic": "example/work#40"}),
            json!({"type": "post_plan", "issue": "example/work#40", "updated_at": READ, "plan": {
                "schema": JOB_PLAN_SCHEMA, "kind": "triage", "runner": "hosted", "brief": "Again."}}),
        ] {
            let body = batch_body(json!([
                {"type": "ask_operator", "domain": "budget", "question": "More?"}, action]));
            let comment = crate::plan::comment(&node(3, BOT, None, &body)).unwrap();
            let got = format!("{:#}", check(&comment, &anchor(), &bounds()).unwrap_err());
            assert_eq!(
                got,
                "action 2 names the anchor example/work#40, the session's own issue"
            );
        }
        let fixture = crate::plan::comment(&node(3, BOT, None, FIXTURE)).unwrap();
        let bounds = Bounds {
            repositories: vec![
                "cgwalters-forge/agent-board".into(),
                "cgwalters-forge/tracker".into(),
            ],
            priorities: vec!["P1".into()],
            domains: vec!["budget".into()],
        };
        let got = format!("{:#}", check(&fixture, &anchor(), &bounds).unwrap_err());
        assert!(
            got.contains("its repo cgwalters-forge/agent-board is not example/work"),
            "{got}"
        );
    }

    #[test]
    fn applies_once_with_a_receipt() {
        let forge = Forge::new(vec![node(3, BOT, None, &batch_body(actions()))]);
        let lines = run(&forge).unwrap();
        assert_eq!(
            lines,
            [
                "Closed example/work#1 as not planned",
                "Rescoped example/work#3",
                "Set example/work#4 to P1",
                "Attached example/work#5 to example/epics#7",
                "Posted a plan (implement) on example/work#6",
                "Asked a question (budget), below",
                "Receipt: https://github.com/example/work/issues/40#issuecomment-101",
            ]
        );
        assert_eq!(
            forge.written(),
            [
                "POST repos/example/work/issues/40/comments",
                "POST repos/example/work/issues/1/comments",
                "PATCH repos/example/work/issues/1",
                "PATCH repos/example/work/issues/3",
                "POST repos/example/work/issues/3/comments",
                "GRAPHQL addProjectV2ItemById",
                "GRAPHQL updateProjectV2ItemFieldValue",
                "GRAPHQL addSubIssue",
                "POST repos/example/work/issues/6/comments",
                "PATCH repos/example/work/issues/comments/101",
            ]
        );
        let writes = forge.writes.borrow();
        assert_eq!(
            writes[2].2,
            json!({"state": "closed", "state_reason": "not_planned"})
        );
        assert_eq!(writes[3].2, json!({"body": "Less."}));
        assert_eq!(
            writes[6].2,
            json!({"project": "PVT_1", "item": "PVTI_4", "field": "F_priority", "option": "O_1"})
        );
        assert_eq!(
            writes[7].2,
            json!({"issueId": "I_7", "subIssueUrl": "https://github.com/example/work/issues/5"})
        );
        let plan = JobPlan::from_comment(writes[8].2["body"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            (plan.repo.as_str(), plan.item, plan.kind.as_str()),
            (REPO, 6, "implement")
        );
        assert!(
            writes[1].2["body"]
                .as_str()
                .unwrap()
                .starts_with("Done by #2.\n\nFrom [this session batch](https://github.com/example/work/issues/40#issuecomment-3)")
        );
        drop(writes);
        let receipt = forge.receipt();
        assert!(
            receipt.starts_with("Applied [this session batch](https://github.com/example/work/issues/40#issuecomment-3) at 2026-11-09T07:00:00Z.\n\n- Closed example/work#1 as not planned\n"),
            "{receipt}"
        );
        assert!(
            receipt.contains("Questions for @operator:\n- (budget) More?\n"),
            "{receipt}"
        );
        let comments = comments(&forge.nodes.borrow());
        let (read, unread) = receipts(&comments, &[OPERATOR, BOT]);
        assert!(unread.is_empty());
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].detail.outcome, Outcome::Applied);
        assert_eq!(read[0].detail.batch, "3@2026-11-09T06:10:00Z");
        assert!(read[0].detail.actions.iter().all(Confirmed::whole));

        // Applied again, and a newer batch the same day: the day's earlier
        // claim wins each, whichever batch it names, and nothing is written.
        for newer in [None, Some(node(4, BOT, None, &batch_body(json!([]))))] {
            if let Some(newer) = newer {
                forge.nodes.borrow_mut().push(newer);
            }
            let before = forge.writes.borrow().len();
            let error = format!("{:#}", run(&forge).unwrap_err());
            assert!(
                error.contains("an earlier apply claim won; nothing applied"),
                "{error}"
            );
            let since = forge.written()[before..].to_vec();
            assert_eq!(
                since,
                [
                    "POST repos/example/work/issues/40/comments".to_owned(),
                    format!("PATCH repos/example/work/issues/comments/{}", 101 + before)
                ]
            );
        }
    }

    /// A batch naming a pull request's number, or a number that is no
    /// issue: the apply stops before any action write, with the claim
    /// kept, since the REST paths a close, a new body or a comment take
    /// would change the pull request.
    #[test]
    fn a_pull_request_target_stops_the_apply_before_any_action_write() {
        for action in [
            json!({"type": "close_issue", "issue": "example/work#12", "updated_at": READ, "reason": "Done."}),
            json!({"type": "rescope_issue", "issue": "example/work#12", "updated_at": READ, "body": "Less.", "reason": "Split."}),
            json!({"type": "post_plan", "issue": "example/work#12", "updated_at": READ, "plan": {
                "schema": JOB_PLAN_SCHEMA, "kind": "implement", "runner": "hosted", "brief": "Next."}}),
            json!({"type": "set_priority", "issue": "example/work#12", "updated_at": READ, "value": "P1"}),
            json!({"type": "attach_to_epic", "issue": "example/work#12", "updated_at": READ, "epic": "example/epics#7"}),
        ] {
            let mut actions = actions();
            actions.as_array_mut().unwrap().insert(0, action.clone());
            let mut forge = Forge::new(vec![node(3, BOT, None, &batch_body(actions))]);
            forge.pulls = vec![12];
            let error = format!("{:#}", run(&forge).unwrap_err());
            assert!(
                error.contains("example/work#12 is not an issue; nothing applied"),
                "{action}: {error}"
            );
            assert_eq!(
                forge.written(),
                [
                    "POST repos/example/work/issues/40/comments",
                    "PATCH repos/example/work/issues/comments/101"
                ],
                "{action}"
            );
            let comments = comments(&forge.nodes.borrow());
            let (read, _) = receipts(&comments, &[OPERATOR, BOT]);
            assert_eq!(read[0].detail.outcome, Outcome::Stopped, "{action}");
            assert!(read[0].detail.actions.is_empty(), "{action}");
        }
    }

    /// An action on an issue that changed since the session read it (its
    /// `updatedAt` is not the action's `updated_at`) is skipped, with the
    /// reason on the receipt, and the rest of the batch is applied: the
    /// session's view of that issue is stale, and the next session reads
    /// the new text. An unchanged issue is written to as before.
    #[test]
    fn an_issue_changed_since_the_session_read_it_is_skipped() {
        let skipped = |action: usize, kind: &str, issue: u64| {
            format!(
                "Skipped action {action} ({kind}) on example/work#{issue}: updated {CHANGED}, not {READ} as the session read it"
            )
        };
        let applied = [
            "Closed example/work#1 as not planned",
            "Rescoped example/work#3",
            "Set example/work#4 to P1",
            "Attached example/work#5 to example/epics#7",
            "Posted a plan (implement) on example/work#6",
            "Asked a question (budget), below",
        ];
        let everything = [
            skipped(1, "close_issue", 1),
            skipped(2, "rescope_issue", 3),
            skipped(3, "set_priority", 4),
            skipped(4, "attach_to_epic", 5),
            skipped(5, "post_plan", 6),
            applied[5].to_owned(),
        ];
        for (name, changed, lines, writes) in [
            (
                "no issue changed",
                vec![],
                applied.map(str::to_owned).to_vec(),
                10,
            ),
            (
                "the rescoped issue changed",
                vec![3],
                [
                    applied[0].to_owned(),
                    skipped(2, "rescope_issue", 3),
                    applied[2].to_owned(),
                    applied[3].to_owned(),
                    applied[4].to_owned(),
                    applied[5].to_owned(),
                ]
                .to_vec(),
                8,
            ),
            (
                "every issue changed",
                vec![1, 3, 4, 5, 6],
                everything.to_vec(),
                2,
            ),
        ] {
            let mut forge = Forge::new(vec![node(3, BOT, None, &batch_body(actions()))]);
            forge.changed = changed.clone();
            let got = run(&forge).unwrap();
            assert_eq!(got[..got.len() - 1], lines[..], "{name}");
            let written = forge.written();
            assert_eq!(written.len(), writes, "{name}: {written:#?}");
            for issue in &changed {
                let path = format!("/issues/{issue}");
                assert!(
                    !written
                        .iter()
                        .any(|w| w.ends_with(&path) || w.contains(&format!("{path}/"))),
                    "{name}: {written:#?}"
                );
            }
            let comments = comments(&forge.nodes.borrow());
            let (read, _) = receipts(&comments, &[OPERATOR, BOT]);
            assert_eq!(read[0].detail.outcome, Outcome::Applied, "{name}");
            for confirmed in &read[0].detail.actions {
                let stale = (confirmed.issue.as_deref())
                    .and_then(|issue| issue.rsplit('#').next()?.parse::<u64>().ok())
                    .is_some_and(|n| changed.contains(&n));
                assert_eq!(confirmed.skipped.is_some(), stale, "{name}: {confirmed:?}");
                assert_eq!(confirmed.whole(), !stale, "{name}: {confirmed:?}");
            }
            if !changed.is_empty() {
                let receipt = forge.receipt();
                assert!(
                    receipt.contains(&format!("- {}\n", skipped(2, "rescope_issue", 3)))
                        && receipt.contains(&format!(
                            "\"skipped\": \"updated {CHANGED}, not {READ} as the session read it\""
                        )),
                    "{name}: {receipt}"
                );
            }
        }
    }

    #[test]
    fn a_failed_request_stops_the_apply_and_keeps_the_claim() {
        let mut forge = Forge::new(vec![node(3, BOT, None, &batch_body(actions()))]);
        forge.fail = Some("addSubIssue");
        let error = format!("{:#}", run(&forge).unwrap_err());
        assert!(
            error.contains("session apply stopped; confirmed writes:\nClosed example/work#1 as not planned\nRescoped example/work#3\nSet example/work#4 to P1\nAction 4 (attach_to_epic) on example/work#5: stopped before any write\nClaim retained; do not replay."),
            "{error}"
        );
        assert!(
            !forge
                .written()
                .iter()
                .any(|w| w.contains("issues/6/comments"))
        );
        let receipt = forge.receipt();
        assert!(
            receipt.starts_with("Stopped applying [this session batch]"),
            "{receipt}"
        );
        let comments = comments(&forge.nodes.borrow());
        let (read, _) = receipts(&comments, &[OPERATOR, BOT]);
        assert_eq!(read[0].detail.outcome, Outcome::Stopped);
        assert_eq!(read[0].detail.actions.len(), 4);
        assert_eq!(read[0].detail.actions[3].writes, Vec::<String>::new());
    }

    #[test]
    fn nothing_is_written_before_the_claim_lands_as_a_claimers() {
        let mut forge = Forge::new(vec![node(3, BOT, None, &batch_body(actions()))]);
        forge.login = "someone";
        let error = format!("{:#}", run(&forge).unwrap_err());
        assert!(
            error.contains("the claim did not land as the bot's; nothing applied"),
            "{error}"
        );
        assert_eq!(forge.written().len(), 2, "the claim and its update");
    }

    #[test]
    fn receipts_table() {
        let detail = |outcome: &str| {
            format!(
                "Applied.\n\n```{APPLY_FENCE}\n{{\"schema\": \"{RECEIPT_SCHEMA}\", \"batch\": \"3@t\", \"outcome\": \"{outcome}\", \"actions\": [], \"later\": 1}}\n```\n"
            )
        };
        for (name, node, read, unread) in [
            (
                "the bot's applied",
                node(9, BOT, None, &detail("applied")),
                1,
                0,
            ),
            (
                "the operator's stopped",
                node(9, OPERATOR, None, &detail("stopped")),
                1,
                0,
            ),
            (
                "a visitor's",
                node(9, "visitor", None, &detail("applied")),
                0,
                0,
            ),
            (
                "edited by a visitor",
                node(9, BOT, Some("visitor"), &detail("applied")),
                0,
                1,
            ),
            (
                "edited by the bot",
                node(9, BOT, Some(BOT), &detail("claimed")),
                1,
                0,
            ),
            (
                "not JSON",
                node(9, BOT, None, &format!("```{APPLY_FENCE}\nnope\n```")),
                0,
                1,
            ),
            (
                "another schema",
                node(
                    9,
                    BOT,
                    None,
                    &detail("applied").replace(RECEIPT_SCHEMA, "x/y"),
                ),
                0,
                1,
            ),
            ("no fence", node(9, BOT, None, "Applied."), 0, 0),
        ] {
            let comments = comments(&[node]);
            let (got, bad) = receipts(&comments, &[OPERATOR, BOT]);
            assert_eq!((got.len(), bad.len()), (read, unread), "{name}");
        }
    }

    #[test]
    fn planned_words_and_write_steps() {
        let comment = crate::plan::comment(&node(3, BOT, None, &batch_body(actions()))).unwrap();
        let batch = check(&comment, &anchor(), &bounds()).unwrap();
        let (lines, writes) = planned(&batch);
        assert_eq!(
            lines,
            [
                "Close example/work#1 as not planned",
                "Rescope example/work#3",
                "Set example/work#4 to P1",
                "Attach example/work#5 to example/epics#7",
                "Post a plan (implement) on example/work#6",
                "Ask the operator (budget)"
            ]
        );
        assert_eq!(writes, 6, "the receipt and five writing actions");
        let empty = crate::plan::comment(&node(3, BOT, None, &batch_body(json!([])))).unwrap();
        let (lines, writes) = planned(&check(&empty, &anchor(), &bounds()).unwrap());
        assert!(lines.is_empty() && writes == 1);
    }

    #[test]
    fn a_priority_without_the_boards_ids_is_refused_before_the_claim() {
        let forge = Forge::new(vec![node(3, BOT, None, &batch_body(actions()))]);
        let comments = comments(&forge.nodes.borrow());
        let from = answer(&comments, &[OPERATOR, BOT], 2).unwrap().unwrap();
        let batch = check(from, &anchor(), &bounds()).unwrap();
        let trusted = Trusted {
            operator: OPERATOR,
            claimers: [OPERATOR, BOT],
            since: at(DUE),
            priorities: None,
        };
        let error = apply(
            &forge,
            &anchor(),
            from,
            &batch,
            &trusted,
            at("2026-11-09T07:00:00Z"),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("there is no board to set Priority on"),
            "{error:#}"
        );
        assert!(forge.written().is_empty());
    }
}
