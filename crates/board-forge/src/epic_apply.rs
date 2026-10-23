//! Opt-in epic mutations, with a dispatch-style claim preventing blind replay.
//!
//! An apply's receipt is one comment on the epic (docs/epic-plans.md#apply):
//! a readable line per sub-issue, then a fenced [`APPLY_MARKER`] block with
//! the machine detail ([`Detail`]). Older receipts, a hidden claim marker
//! with a separate fingerprint comment, are still read ([`receipts`]).
use crate::epic::{
    Action, Bounds, EPIC_PLAN_FENCE, Epic, EpicPlan, IssueRef, Target, epic_labelled_by, read_epic,
};
use crate::github::{API_ROOT, DEFAULT_HOST, Transport, is_github_login};
use crate::plan::{
    Comment, comment_id, fenced_json, has_fence, issue_comments, newest_fenced, plan_id, send,
    viewer,
};
use crate::poll::Rest;
use anyhow::{Context, Result, ensure};
use board_core::ItemRef;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// The receipt's fence info string, and the older receipts' marker name.
pub const APPLY_MARKER: &str = "agent-board-epic-apply";
/// The older receipts' separate fingerprint comment, still read.
pub const FINGERPRINT_MARKER: &str = "agent-board-epic-fingerprint";
/// The schema of a receipt's machine detail.
pub const RECEIPT_SCHEMA: &str = "agent-board/epic-apply/v1";

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

/// The newest plan itself cannot be applied: not a plan a writer may make,
/// or refused against the epic now. A person, or a new plan, mends it; a
/// failed read is not this.
#[derive(Debug)]
pub struct Refused;

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the plan cannot be applied")
    }
}

impl std::error::Error for Refused {}

impl Preconditions {
    fn enabled(&self) -> bool {
        self.request.is_some() || self.comment.is_some() || self.updated_at.is_some()
    }

    fn check(&self, value: &Value, from: &Comment) -> Result<()> {
        let comment = comment_id(&from.url);
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
    /// Plan writer besides the operator; the bot when not configured.
    pub planner: String,
    pub bot: String,
    pub bounds: Bounds,
    pub project: String,
    pub priority_field: String,
    pub priority_field_id: String,
    pub priorities: BTreeMap<String, String>,
    pub default_priority: String,
    /// Require the newest `epic` labeling to be the operator's, so that
    /// someone with only triage access (or the bot) cannot start a
    /// decomposition the bot then carries out. Off by default.
    pub label_by_operator: bool,
    /// Let a plan's `close_epic` close the epic. Off by default, closing an
    /// epic is the operator's call alone: off, a plan writer cannot end a
    /// decomposition the operator still wants worked, which `moved_to` (the
    /// planner's own word that work moved) would otherwise let it do.
    pub close_epic: bool,
}

impl Config {
    pub fn from_json(value: &Value) -> Result<Self> {
        let object = value.as_object().context("epic config is not an object")?;
        for key in object.keys() {
            ensure!(
                [
                    "operator",
                    "planner",
                    "bot",
                    "repositories",
                    "project",
                    "priority_field",
                    "priority_field_id",
                    "priorities",
                    "default_priority",
                    "label_by_operator",
                    "close_epic"
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
        let planner = match object.get("planner") {
            None => bot.clone(),
            Some(_) => text("planner")?,
        };
        ensure!(
            [&operator, &planner, &bot]
                .iter()
                .all(|l| is_github_login(l)),
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
        let flag = |key: &str| -> Result<bool> {
            Ok(object
                .get(key)
                .map(|v| {
                    v.as_bool()
                        .with_context(|| format!("{key} is not a boolean"))
                })
                .transpose()?
                .unwrap_or(false))
        };
        let label_by_operator = flag("label_by_operator")?;
        // Else the bot's own labeling would pass as the operator's.
        ensure!(
            !label_by_operator || !operator.eq_ignore_ascii_case(&bot),
            "label_by_operator needs an operator other than the bot"
        );
        Ok(Self {
            operator,
            planner,
            bot,
            bounds,
            project: text("project")?,
            priority_field: text("priority_field")?,
            priority_field_id: text("priority_field_id")?,
            priorities,
            default_priority,
            label_by_operator,
            close_epic: flag("close_epic")?,
        })
    }

    /// The plan, checked against the bounds and the caller's opt-ins.
    fn validate(&self, value: &Value, epic: &Epic) -> Result<EpicPlan> {
        let plan = EpicPlan::validate(value, &self.bounds, epic)?;
        ensure!(
            self.close_epic || !plan.closes_epic(),
            "the plan closes the epic, which the configuration does not allow (close_epic)"
        );
        Ok(plan)
    }

    /// Whose plans count, as for `epic plan --operator --planner`. With a
    /// separate planner the bot's own comments (claims, status, poll markers)
    /// are not plans.
    pub fn writers(&self) -> [&str; 2] {
        [&self.operator, &self.planner]
    }

    /// Whose claims count: only the operator and the bot that applies.
    fn claimers(&self) -> [&str; 2] {
        [&self.operator, &self.bot]
    }
}

/// The sorted `[lowercase OWNER/REPO#N, OPEN-or-CLOSED]` pairs of every
/// current sub-issue: membership and state, not a digest of untrusted text.
/// Body and Priority drift are deliberately not represented.
pub fn fingerprint_pairs(epic: &Epic) -> Vec<(String, String)> {
    let mut states: Vec<_> = (epic.sub_issues.iter())
        .map(|i| (key(i), "OPEN".to_owned()))
        .chain(epic.closed.iter().map(|i| (key(i), "CLOSED".to_owned())))
        .collect();
    states.sort_unstable();
    states
}

/// `OWNER/REPO#N` in lower case, as receipts and censuses compare issues.
fn key(issue: &IssueRef) -> String {
    issue.to_string().to_ascii_lowercase()
}

// ---- Receipts ----

/// How far an apply got, as its receipt says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// Posted before the first write and not finished: the apply is under
    /// way, or was cut short before it could say how it ended.
    Claimed,
    Applied,
    /// A request failed; what it did is listed, and may be incomplete.
    Stopped,
    /// Cut short, and every action's effect was later confirmed by a pass
    /// from the receipt and the epic ([`unproved`]).
    Recovered,
}

impl Outcome {
    /// Whether the plan is done with: applied, or proved applied.
    pub fn applied(self) -> bool {
        matches!(self, Self::Applied | Self::Recovered)
    }
}

/// One plan action as far as its writes were confirmed. The writes are
/// named `create`, `link`, `priority`, `comment` and `patch` (a close or a
/// new body), or `satisfied` for a close of an issue already closed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Confirmed {
    /// Its 1-based position in the plan.
    pub action: usize,
    /// Its plan action type; empty in an older receipt, which did not say.
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// The issue it created or changed, `OWNER/REPO#N`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    /// The Priority it set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(default)]
    pub writes: Vec<String>,
}

impl Confirmed {
    /// Whether it confirmed `name`; a Priority set with its create is
    /// `folded`, made by the create's own write.
    fn wrote(&self, name: &str) -> bool {
        (self.writes.iter()).any(|w| w == name || (name == "priority" && w == "folded"))
    }
}

/// The writes a plan action of `kind` makes, by the names [`Confirmed`]
/// records them; a create with a job plan also posts it, a comment.
fn needs(kind: &str, job_plan: bool) -> &'static [&'static str] {
    match kind {
        "create_sub_issue" if job_plan => &["create", "link", "priority", "comment"],
        "create_sub_issue" => &["create", "link", "priority"],
        "close_sub_issue" | "close_epic" | "rescope_sub_issue" => &["comment", "patch"],
        "set_priority" => &["priority"],
        _ => &[],
    }
}

/// The machine detail of a receipt, its fenced [`APPLY_MARKER`] block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Detail {
    pub schema: String,
    /// The plan it claims: `COMMENT-ID@CHANGE-TIME` ([`plan_id`]).
    pub plan: String,
    pub outcome: Outcome,
    #[serde(default)]
    pub actions: Vec<Confirmed>,
    /// The epic's [`fingerprint_pairs`] once the apply ended, where it
    /// could be read: the baseline a later pass compares the epic with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<Vec<(String, String)>>,
}

impl Detail {
    fn new(plan: &str, outcome: Outcome) -> Self {
        Self {
            schema: RECEIPT_SCHEMA.into(),
            plan: plan.into(),
            outcome,
            actions: Vec::new(),
            fingerprint: None,
        }
    }

    /// The Priority the apply left on each issue it set one on, by
    /// lower-case `OWNER/REPO#N`.
    pub fn priorities(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for c in &self.actions {
            if let (Some(issue), Some(priority)) = (&c.issue, &c.priority) {
                out.insert(issue.to_ascii_lowercase(), priority.clone());
            }
        }
        out
    }
}

/// An apply's receipt on the epic: one comment, posted as the claim before
/// any write and updated as writes are confirmed.
#[derive(Clone, Debug, PartialEq)]
pub struct Receipt {
    pub url: String,
    pub id: u64,
    pub author: Option<String>,
    /// When it last changed: when the apply ended, for a finished one.
    pub changed: OffsetDateTime,
    pub detail: Detail,
}

/// The receipts on an epic by `claimers`, in comment order (this format's,
/// and the older hidden-marker claims with their fingerprint comments), and
/// the URLs of those that look like receipts but cannot be read: a fence
/// that does not parse, another schema, no comment id, or an edit by anyone
/// else. Each unreadable one may claim any plan, so none may be applied past
/// it. Unknown fields are ignored, so a later build's receipts still claim.
pub fn receipts(comments: &[Comment], claimers: &[&str]) -> (Vec<Receipt>, Vec<String>) {
    let claimer = |login: Option<&str>| {
        login.is_some_and(|a| claimers.iter().any(|w| w.eq_ignore_ascii_case(a)))
    };
    let trusted: Vec<&Comment> = (comments.iter())
        .filter(|c| claimer(c.author.as_deref()))
        .collect();
    let legacy_marker = format!("<!-- {APPLY_MARKER}: ");
    let (mut out, mut unread) = (Vec::new(), Vec::new());
    for (n, comment) in trusted.iter().enumerate() {
        let fenced = has_fence(&comment.body, APPLY_MARKER);
        if !fenced && !comment.body.starts_with(&legacy_marker) {
            continue;
        }
        let foreign =
            (comment.edited || comment.editor.is_some()) && !claimer(comment.editor.as_deref());
        let detail = match fenced {
            true => fenced_json(&comment.body, APPLY_MARKER)
                .ok()
                .flatten()
                .and_then(|value| serde_json::from_value::<Detail>(value).ok())
                .filter(|d| d.schema == RECEIPT_SCHEMA),
            false => legacy(comment, &trusted[n + 1..]),
        };
        match (detail, comment_id(&comment.url)) {
            (Some(detail), Some(id)) if !foreign => out.push(Receipt {
                url: comment.url.clone(),
                id,
                author: comment.author.clone(),
                changed: comment.changed,
                detail,
            }),
            _ => unread.push(comment.url.clone()),
        }
    }
    (out, unread)
}

/// An older receipt: a first line `<!-- agent-board-epic-apply: PLAN -->`,
/// then `TIME: Applied.`, `TIME: Stopped …` or the claim's `Applying …`,
/// then a `- Action N: …` line per confirmed write. Its fingerprint is in a
/// separate comment after it, whose second line is the same.
fn legacy(comment: &Comment, after: &[&Comment]) -> Option<Detail> {
    let mut lines = comment.body.lines();
    let plan = lines
        .next()?
        .trim_end()
        .strip_prefix(&format!("<!-- {APPLY_MARKER}: "))?
        .strip_suffix(" -->")?;
    let status = lines.next().unwrap_or_default().trim_end();
    let outcome = if status.ends_with(": Applied.") {
        Outcome::Applied
    } else if status.contains(": Stopped after a failed request") {
        Outcome::Stopped
    } else {
        Outcome::Claimed
    };
    let mut detail = Detail::new(plan, outcome);
    for line in lines {
        let Some((n, write)) = line
            .trim_end()
            .strip_prefix("- Action ")
            .and_then(|rest| rest.split_once(": "))
        else {
            continue;
        };
        let Ok(n) = n.parse::<usize>() else { continue };
        if !detail.actions.last().is_some_and(|c| c.action == n) {
            detail.actions.push(Confirmed {
                action: n,
                ..Confirmed::default()
            });
        }
        let Some(confirmed) = detail.actions.last_mut() else {
            continue;
        };
        let (name, issue, priority) = if let Some(path) = write.strip_prefix("POST ") {
            match path.strip_suffix("/issues") {
                Some(_) => ("create", None, None),
                None => ("comment", rest_issue(path), None),
            }
        } else if let Some(path) = write.strip_prefix("PATCH ") {
            ("patch", rest_issue(path), None)
        } else if let Some(issue) = write.strip_prefix("linked ") {
            let issue = IssueRef::parse(issue.trim_end_matches('.')).ok();
            // A link names the issue the create made.
            confirmed.issue = issue.as_ref().map(ToString::to_string);
            ("link", issue, None)
        } else if let Some(set) = write.strip_prefix("set ") {
            let (issue, value) = set.trim_end_matches('.').split_once(' ')?;
            let value = value.split_once('=').map(|(_, v)| v.to_owned());
            ("priority", IssueRef::parse(issue).ok(), value)
        } else if let Some(issue) = write.strip_suffix(" is already closed; nothing to do.") {
            ("satisfied", IssueRef::parse(issue).ok(), None)
        } else {
            continue;
        };
        if confirmed.issue.is_none() {
            confirmed.issue = issue.map(|i| i.to_string());
        }
        if priority.is_some() {
            confirmed.priority = priority;
        }
        confirmed.writes.push(name.into());
    }
    let marker = format!("<!-- {FINGERPRINT_MARKER}: ");
    detail.fingerprint = after
        .iter()
        .find(|c| {
            let mut lines = c.body.lines();
            lines.next().is_some_and(|l| l.starts_with(&marker))
                && lines.next().map(str::trim_end) == Some(status)
        })
        .and_then(|c| {
            let json = c.body.lines().next()?.strip_prefix(&marker)?;
            serde_json::from_str(json.trim_end().strip_suffix(" -->")?).ok()
        });
    Some(detail)
}

/// The claims of plan `id` by the operator or the bot, by comment id: the
/// oldest wins.
fn claims(comments: &[Comment], config: &Config, id: &str) -> Result<Vec<u64>> {
    let (read, unread) = receipts(comments, &config.claimers());
    ensure!(
        unread.is_empty(),
        "{} looks like an apply receipt but cannot be read; nothing changed, a person inspects it",
        unread.join(", ")
    );
    Ok((read.into_iter())
        .filter(|r| r.detail.plan == id)
        .map(|r| r.id)
        .collect())
}

/// How a receipt names an issue: `#N` in the epic's repository, a link
/// named `REPO#N` in another of its owner's, and `OWNER/REPO#N` elsewhere,
/// as GitHub links it. Without an epic, `OWNER/REPO#N`.
fn named(issue: &str, epic: Option<&IssueRef>) -> String {
    // Only what parses as an issue is shown: a receipt is read back from a
    // comment, and the bot writes it again.
    let Ok(parsed) = IssueRef::parse(issue) else {
        return "an issue".into();
    };
    let Some(epic) = epic else {
        return parsed.to_string();
    };
    let (owner, name) = parsed.repo.split_once('/').unwrap_or(("", &parsed.repo));
    let epic_owner = epic.repo.split_once('/').map_or("", |(o, _)| o);
    if parsed.repo.eq_ignore_ascii_case(&epic.repo) {
        format!("#{}", parsed.number)
    } else if owner.eq_ignore_ascii_case(epic_owner) {
        format!(
            "[{name}#{n}](https://{DEFAULT_HOST}/{repo}/issues/{n})",
            n = parsed.number,
            repo = parsed.repo
        )
    } else {
        parsed.to_string()
    }
}

/// `text` as a receipt line may show it: letters, digits and ` ._-`.
fn plain(text: &str) -> String {
    (text.chars())
        .filter(|c| c.is_ascii_alphanumeric() || " ._-".contains(*c))
        .collect()
}

/// One line per issue the apply touched, saying what it did: `Created
/// agentic-job#715 (P0)`, `Closed #611 as not planned`. `epic` names
/// issues as a comment on it links them; without one, in full.
pub fn readable(detail: &Detail, epic: Option<&IssueRef>) -> Vec<String> {
    let mut lines: Vec<(Option<String>, Vec<String>)> = Vec::new();
    for c in &detail.actions {
        let issue = c.issue.as_deref().map(|i| named(i, epic));
        let priority = c.priority.as_deref().map(plain);
        let wanted = needs(&c.kind, c.wrote("comment"));
        let whole = wanted.iter().all(|w| c.wrote(w));
        let phrase = match (c.kind.as_str(), &issue) {
            (_, Some(i)) if c.wrote("satisfied") => format!("{i} was already closed"),
            ("create_sub_issue", Some(i)) if whole => match &priority {
                Some(p) => format!("Created {i} ({p})"),
                None => format!("Created {i}"),
            },
            ("create_sub_issue", Some(i)) => {
                format!("Created {i}, then stopped before linking and prioritising it")
            }
            ("create_sub_issue", None) => {
                format!("Action {}: stopped before creating its sub-issue", c.action)
            }
            ("close_sub_issue", Some(i)) if whole => format!("Closed {i} as not planned"),
            ("rescope_sub_issue", Some(i)) if whole => format!("Rescoped {i}"),
            ("set_priority", Some(i)) => match &priority {
                Some(p) => format!("Set {i} to {p}"),
                None => format!("Set {i}'s Priority"),
            },
            ("close_epic", _) if whole => "Closed this epic".into(),
            (kind, i) => format!(
                "Action {}{}: stopped part way ({})",
                c.action,
                i.as_ref().map(|i| format!(" on {i}")).unwrap_or_default(),
                match plain(kind) {
                    kind if kind.is_empty() => plain(&c.writes.join(", ")),
                    kind => format!("{kind}: {}", plain(&c.writes.join(", "))),
                }
            ),
        };
        // A Priority the create already set is not said twice.
        let at = lines.iter_mut().find(|(i, _)| i.is_some() && *i == issue);
        match at {
            Some((_, phrases)) => {
                let created = phrases.iter().any(|p| p.starts_with("Created "));
                if !(created && c.kind == "set_priority") {
                    phrases.push(phrase);
                }
            }
            None => lines.push((issue, vec![phrase])),
        }
    }
    lines.into_iter().map(|(_, p)| p.join("; ")).collect()
}

fn receipt_body(detail: &Detail, plan_url: &str, epic: &IssueRef, at: &str) -> Result<String> {
    let plan = format!("[this epic plan]({plan_url})");
    let header = match detail.outcome {
        Outcome::Claimed => format!(
            "Applying {plan}. Do not replay it after a failure: this receipt says what was done."
        ),
        Outcome::Applied => format!("Applied {plan} at {at}."),
        Outcome::Stopped => format!(
            "Stopped applying {plan} at {at} after a failed request; what it did may be incomplete. Inspect it before replacing the plan."
        ),
        Outcome::Recovered => format!(
            "Applied {plan}: its receipt was cut short, and at {at} a pass confirmed each action's effect."
        ),
    };
    let lines: String = readable(detail, Some(epic))
        .iter()
        .map(|l| format!("- {l}\n"))
        .collect();
    let lines = match lines.is_empty() {
        true => String::new(),
        false => format!("{lines}\n"),
    };
    let json = serde_json::to_string_pretty(detail)?;
    // The older claim's first line too, which earlier builds read as the
    // claim, so that one running beside this never applies the plan again.
    let claim = format!("<!-- {APPLY_MARKER}: {} -->", detail.plan);
    Ok(format!(
        "{claim}\n{header}\n\n{lines}```{APPLY_MARKER}\n{json}\n```\n"
    ))
}

/// The actions of `plan` (the plan's JSON) whose effects a receipt cut
/// short does not prove, one phrase each; none when the apply is proved.
/// Only the receipt's confirmed identities name what it created, never a
/// title, so a sub-issue retitled, closed or reprioritised since still
/// counts. A close is also proved by its issue being closed now, and a
/// Priority by the board showing it (`priorities`, by lower-case
/// `OWNER/REPO#N`).
pub fn unproved(
    plan: &Value,
    detail: &Detail,
    epic: &Epic,
    priorities: &BTreeMap<String, String>,
    posted: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let Some(actions) = plan["actions"].as_array() else {
        return vec!["the plan's actions could not be read".into()];
    };
    let member = |issue: &str| {
        (epic.sub_issues.iter().chain(&epic.closed)).any(|i| key(i) == issue.to_ascii_lowercase())
    };
    let closed = |issue: &str| {
        epic.closed
            .iter()
            .any(|i| key(i) == issue.to_ascii_lowercase())
    };
    let confirmed = |n: usize| detail.actions.iter().find(|c| c.action == n);
    let mut out = Vec::new();
    for (index, action) in actions.iter().enumerate() {
        let n = index + 1;
        let kind = action["type"].as_str().unwrap_or("?");
        let c = confirmed(n);
        let whole = c.is_some_and(|c| {
            c.wrote("satisfied")
                || needs(kind, action.get("plan").is_some())
                    .iter()
                    .all(|w| c.wrote(w))
        });
        let issue = action["issue"].as_str();
        let missing = match kind {
            "create_sub_issue" => match c.and_then(|c| c.issue.as_deref()) {
                None => Some(format!(
                    "no confirmed issue for its sub-issue in {}",
                    action["repo"].as_str().unwrap_or("?")
                )),
                Some(made) if !member(made) => {
                    Some(format!("{made} is not a sub-issue of the epic"))
                }
                // Cut short after its create, what followed is observed: it is
                // linked, being a sub-issue; it has a Priority on the board;
                // and its job plan, where it has one, is posted.
                Some(made)
                    if !whole
                        && !(priorities.contains_key(&made.to_ascii_lowercase())
                            && (action.get("plan").is_none() || posted(made))) =>
                {
                    Some(format!("{made}'s writes were not all confirmed"))
                }
                Some(_) => None,
            },
            "close_sub_issue" => (!whole && !issue.is_some_and(closed))
                .then(|| format!("{} is open", issue.unwrap_or("?"))),
            "rescope_sub_issue" => {
                (!whole).then(|| format!("{}'s new body was not confirmed", issue.unwrap_or("?")))
            }
            "set_priority" => {
                let target = match action["new"].as_u64() {
                    Some(k) => confirmed(k as usize).and_then(|c| c.issue.clone()),
                    None => issue.map(str::to_owned),
                };
                let value = action["value"].as_str();
                let shown = target
                    .as_deref()
                    .and_then(|t| priorities.get(&t.to_ascii_lowercase()))
                    .map(String::as_str);
                (!whole && (value.is_none() || shown != value)).then(|| {
                    format!(
                        "{} is not {}",
                        target.as_deref().unwrap_or("its new sub-issue"),
                        value.unwrap_or("?")
                    )
                })
            }
            "close_epic" => (!whole && epic.open).then(|| "the epic is open".to_owned()),
            other => Some(format!("{other} is not an action this apply knows")),
        };
        if let Some(why) = missing {
            out.push(format!("action {n} ({kind}): {why}"));
        }
    }
    out
}

/// Rewrites the bot's own receipt `receipt` as recovered: every action's
/// effect proved ([`unproved`]), with the epic's fingerprint now as its
/// baseline. One write.
pub fn record_recovery<R: Rest>(
    client: &R,
    epic: &Epic,
    receipt: &Receipt,
    plan_url: &str,
    now: OffsetDateTime,
) -> Result<()> {
    let mut detail = receipt.detail.clone();
    detail.outcome = Outcome::Recovered;
    detail.fingerprint = Some(fingerprint_pairs(epic));
    let body = receipt_body(&detail, plan_url, &epic.issue, &now.format(&Rfc3339)?)?;
    send(
        client,
        "PATCH",
        &format!(
            "{API_ROOT}repos/{}/issues/comments/{}",
            epic.issue.repo, receipt.id
        ),
        &json!({"body": body}),
        0,
    )
    .map(drop)
}

fn accepted(comments: &[Comment], config: &Config) -> Result<(Value, Comment)> {
    let from = newest_fenced(comments, &config.writers(), EPIC_PLAN_FENCE)?;
    let value = fenced_json(&from.body, EPIC_PLAN_FENCE)?.context("epic plan fence vanished")?;
    Ok((value, from.clone()))
}

/// The parent is an epic: labelled, and by the operator where configured.
/// `still` words the re-check after claiming.
fn designated<T: Transport>(
    client: &T,
    issue: &ItemRef,
    epic: &Epic,
    config: &Config,
    still: bool,
) -> Result<()> {
    let has = if still {
        "no longer has"
    } else {
        "does not have"
    };
    ensure!(epic.designated, "parent {has} the epic label");
    if config.label_by_operator {
        let by = epic_labelled_by(client, issue)?;
        ensure!(
            by.as_deref()
                .is_some_and(|login| login.eq_ignore_ascii_case(&config.operator)),
            "parent's epic label was added by {}, not the operator",
            by.map_or("an app or a deleted account".into(), |login| format!(
                "{login:?}"
            ))
        );
    }
    Ok(())
}

/// The Priority each sub-issue the plan creates gets, by its action's
/// position: a later `set_priority` of it, else the configured default.
/// It is set once, with the create, not the default and then a second change.
fn created_priorities(plan: &EpicPlan, config: &Config) -> BTreeMap<usize, String> {
    let mut out = BTreeMap::new();
    for (index, action) in plan.actions.iter().enumerate() {
        if let Action::Create { .. } = action {
            out.insert(index + 1, config.default_priority.clone());
        }
    }
    for action in &plan.actions {
        if let Action::SetPriority {
            target: Target::New(k),
            value,
        } = action
        {
            out.insert(*k, value.clone());
        }
    }
    out
}

/// The newest accepted plan, checked: its JSON, its comment, the epic now
/// and the validated plan. Writes nothing.
fn checked<T: Transport>(
    client: &T,
    issue: &ItemRef,
    config: &Config,
    expected: &Preconditions,
) -> Result<(Value, Comment, Epic, EpicPlan)> {
    let comments = issue_comments(client, issue)?;
    let (value, from) = accepted(&comments, config).map_err(|e| e.context(Refused))?;
    expected.check(&value, &from)?;
    let epic = read_epic(client, issue)?;
    let plan = (config.validate(&value, &epic)).map_err(|e| e.context(Refused))?;
    designated(client, issue, &epic, config, false)?;
    Ok((value, from, epic, plan))
}

/// Read-only preview. Apply checks for an existing claim before validating
/// current sub-issues: a prior rescope's target may have closed since, which
/// makes its own plan invalid. A prior close is satisfied, so only the claim
/// stops that plan's replay.
pub fn preview<T: Transport>(client: &T, issue: &ItemRef, config: &Config) -> Result<Vec<Value>> {
    preview_checked(client, issue, config, &Preconditions::default())
}

pub fn preview_checked<T: Transport>(
    client: &T,
    issue: &ItemRef,
    config: &Config,
    expected: &Preconditions,
) -> Result<Vec<Value>> {
    let (_, _, epic, plan) = checked(client, issue, config, expected)?;
    let priorities = created_priorities(&plan, config);
    let mut out = Vec::new();
    for mut mutation in plan.preview(&epic) {
        if mutation["graphql"] == "updateProjectV2ItemFieldValue" {
            // A new sub-issue's Priority is set as it is created.
            if mutation["issue"]
                .as_str()
                .is_some_and(|i| i.contains("#NEW-"))
            {
                continue;
            }
            mutation["field"] = config.priority_field.clone().into();
            out.push(json!({"action": mutation["action"], "graphql": "addProjectV2ItemById", "project": config.project, "issue": mutation["issue"]}));
        }
        let link = mutation["graphql"] == "addSubIssue";
        let n = mutation["action"].clone();
        out.push(mutation);
        if link {
            let value = (n.as_u64())
                .and_then(|n| priorities.get(&(n as usize)))
                .unwrap_or(&config.default_priority);
            out.push(json!({"action": n, "graphql": "addProjectV2ItemById", "project": config.project, "issue": format!("NEW-{n}")}));
            out.push(json!({"action": n, "graphql": "updateProjectV2ItemFieldValue", "project": config.project, "field": config.priority_field, "value": value, "issue": format!("NEW-{n}")}));
        }
    }
    Ok(out)
}

/// What applying the newest accepted plan would do, read but not written.
pub struct Planned {
    /// Its plan id ([`plan_id`]) and comment URL.
    pub id: String,
    pub url: String,
    /// One line per action that writes, as `Create a sub-issue in R (P0)`.
    pub lines: Vec<String>,
    /// Its write steps: the receipt, and each action that writes.
    pub writes: usize,
    /// Its actions, for a bound on the requests it may take.
    pub actions: usize,
}

/// [`preview_checked`] in words, for `agent-board pass`.
pub fn planned<T: Transport>(
    client: &T,
    issue: &ItemRef,
    config: &Config,
    expected: &Preconditions,
) -> Result<Planned> {
    let (_, from, _, plan) = checked(client, issue, config, expected)?;
    let priorities = created_priorities(&plan, config);
    let mut lines = Vec::new();
    for (index, action) in plan.actions.iter().enumerate() {
        lines.push(match action {
            Action::Create { repo, .. } => {
                let p = priorities
                    .get(&(index + 1))
                    .unwrap_or(&config.default_priority);
                format!("Create a sub-issue in {repo} ({p})")
            }
            Action::Close { issue, .. } => format!("Close {issue} as not planned"),
            Action::AlreadyClosed { issue } => format!("{issue} is already closed"),
            Action::Rescope { issue, .. } => format!("Rescope {issue}"),
            Action::SetPriority {
                target: Target::Issue(issue),
                value,
            } => format!("Set {issue} to {value}"),
            Action::SetPriority {
                target: Target::New(_),
                ..
            } => continue,
            Action::CloseEpic { .. } => "Close the epic".into(),
        });
    }
    let silent = |a: &Action| {
        matches!(
            a,
            Action::AlreadyClosed { .. }
                | Action::SetPriority {
                    target: Target::New(_),
                    ..
                }
        )
    };
    Ok(Planned {
        id: plan_id(&from)?,
        url: from.url.clone(),
        lines,
        writes: 1 + plan.actions.iter().filter(|a| !silent(a)).count(),
        actions: plan.actions.len(),
    })
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

/// [`apply`] against `expected`. The receipt is one comment on the epic:
/// the claim, posted before any write, updated as each new sub-issue is
/// created, and at the end with every confirmed write, the outcome and the
/// epic's fingerprint. Returns a `Request:` line where the plan has one,
/// the receipt's lines and a `Receipt:` line.
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
    if !claims(&comments, config, &id)?.is_empty() {
        return Ok(request_output()
            .into_iter()
            .chain([ALREADY_CLAIMED.into()])
            .collect());
    }
    let epic = read_epic(client, issue)?;
    designated(client, issue, &epic, config, false)?;
    config.validate(&value, &epic)?;
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
    let dated = now.format(&Rfc3339)?;
    let claim = receipt_body(
        &Detail::new(&id, Outcome::Claimed),
        &from.url,
        &epic.issue,
        &dated,
    )?;
    let (_, posted) = send(client, "POST", &path, &json!({"body": claim}), 0)?;
    let mine = posted["id"]
        .as_u64()
        .context("apply receipt has no id; no mutations attempted")?;
    let receipt_url = posted["html_url"].as_str().map(str::to_owned);
    let edit = format!(
        "{API_ROOT}repos/{}/issues/comments/{mine}",
        issue.repository
    );
    let mut detail = Detail::new(&id, Outcome::Claimed);
    let result = (|| -> Result<()> {
        let again = issue_comments(client, issue)?;
        let (current, comment) = accepted(&again, config)?;
        expected.check(&current, &comment)?;
        if plan_id(&comment)? != id || current != value {
            return Err(PlanChanged.into());
        }
        ensure!(
            !claims(&again, config, &id)?.iter().any(|n| *n < mine),
            "an earlier apply claim won; no mutations attempted"
        );
        // Recheck membership and all refusals after claiming, before any write.
        let epic = read_epic(client, issue)?;
        designated(client, issue, &epic, config, true)?;
        let checked = config.validate(&current, &epic)?;
        // Each new sub-issue is on the receipt before anything more is
        // written, so a pass cut short still names what it created.
        let mut progress = |actions: &[Confirmed]| -> Result<()> {
            let detail = Detail {
                actions: actions.to_vec(),
                ..Detail::new(&id, Outcome::Claimed)
            };
            let body = receipt_body(&detail, &from.url, &epic.issue, &dated)?;
            send(client, "PATCH", &edit, &json!({"body": body}), 0).map(drop)
        };
        execute(
            client,
            &checked,
            &epic,
            config,
            &mut detail.actions,
            &mut progress,
        )
    })();
    detail.outcome = match &result {
        Ok(()) => Outcome::Applied,
        Err(_) => Outcome::Stopped,
    };
    // Never copy API error text into a bot-authored comment: it could forge a
    // plan fence or marker under this login.
    let final_epic = read_epic(client, issue);
    detail.fingerprint = final_epic.as_ref().ok().map(fingerprint_pairs);
    let lines = readable(&detail, None);
    let confirmed = lines.join("\n");
    let body = receipt_body(&detail, &from.url, &epic.issue, &dated)?;
    send(client, "PATCH", &edit, &json!({"body": body}), 0).with_context(|| {
        format!("could not update the apply receipt; its claim remains, do not replay. Confirmed writes:\n{confirmed}")
    })?;
    final_epic.with_context(|| {
        format!("could not read the epic after applying; its receipt has no fingerprint. Confirmed writes:\n{confirmed}")
    })?;
    result.with_context(|| {
        format!(
            "epic apply stopped; confirmed writes:\n{confirmed}\nClaim retained; do not replay."
        )
    })?;
    Ok(request_output()
        .into_iter()
        .chain(lines)
        .chain(receipt_url.map(|url| format!("Receipt: {url}")))
        .collect())
}

/// What [`apply`] says when the plan has a receipt already.
pub const ALREADY_CLAIMED: &str = "Already claimed; nothing changed.";

/// The type a plan action has in its JSON.
fn kind(action: &Action) -> &'static str {
    match action {
        Action::Create { .. } => "create_sub_issue",
        Action::Close { .. } | Action::AlreadyClosed { .. } => "close_sub_issue",
        Action::Rescope { .. } => "rescope_sub_issue",
        Action::SetPriority { .. } => "set_priority",
        Action::CloseEpic { .. } => "close_epic",
    }
}

/// The issue `repos/OWNER/REPO/issues/N[/comments]` names, if it is one.
fn rest_issue(path: &str) -> Option<IssueRef> {
    let (repo, number) = path.strip_prefix("repos/")?.split_once("/issues/")?;
    let number = number.strip_suffix("/comments").unwrap_or(number);
    IssueRef::parse(&format!("{repo}#{number}")).ok()
}

fn execute<C: Transport + Rest>(
    client: &C,
    plan: &EpicPlan,
    epic: &Epic,
    config: &Config,
    confirmed: &mut Vec<Confirmed>,
    progress: &mut dyn FnMut(&[Confirmed]) -> Result<()>,
) -> Result<()> {
    let priorities = created_priorities(plan, config);
    let mut created: BTreeMap<u64, IssueRef> = BTreeMap::new();
    for mutation in plan.mutations(epic) {
        let n = mutation["action"]
            .as_u64()
            .context("mutation has no action")?;
        let index = n as usize - 1;
        let action = plan.actions.get(index).context("mutation's action")?;
        if !confirmed.last().is_some_and(|c| c.action == index + 1) {
            confirmed.push(Confirmed {
                action: index + 1,
                kind: kind(action).into(),
                ..Confirmed::default()
            });
        }
        let at = confirmed.len() - 1;
        let record = |confirmed: &mut Vec<Confirmed>, write: &str, issue: Option<String>| {
            let c = &mut confirmed[at];
            c.writes.push(write.into());
            if c.issue.is_none() {
                c.issue = issue;
            }
        };
        if mutation["satisfied"].is_string() {
            if let Action::AlreadyClosed { issue } = action {
                record(confirmed, "satisfied", Some(issue.to_string()));
            }
        } else if let Some(method) = mutation["method"].as_str() {
            let mut url = mutation["url"]
                .as_str()
                .context("mutation has no URL")?
                .to_owned();
            let mut body = mutation["body"].clone();
            for (k, issue) in &created {
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
                record(confirmed, "create", Some(issue.to_string()));
                progress(confirmed)?;
            } else {
                let write = if method == "POST" { "comment" } else { "patch" };
                let issue = url.strip_prefix(API_ROOT).and_then(rest_issue);
                record(confirmed, write, issue.map(|i| i.to_string()));
            }
        } else if mutation["graphql"] == "addSubIssue" {
            let sub = created.get(&n).context("sub-issue was not created")?;
            client.graphql("mutation($issueId: ID!, $subIssueUrl: String!) { addSubIssue(input: {issueId: $issueId, subIssueUrl: $subIssueUrl}) { issue { id } } }", json!({"issueId": epic.id, "subIssueUrl": format!("https://github.com/{}/issues/{}", sub.repo, sub.number)}))?;
            record(confirmed, "link", None);
            let value = priorities
                .get(&(n as usize))
                .unwrap_or(&config.default_priority);
            set_priority(client, sub, value, config)?;
            record(confirmed, "priority", None);
            confirmed[at].priority = Some(value.clone());
        } else {
            let target = mutation["issue"].as_str().context("priority target")?;
            let value = mutation["value"].as_str().context("priority value")?;
            let (issue, write) = if let Some((_, k)) = target.rsplit_once("#NEW-") {
                // Set with the create, which wrote this very value.
                let made = created
                    .get(&k.parse::<u64>()?)
                    .context("priority target was not created")?;
                (made.clone(), "folded")
            } else {
                let issue = IssueRef::parse(target)?;
                set_priority(client, &issue, value, config)?;
                (issue, "priority")
            };
            record(confirmed, write, Some(issue.to_string()));
            confirmed[at].priority = Some(value.into());
        }
    }
    Ok(())
}

/// Puts `issue` on the project, where it may be already, and sets its
/// Priority: one write step.
fn set_priority<T: Transport>(
    client: &T,
    issue: &IssueRef,
    value: &str,
    config: &Config,
) -> Result<()> {
    let (owner, name) = issue.repo.split_once('/').context("priority repository")?;
    let data = client.graphql("query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { issue(number: $number) { id } } }", json!({"owner": owner, "name": name, "number": issue.number}))?;
    let content = data
        .pointer("/repository/issue/id")
        .and_then(Value::as_str)
        .context("priority target is not an issue")?;
    let data = client.graphql("mutation($project: ID!, $content: ID!) { addProjectV2ItemById(input: {projectId: $project, contentId: $content}) { item { id } } }", json!({"project": config.project, "content": content}))?;
    let item = data
        .pointer("/addProjectV2ItemById/item/id")
        .and_then(Value::as_str)
        .context("project membership has no id")?;
    let option = config
        .priorities
        .get(value)
        .context("unconfigured Priority")?;
    client.graphql("mutation($project: ID!, $item: ID!, $field: ID!, $option: String!) { updateProjectV2ItemFieldValue(input: {projectId: $project, itemId: $item, fieldId: $field, value: {singleSelectOptionId: $option}}) { projectV2Item { id } } }", json!({"project": config.project, "item": item, "field": config.priority_field_id, "option": option}))?;
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
        authored(id, body, editor, "planner")
    }

    fn authored(id: u64, body: String, editor: Option<&str>, author: &str) -> Value {
        json!({"url": format!("https://github.com/example/epics/issues/1#issuecomment-{id}"), "body": body,
            "createdAt": "2026-10-14T10:00:00Z", "lastEditedAt": editor.map(|_| "2026-10-14T11:00:00Z"),
            "author": {"login": author}, "editor": editor.map(|login| json!({"login": login}))})
    }

    /// A labelings timeline, oldest first: (actor type, login, label).
    fn labellings(events: &[(&str, &str, &str)], earlier: bool) -> Value {
        let nodes: Vec<_> = (events.iter())
            .map(|(kind, login, label)| json!({"actor": {"__typename": kind, "login": login}, "label": {"name": label}}))
            .collect();
        json!({"nodes": nodes, "pageInfo": {"hasPreviousPage": earlier}})
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
        /// The epic's labelings timeline, and the one from the second read on.
        labellings: Value,
        relabelled: Option<Value>,
        timeline_reads: Cell<usize>,
        /// The epic is closed from this epic read on.
        epic_closed_from: Option<usize>,
        /// Closed by a plan's close_epic.
        epic_closed: Cell<bool>,
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
                labellings: labellings(&[("User", "operator", "epic")], false),
                relabelled: None,
                timeline_reads: Cell::new(0),
                epic_closed_from: None,
                epic_closed: Cell::new(false),
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
            if query.contains("timelineItems(itemTypes: [LABELED_EVENT]") {
                self.timeline_reads.set(self.timeline_reads.get() + 1);
                let timeline = match &self.relabelled {
                    Some(later) if self.timeline_reads.get() >= 2 => later,
                    _ => &self.labellings,
                };
                return Ok(json!({"repository": {"issue": {"timelineItems": timeline}}}));
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
                let reads = self.epic_reads.get();
                let closed = self.epic_closed.get()
                    || self.epic_closed_from.is_some_and(|from| reads >= from);
                let state = if closed { "CLOSED" } else { "OPEN" };
                return Ok(
                    json!({"repository": {"issue": {"id": "E", "state": state, "labels": {"nodes": if labelled { vec![json!({"name": "epic"})] } else { vec![] }}, "subIssues": {"nodes": nodes, "pageInfo": {"hasNextPage": false}}}}}),
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
            // The receipt's last edit, after the final epic read.
            if method == "PATCH"
                && path == "repos/example/epics/issues/comments/2"
                && self.fail == Some("receipt")
                && self.epic_reads.get() >= 3
            {
                anyhow::bail!("injected receipt failure");
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
                self.comments.borrow_mut().push(authored(
                    id,
                    body["body"].as_str().unwrap().into(),
                    None,
                    self.login,
                ));
                return Ok((201, json!({"id": id})));
            }
            if path == "repos/example/epics/issues/comments/2" {
                self.comments.borrow_mut()[1]["body"] = body["body"].clone();
            }
            if path == "repos/example/work/issues/3" {
                self.closed.set(true);
            }
            if path == "repos/example/epics/issues/1" {
                self.epic_closed.set(true);
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
                    assert_eq!(receipt(&forge).detail.plan, "1@2026-10-14T10:00:00Z");
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

    /// The bot's receipt on the epic, its second comment.
    fn receipt(forge: &Forge) -> Receipt {
        let comments = forge.comments.borrow();
        let comments: Vec<Comment> = (comments.iter())
            .map(|c| crate::plan::comment(c).unwrap())
            .collect();
        let (mut found, _) = receipts(&comments, &["operator", "planner", "applier"]);
        assert_eq!(found.len(), 1, "{comments:?}");
        found.remove(0)
    }

    #[test]
    fn applies_once_with_configured_ids_and_real_job_plan_number() {
        let forge = Forge::full();
        let changes = apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH).unwrap();
        assert_eq!(
            changes,
            [
                "Created example/work#10 (High)",
                "Rescoped example/work#2",
                "Closed example/work#3 as not planned"
            ]
        );
        let requests = forge.requests.borrow().clone();
        let targets: Vec<_> = requests
            .iter()
            .map(|(method, target, _)| format!("{method} {target}"))
            .collect();
        // The new sub-issue gets its final Priority in one write, and is on
        // the receipt before it is linked.
        assert_eq!(
            targets,
            [
                "POST repos/example/epics/issues/1/comments",
                "POST repos/example/work/issues",
                "PATCH repos/example/epics/issues/comments/2",
                "graphql link",
                "graphql enroll",
                "graphql priority",
                "POST repos/example/work/issues/10/comments",
                "PATCH repos/example/work/issues/2",
                "POST repos/example/work/issues/2/comments",
                "POST repos/example/work/issues/3/comments",
                "PATCH repos/example/work/issues/3",
                "PATCH repos/example/epics/issues/comments/2"
            ]
        );
        assert_eq!(requests[5].2["option"], "O_high");
        let job = crate::plan::JobPlan::from_comment(requests[6].2["body"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!((job.repo.as_str(), job.item), ("example/work", 10));
        assert_eq!(requests[7].2, json!({"body": "new scope"}));
        assert_eq!(
            requests[10].2,
            json!({"state": "closed", "state_reason": "not_planned"})
        );
        let progress = requests[2].2["body"].as_str().unwrap();
        assert!(
            progress.starts_with(
                "<!-- agent-board-epic-apply: 1@2026-10-14T10:00:00Z -->\nApplying [this epic plan]("
            ),
            "{progress}"
        );
        assert!(
            progress.contains("\"issue\": \"example/work#10\""),
            "{progress}"
        );
        let body = forge.comments.borrow()[1]["body"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            body.starts_with(
                "<!-- agent-board-epic-apply: 1@2026-10-14T10:00:00Z -->\n\
                 Applied [this epic plan](https://github.com/example/epics/issues/1#issuecomment-1) at 1970-01-01T00:00:00Z.\n\n\
                 - Created [work#10](https://github.com/example/work/issues/10) (High)\n\
                 - Rescoped [work#2](https://github.com/example/work/issues/2)\n\
                 - Closed [work#3](https://github.com/example/work/issues/3) as not planned\n\n\
                 ```agent-board-epic-apply\n{"
            ),
            "{body}"
        );
        let receipt = receipt(&forge);
        assert_eq!(receipt.detail.outcome, Outcome::Applied);
        assert!(
            (receipt.detail.fingerprint.as_ref().unwrap())
                .contains(&("example/work#3".into(), "CLOSED".into()))
        );
        assert_eq!(
            receipt.detail.priorities(),
            BTreeMap::from([("example/work#10".into(), "High".into())])
        );
        assert_eq!(
            apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH).unwrap(),
            [ALREADY_CLAIMED]
        );
        assert_eq!(forge.requests.borrow().len(), requests.len());
    }

    /// tracker#596: the sub-issue a plan closes closed before apply. The rest
    /// applies, nothing touches the closed one, and the claim ends the retries.
    #[test]
    fn close_of_an_already_closed_sub_issue_applies_the_rest() {
        let forge = Forge::full();
        forge.closed.set(true);
        let lines = apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH).unwrap();
        assert_eq!(lines[2], "example/work#3 was already closed");
        let targets: Vec<_> = (forge.requests.borrow().iter())
            .map(|(_, target, _)| target.clone())
            .collect();
        // As applies_once_with_configured_ids_and_real_job_plan_number, less
        // the close's comment and PATCH.
        assert_eq!(
            targets,
            [
                "repos/example/epics/issues/1/comments",
                "repos/example/work/issues",
                "repos/example/epics/issues/comments/2",
                "link",
                "enroll",
                "priority",
                "repos/example/work/issues/10/comments",
                "repos/example/work/issues/2",
                "repos/example/work/issues/2/comments",
                "repos/example/epics/issues/comments/2"
            ]
        );
        let receipt = receipt(&forge);
        assert_eq!(receipt.detail.outcome, Outcome::Applied);
        assert_eq!(receipt.detail.actions[3].writes, ["satisfied"]);
        assert_eq!(
            apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH).unwrap(),
            [ALREADY_CLAIMED]
        );
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
        assert!(
            error.contains("Created example/work#10, then stopped before"),
            "{error}"
        );
        assert!(!forge.closed.get());
        let receipt = receipt(&forge);
        assert_eq!(receipt.detail.outcome, Outcome::Stopped);
        assert_eq!(receipt.detail.actions[0].writes, ["create", "link"]);
        assert_eq!(
            receipt.detail.actions[0].issue.as_deref(),
            Some("example/work#10")
        );
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
                ["Request: Request-123", ALREADY_CLAIMED]
            );
        }
    }

    #[test]
    fn preview_is_read_only_and_config_is_strict() {
        let forge = Forge::full();
        let preview = preview(&forge, &issue(), &config()).unwrap();
        assert!(forge.requests.borrow().is_empty());
        // The new sub-issue's Priority is the plan's, set with its create.
        let priorities: Vec<_> = (preview.iter())
            .filter(|m| m["graphql"] == "updateProjectV2ItemFieldValue")
            .map(|m| (m["field"].as_str().unwrap(), m["value"].as_str().unwrap()))
            .collect();
        assert_eq!(priorities, [("Order", "High")]);
        for value in [json!({}), json!({"unknown": true})] {
            assert!(Config::from_json(&value).is_err());
        }
        let mut epic = read_epic(&forge, &issue()).unwrap();
        let first = fingerprint_pairs(&epic);
        epic.sub_issues.reverse();
        assert_eq!(fingerprint_pairs(&epic), first);
        epic.closed.push(epic.sub_issues.pop().unwrap());
        assert_ne!(fingerprint_pairs(&epic), first);
    }

    #[test]
    fn a_separate_planner_writes_plans_and_the_bot_claims() {
        let separate = |planner: Value| {
            let mut value = json!({"operator": "operator", "bot": "applier", "repositories": ["example/work"],
                "project": "P_config", "priority_field": "Order", "priority_field_id": "F_config",
                "priorities": {"High": "O_high", "Normal": "O_normal"}, "default_priority": "Normal"});
            value["planner"] = planner;
            Config::from_json(&value)
        };
        for bad in [json!(""), json!("not a login!"), json!(null), json!(7)] {
            assert!(separate(bad).is_err());
        }
        // Without a planner the bot writes plans, as before.
        assert_eq!(config().writers()[1], config().bot);
        let config = separate(json!("planner")).unwrap();
        assert_eq!(config.writers(), ["operator", "planner"]);

        let mut forge = Forge::new(json!([
            {"type": "set_priority", "issue": "example/work#2", "value": "High"}
        ]));
        forge.login = "applier";
        // A newer fenced comment from the bot is not a plan once a planner is named.
        let decoy = json!({"schema": EPIC_PLAN_SCHEMA, "epic": "example/epics#1", "actions": [
            {"type": "close_sub_issue", "issue": "example/work#3", "reason": "decoy"}]});
        forge.comments.borrow_mut().push(authored(
            2,
            format!("```{EPIC_PLAN_FENCE}\n{decoy}\n```"),
            None,
            "applier",
        ));
        let mutations = preview(&forge, &issue(), &config).unwrap();
        assert!(
            mutations
                .iter()
                .all(|m| !m.to_string().contains("issues/3"))
        );
        assert!(mutations.iter().any(|m| m["value"] == "High"));
        let now = OffsetDateTime::UNIX_EPOCH;
        let fresh = forge.comments.borrow().clone();
        apply(&forge, &issue(), &config, now).unwrap();
        assert!(!forge.closed.get());
        // A claim the planner posts does not stop the bot's apply.
        let claim = forge.comments.borrow()[2]["body"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut planted = Forge::new(json!([
            {"type": "set_priority", "issue": "example/work#2", "value": "High"}
        ]));
        planted.login = "applier";
        *planted.comments.borrow_mut() = fresh;
        planted
            .comments
            .borrow_mut()
            .push(authored(3, claim, None, "planner"));
        assert_ne!(
            apply(&planted, &issue(), &config, now).unwrap(),
            [ALREADY_CLAIMED]
        );
        // The bot's claim, not the planner's, stops the replay.
        let writes = forge.requests.borrow().len();
        assert_eq!(
            apply(&forge, &issue(), &config, now).unwrap(),
            [ALREADY_CLAIMED]
        );
        assert_eq!(forge.requests.borrow().len(), writes);
        // The planner's token may not apply.
        forge.login = "planner";
        assert!(apply(&forge, &issue(), &config, now).is_err());
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
        execute(&forge, &plan, &epic, &config, &mut Vec::new(), &mut |_| {
            Ok(())
        })
        .unwrap();
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
    fn only_designated_epics_may_be_decomposed() {
        for before in [true, false] {
            let mut forge = Forge::full();
            forge.labelled = !before;
            forge.remove_label_on_recheck = !before;
            let error = format!(
                "{:#}",
                apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH).unwrap_err()
            );
            assert!(error.contains("the epic label"), "{error}");
            assert!(!forge.created.get());
            if before {
                assert!(forge.requests.borrow().is_empty());
                assert!(preview(&forge, &issue(), &config()).is_err());
            }
        }
    }

    /// With `label_by_operator`, the newest epic labeling must be a user's
    /// whose login is the operator's, before apply and after its claim. Off,
    /// the timeline is never read.
    #[test]
    fn label_by_operator_requires_the_operators_newest_labeling() {
        let operator = ("User", "Operator", "epic");
        let intruder = ("User", "intruder", "epic");
        let other = ("User", "operator", "bug");
        // Configured, the timeline, a later one, and the error (None: applies).
        for (on, timeline, later, expected) in [
            (false, labellings(&[intruder], false), None, None),
            (true, labellings(&[operator], false), None, None),
            (
                true,
                labellings(&[intruder, operator, other], true),
                None,
                None,
            ),
            (
                true,
                labellings(&[("User", "operator", "Epic")], false),
                None,
                None,
            ),
            (
                true,
                labellings(&[intruder], false),
                None,
                Some("added by \"intruder\""),
            ),
            (
                true,
                labellings(&[operator, intruder], false),
                None,
                Some("added by \"intruder\""),
            ),
            (
                true,
                labellings(&[("Bot", "operator", "epic")], false),
                None,
                Some("an app or a deleted account"),
            ),
            (
                true,
                labellings(&[other], true),
                None,
                Some("more labelings than one page holds"),
            ),
            (
                true,
                labellings(&[other], false),
                None,
                Some("no \"epic\" labeling"),
            ),
            (
                true,
                json!({"nodes": null}),
                None,
                Some("timeline has no nodes"),
            ),
            (
                true,
                json!({"nodes": [{"actor": null, "label": {"name": "epic"}}], "pageInfo": {"hasPreviousPage": false}}),
                None,
                Some("an app or a deleted account"),
            ),
            (
                true,
                labellings(&[("Mannequin", "operator", "epic")], false),
                None,
                Some("an app or a deleted account"),
            ),
            // A labeling whose label was since deleted is not an epic one.
            (
                true,
                json!({"nodes": [{"actor": {"__typename": "User", "login": "operator"}, "label": null}], "pageInfo": {"hasPreviousPage": false}}),
                None,
                Some("no \"epic\" labeling"),
            ),
            (
                true,
                json!({"nodes": []}),
                None,
                Some("or GitHub did not say"),
            ),
            (
                true,
                labellings(&[operator], false),
                Some(labellings(&[operator, intruder], false)),
                Some("added by \"intruder\""),
            ),
        ] {
            let mut forge = Forge::full();
            forge.labellings = timeline;
            let after_claim = later.is_some();
            forge.relabelled = later;
            let mut config = config();
            config.label_by_operator = on;
            let result = apply(&forge, &issue(), &config, OffsetDateTime::UNIX_EPOCH);
            if !on {
                assert_eq!(forge.timeline_reads.get(), 0);
            }
            match expected {
                None => {
                    result.unwrap();
                    assert!(forge.created.get());
                }
                Some(expected) => {
                    let error = format!("{:#}", result.unwrap_err());
                    assert!(error.contains(expected), "{error}");
                    assert!(!forge.created.get() && !forge.closed.get());
                    let writes = forge.requests.borrow();
                    if after_claim {
                        assert!(
                            (writes.iter())
                                .all(|(_, path, _)| path.starts_with("repos/example/epics/")),
                            "{writes:?}"
                        );
                    } else {
                        assert!(writes.is_empty(), "{writes:?}");
                        drop(writes);
                        let refused = preview(&forge, &issue(), &config).unwrap_err();
                        assert!(format!("{refused:#}").contains(expected));
                    }
                }
            }
        }
        let on = |value: Value, bot: &str| {
            let mut object = json!({"operator": "operator", "bot": bot, "repositories": [],
                "project": "P", "priority_field": "Order", "priority_field_id": "F",
                "priorities": {"Normal": "O"}, "default_priority": "Normal"});
            if !value.is_null() {
                object["label_by_operator"] = value;
            }
            Config::from_json(&object).map(|c| c.label_by_operator).ok()
        };
        assert_eq!(on(Value::Null, "planner"), Some(false));
        assert_eq!(on(json!(true), "planner"), Some(true));
        assert_eq!(on(json!(false), "planner"), Some(false));
        assert_eq!(on(json!("true"), "planner"), None);
        assert_eq!(on(json!(true), "Operator"), None);
        assert_eq!(on(json!(false), "Operator"), Some(false));
    }

    /// Closing the epic ends its decomposition: a plan written while it was
    /// open makes no write, whether it closed before apply or after the claim.
    #[test]
    fn a_closed_epic_is_not_decomposed() {
        for before in [true, false] {
            let mut forge = Forge::full();
            forge.epic_closed_from = Some(if before { 1 } else { 2 });
            let error = format!(
                "{:#}",
                apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH).unwrap_err()
            );
            assert!(error.contains("epic is closed"), "{error}");
            assert!(!forge.created.get());
            let writes = forge.requests.borrow();
            assert!(
                writes
                    .iter()
                    .all(|(_, path, _)| path.starts_with("repos/example/epics/")),
                "{writes:?}"
            );
            if before {
                assert!(writes.is_empty());
                let refused = preview(&forge, &issue(), &config()).unwrap_err();
                assert!(format!("{refused:#}").contains("epic is closed"));
            }
        }
    }

    /// agent-board#200: a plan closes its epic after its last sub-issues, and
    /// only where the configuration allows it; off, nothing is written.
    #[test]
    fn close_epic_applies_only_when_configured() {
        let actions = json!([
            {"type": "close_sub_issue", "issue": "example/work#2", "reason": "done"},
            {"type": "close_sub_issue", "issue": "example/work#3", "reason": "done"},
            {"type": "close_epic", "reason": "Nothing is left."}
        ]);
        let forge = Forge::new(actions.clone());
        let error = format!("{:#}", preview(&forge, &issue(), &config()).unwrap_err());
        assert!(error.contains("does not allow (close_epic)"), "{error}");
        let error = apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH).unwrap_err();
        assert!(format!("{error:#}").contains("does not allow (close_epic)"));
        assert!(forge.requests.borrow().is_empty());
        assert!(!forge.epic_closed.get());

        let mut config = config();
        config.close_epic = true;
        let preview = preview(&forge, &issue(), &config).unwrap();
        assert_eq!(preview.len(), 6, "{preview:?}");
        apply(&forge, &issue(), &config, OffsetDateTime::UNIX_EPOCH).unwrap();
        let requests = forge.requests.borrow().clone();
        let targets: Vec<_> = (requests.iter())
            .map(|(method, target, _)| format!("{method} {target}"))
            .collect();
        assert_eq!(
            targets,
            [
                "POST repos/example/epics/issues/1/comments",
                "POST repos/example/work/issues/2/comments",
                "PATCH repos/example/work/issues/2",
                "POST repos/example/work/issues/3/comments",
                "PATCH repos/example/work/issues/3",
                "POST repos/example/epics/issues/1/comments",
                "PATCH repos/example/epics/issues/1",
                "PATCH repos/example/epics/issues/comments/2"
            ]
        );
        assert_eq!(requests[5].2, json!({"body": "Nothing is left."}));
        assert_eq!(
            requests[6].2,
            json!({"state": "closed", "state_reason": "completed"})
        );
        assert!(forge.epic_closed.get());
        // The claim, not the closed epic's refusal, ends a replay.
        assert_eq!(
            apply(&forge, &issue(), &config, OffsetDateTime::UNIX_EPOCH).unwrap(),
            [ALREADY_CLAIMED]
        );
        let on = |value: Value| {
            let mut object = json!({"operator": "operator", "bot": "planner", "repositories": [],
                "project": "P", "priority_field": "Order", "priority_field_id": "F",
                "priorities": {"Normal": "O"}, "default_priority": "Normal"});
            if !value.is_null() {
                object["close_epic"] = value;
            }
            Config::from_json(&object).map(|c| c.close_epic).ok()
        };
        assert_eq!(on(Value::Null), Some(false));
        assert_eq!(on(json!(true)), Some(true));
        assert_eq!(on(json!(false)), Some(false));
        assert_eq!(on(json!("true")), None);
    }

    #[test]
    fn failure_at_each_stage_stops_action_writes_and_retains_claim() {
        // Where it fails, and how many writes outside the epic were made.
        for (target, action_requests) in [
            ("repos/example/work/issues", 1),
            ("link", 2),
            ("enroll", 3),
            ("priority", 4),
            ("repos/example/work/issues/10/comments", 5),
            ("repos/example/work/issues/2", 6),
            ("repos/example/work/issues/2/comments", 7),
            ("repos/example/work/issues/3/comments", 8),
            ("repos/example/work/issues/3", 9),
            ("final_epic", 9),
            ("receipt", 9),
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
                    || error.contains("injected receipt"),
                "{target}: {error}"
            );
            let actual = forge
                .requests
                .borrow()
                .iter()
                .filter(|(_, path, _)| !path.starts_with("repos/example/epics/"))
                .count();
            assert_eq!(actual, action_requests, "{target}");
            // A claim stays whatever failed: the receipt as last written.
            let receipt = receipt(&forge);
            let outcome = match target {
                "final_epic" => Outcome::Applied,
                "receipt" => Outcome::Claimed,
                _ => Outcome::Stopped,
            };
            assert_eq!(receipt.detail.outcome, outcome, "{target}");
            assert_eq!(
                receipt.detail.fingerprint.is_some(),
                target != "final_epic" && target != "receipt",
                "{target}"
            );
            let requests = forge.requests.borrow().len();
            apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH).unwrap();
            assert_eq!(forge.requests.borrow().len(), requests, "{target}");
        }
    }

    fn bot_comment(id: u64, body: &str, author: &str) -> Comment {
        crate::plan::comment(&json!({
            "url": format!("https://github.com/cgwalters-forge/tracker/issues/596#issuecomment-{id}"),
            "body": body, "createdAt": "2026-10-16T03:33:05Z", "lastEditedAt": null,
            "author": {"login": author}, "editor": null}))
        .unwrap()
    }

    /// Older receipts as tracker has them (shortened): #596's, applied,
    /// with its fingerprint comment after it; #674's, stopped part way; a
    /// bare claim; and a visitor's copy, which is no receipt.
    #[test]
    fn older_receipts_are_read() {
        let applied = "<!-- agent-board-epic-apply: 6207389344@2026-10-16T03:25:59Z -->\n\
            2026-10-16T03:32:32.837124566Z: Applied.\n\
            - Action 1: POST repos/cgwalters-forge/agentic-job/issues/611/comments\n\
            - Action 1: PATCH repos/cgwalters-forge/agentic-job/issues/611\n\
            - Action 2: POST repos/cgwalters-forge/agentic-job/issues\n\
            - Action 2: linked cgwalters-forge/agentic-job#715.\n\
            - Action 2: enrolled cgwalters-forge/agentic-job#715.\n\
            - Action 2: set cgwalters-forge/agentic-job#715 Priority=P2.\n\
            - Action 2: POST repos/cgwalters-forge/agentic-job/issues/715/comments\n\
            - Action 3: enrolled cgwalters-forge/agentic-job#715.\n\
            - Action 3: set cgwalters-forge/agentic-job#715 Priority=P0.\n\
            - Action 4: cgwalters-forge/tracker#607 is already closed; nothing to do.";
        let fingerprint = "<!-- agent-board-epic-fingerprint: [[\"cgwalters-forge/agentic-job#611\",\"CLOSED\"],[\"cgwalters-forge/agentic-job#715\",\"OPEN\"]] -->\n\
            2026-10-16T03:32:32.837124566Z: Applied.";
        let stopped = "<!-- agent-board-epic-apply: 6228083778@2026-10-17T01:42:27Z -->\n\
            2026-10-17T01:50:10.691131826Z: Stopped after a failed request; its outcome may be ambiguous. Inspect before replacing the plan.\n\
            - Action 1: POST repos/cgwalters-forge/agentic-job/issues\n\
            - Action 1: linked cgwalters-forge/agentic-job#1009.\n\
            - Action 1: enrolled cgwalters-forge/agentic-job#1009.\n\
            - Action 1: set cgwalters-forge/agentic-job#1009 Priority=P2.\n\
            - Action 1: POST repos/cgwalters-forge/agentic-job/issues/1009/comments\n\
            - Action 2: POST repos/cgwalters-forge/agentic-job/issues";
        let claim = "<!-- agent-board-epic-apply: 1@2026-10-17T01:42:27Z -->\n\
            Applying https://github.com/cgwalters-forge/tracker/issues/674#issuecomment-1. Do not replay this plan after a failure.";
        let comments = [
            bot_comment(1, applied, "cgwalters-bot"),
            bot_comment(2, fingerprint, "cgwalters-bot"),
            bot_comment(3, stopped, "cgwalters-bot"),
            bot_comment(4, claim, "cgwalters"),
            bot_comment(5, stopped, "visitor"),
        ];
        let (found, _) = receipts(&comments, &["cgwalters", "cgwalters-bot"]);
        let summary: Vec<_> = (found.iter())
            .map(|r| (r.id, r.detail.plan.as_str(), r.detail.outcome))
            .collect();
        assert_eq!(
            summary,
            [
                (1, "6207389344@2026-10-16T03:25:59Z", Outcome::Applied),
                (3, "6228083778@2026-10-17T01:42:27Z", Outcome::Stopped),
                (4, "1@2026-10-17T01:42:27Z", Outcome::Claimed),
            ]
        );
        let applied = &found[0].detail;
        assert_eq!(
            applied.fingerprint.as_deref(),
            Some(
                &[
                    ("cgwalters-forge/agentic-job#611".into(), "CLOSED".into()),
                    ("cgwalters-forge/agentic-job#715".into(), "OPEN".into())
                ][..]
            )
        );
        let confirmed = |c: &Confirmed| {
            (
                c.action,
                c.issue.clone(),
                c.priority.clone(),
                c.writes.join(","),
            )
        };
        assert_eq!(
            applied.actions.iter().map(confirmed).collect::<Vec<_>>(),
            [
                (
                    1,
                    Some("cgwalters-forge/agentic-job#611".into()),
                    None,
                    "comment,patch".into()
                ),
                (
                    2,
                    Some("cgwalters-forge/agentic-job#715".into()),
                    Some("P2".into()),
                    "create,link,priority,comment".into()
                ),
                (
                    3,
                    Some("cgwalters-forge/agentic-job#715".into()),
                    Some("P0".into()),
                    "priority".into()
                ),
                (
                    4,
                    Some("cgwalters-forge/tracker#607".into()),
                    None,
                    "satisfied".into()
                ),
            ]
        );
        // The last Priority set wins.
        assert_eq!(
            applied.priorities()["cgwalters-forge/agentic-job#715"],
            "P0"
        );
        assert_eq!(found[1].detail.fingerprint, None);
        let partial = &found[1].detail.actions;
        assert_eq!(partial[1].writes, ["create"]);
        assert_eq!(partial[1].issue, None);
        assert!(found[2].detail.actions.is_empty());
    }

    /// What a receipt cut short proves, from its confirmed identities and the
    /// epic now, never from titles.
    #[test]
    fn unproved_table() {
        let plan = json!({"actions": [
            {"type": "create_sub_issue", "repo": "example/work", "title": "child", "body": "b",
             "plan": {"kind": "implement"}},
            {"type": "set_priority", "new": 1, "value": "High"},
            {"type": "close_sub_issue", "issue": "example/work#3", "reason": "r"},
            {"type": "rescope_sub_issue", "issue": "example/work#2", "body": "b", "reason": "r"},
            {"type": "set_priority", "issue": "example/work#2", "value": "Low"}
        ]});
        let done = |action: usize, kind: &str, issue: &str, writes: &[&str]| Confirmed {
            action,
            kind: kind.into(),
            issue: Some(issue.into()),
            priority: None,
            writes: writes.iter().map(|w| (*w).to_owned()).collect(),
        };
        let all = [
            done(
                1,
                "create_sub_issue",
                "example/work#10",
                &["create", "link", "priority", "comment"],
            ),
            done(2, "set_priority", "example/work#10", &["priority"]),
            done(
                3,
                "close_sub_issue",
                "example/work#3",
                &["comment", "patch"],
            ),
            done(
                4,
                "rescope_sub_issue",
                "example/work#2",
                &["patch", "comment"],
            ),
            done(5, "set_priority", "example/work#2", &["priority"]),
        ];
        let epic = |open: &[u64], closed: &[u64]| Epic {
            issue: IssueRef::parse("example/epics#1").unwrap(),
            id: "E".into(),
            designated: true,
            open: true,
            sub_issues: (open.iter())
                .map(|n| IssueRef::parse(&format!("example/work#{n}")).unwrap())
                .collect(),
            closed: (closed.iter())
                .map(|n| IssueRef::parse(&format!("example/work#{n}")).unwrap())
                .collect(),
        };
        let shown = BTreeMap::from([("example/work#2".to_owned(), "Low".to_owned())]);
        let none = BTreeMap::new();
        // The confirmed actions kept, the epic's open and closed sub-issues,
        // the board's Priorities, and what is not proved.
        type Case<'a> = (
            &'a [usize],
            &'a [u64],
            &'a [u64],
            &'a BTreeMap<String, String>,
            Vec<&'a str>,
        );
        let cases: Vec<Case> = vec![
            // Retitled, closed or reprioritised since: proved all the same.
            (&[1, 2, 3, 4, 5], &[2], &[3, 10], &none, vec![]),
            (
                &[1, 2, 4],
                &[2, 3, 10],
                &[],
                &shown,
                vec!["action 3 (close_sub_issue): example/work#3 is open"],
            ),
            (&[1, 2, 4], &[2, 10], &[3], &shown, vec![]),
            (
                &[2, 3, 4, 5],
                &[2, 10],
                &[3],
                &none,
                vec![
                    "action 1 (create_sub_issue): no confirmed issue for its sub-issue in example/work",
                ],
            ),
            (
                &[1, 2, 3, 4, 5],
                &[2],
                &[3],
                &none,
                vec!["action 1 (create_sub_issue): example/work#10 is not a sub-issue of the epic"],
            ),
            (
                &[1, 2, 3],
                &[2, 10],
                &[3],
                &none,
                vec![
                    "action 4 (rescope_sub_issue): example/work#2's new body was not confirmed",
                    "action 5 (set_priority): example/work#2 is not Low",
                ],
            ),
        ];
        for (n, (kept, open, closed, priorities, expected)) in cases.into_iter().enumerate() {
            let detail = Detail {
                actions: (all.iter())
                    .filter(|c| kept.contains(&c.action))
                    .cloned()
                    .collect(),
                ..Detail::new("1@2026-10-14T10:00:00Z", Outcome::Claimed)
            };
            let got = unproved(&plan, &detail, &epic(open, closed), priorities, &|_| false);
            assert_eq!(got.len(), expected.len(), "{n}: {got:?}");
            for (got, expected) in got.iter().zip(&expected) {
                assert!(got.starts_with(expected), "{n}: {got}");
            }
        }
        // Cut short just after a create: its link, Priority and job plan
        // are observed rather than confirmed.
        let cut = Detail {
            actions: vec![
                done(1, "create_sub_issue", "example/work#10", &["create"]),
                done(2, "set_priority", "example/work#10", &["folded"]),
            ],
            ..Detail::new("1@2026-10-14T10:00:00Z", Outcome::Claimed)
        };
        let short = json!({"actions": plan["actions"].as_array().unwrap()[..2]});
        let board = BTreeMap::from([("example/work#10".to_owned(), "High".to_owned())]);
        let linked = epic(&[10], &[]);
        for (priorities, posted, proved) in [
            (&board, true, true),
            (&board, false, false),
            (&none, true, false),
        ] {
            let got = unproved(&short, &cut, &linked, priorities, &|_| posted);
            assert_eq!(got.is_empty(), proved, "{got:?}");
        }
        assert!(!unproved(&short, &cut, &epic(&[], &[]), &board, &|_| true).is_empty());
    }

    /// A comment that looks like a receipt and cannot be read, or one edited
    /// by anyone else, claims every plan: apply refuses rather than replay.
    #[test]
    fn unreadable_receipts_refuse_apply() {
        let claim = |forge: &Forge, body: &str, editor: Option<&str>| {
            forge
                .comments
                .borrow_mut()
                .push(comment(2, body.into(), editor));
        };
        // A receipt with a field this build does not know.
        let valid = |_: &Forge| {
            let detail = Detail::new("1@2026-10-14T10:00:00Z", Outcome::Applied);
            let epic = IssueRef::parse("example/epics#1").unwrap();
            receipt_body(&detail, "u", &epic, "t")
                .unwrap()
                .replace("\"outcome\"", "\"later\": 1,\n  \"outcome\"")
        };
        let fenced = |json: &str| format!("```{APPLY_MARKER}\n{json}\n```");
        for (body, editor, refused) in [
            (fenced("{not json"), None, true),
            (
                fenced(
                    r#"{"schema": "agent-board/epic-apply/v9", "plan": "1@x", "outcome": "applied"}"#,
                ),
                None,
                true,
            ),
            (
                "<!-- agent-board-epic-apply: 1@2026-10-14T10:00:00Z -->".to_owned(),
                Some("intruder"),
                true,
            ),
            (String::new(), None, false),
        ] {
            let forge = Forge::full();
            let body = match body.is_empty() {
                true => valid(&forge),
                false => body,
            };
            claim(&forge, &body, editor);
            let result = apply(&forge, &issue(), &config(), OffsetDateTime::UNIX_EPOCH);
            match refused {
                true => {
                    let error = format!("{:#}", result.unwrap_err());
                    // An edit by anyone else stops plan selection first.
                    assert!(
                        error.contains("cannot be read") || error.contains("may not write plans"),
                        "{error}"
                    );
                }
                // A later build's field is ignored: still the plan's claim.
                false => assert_eq!(result.unwrap(), [ALREADY_CLAIMED]),
            }
            assert!(forge.requests.borrow().is_empty(), "{body}");
        }
    }
}
