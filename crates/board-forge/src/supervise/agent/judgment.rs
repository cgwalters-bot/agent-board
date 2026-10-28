//! Typed writes, never agent-selected API paths. Free comments must not become
//! bot-authored plans, poll markers or execution receipts on the next pass.
use super::*;
use crate::Footer;
use crate::changes;
use crate::dispatch::{GitHub, Start, StartOutcome};
use board_core::NEEDS_HUMAN_LABEL as NEEDS_HUMAN;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;

pub const ACTIONS: &[&str] = &[
    "comment",
    "create_issue",
    "file_bug",
    "close",
    "remove_needs_human",
    "clear_needs_human",
    "replace_state",
    "status_update",
];

/// What shows that the operator no longer waits on an item held
/// `needs-human`: facts the executor reads, never the agent's reading of
/// the conversation.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// The operator commented [`RELEASE_COMMAND`] after the newest
    /// `needs-human` label was added.
    OperatorComment,
    /// The item was closed after the newest `needs-human` label was added; a
    /// merged pull request is.
    Closed,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Write {
    Comment {
        repository: String,
        item: u64,
        body: String,
    },
    CreateIssue {
        title: String,
        body: String,
    },
    /// An issue in the tracker like `create_issue`, but neither assigned nor
    /// held: a failure for a run to pick up, not a question for the operator.
    /// A separate action rather than a kind of `create_issue`, so that its
    /// own cap bounds it and leaves it off where the caller has not set one.
    FileBug {
        title: String,
        body: String,
    },
    Close {
        repository: String,
        item: u64,
        triage_run: String,
        triage_comment: u64,
    },
    RemoveNeedsHuman {
        repository: String,
        item: u64,
    },
    /// Takes `needs-human` off an issue or pull request the operator has
    /// settled, on [`Evidence`]; `comment` is the operator's comment ID.
    /// Without it, a compromised agent could release any of the operator's
    /// holds to get the work dispatched.
    ClearNeedsHuman {
        repository: String,
        item: u64,
        evidence: Evidence,
        #[serde(default)]
        comment: Option<u64>,
    },
    ReplaceState {
        body: String,
    },
    /// The executor writes the body: `summary` over the project's changes
    /// since its last status update, each of `notes` (keyed `owner/repo#N`)
    /// on its item's line.
    StatusUpdate {
        status: String,
        summary: String,
        #[serde(default)]
        notes: BTreeMap<String, String>,
        /// Built by [`Checked::preflight`], read before any write; the
        /// reason it could not be, which refuses this request alone.
        #[serde(skip)]
        body: Option<std::result::Result<String, String>>,
        /// Why the body has no spend split though `agent.passConfig` asks
        /// for one: an unreadable split leaves out only its line.
        #[serde(skip)]
        no_split: Option<String>,
    },
}

/// A status update's own text: the facts come from the forge.
const SUMMARY_LIMIT: usize = 1_000;
const NOTE_LIMIT: usize = 280;
const MAX_NOTES: usize = 20;
/// How the status update's line on the needs-human queue starts, by which
/// the next update reads the count it gave.
const QUEUE_LINE: &str = "**Needs human:**";
/// How many of the queue's items the status update names.
const QUEUE_TOP: usize = 5;
/// The longest board Priority the status update shows; a longer one, or
/// one with other than letters, digits, spaces or dashes, is left out.
const PRIORITY_LIMIT: usize = 20;
/// The project's newest status update, which the queue's count is read from.
const NEWEST_UPDATE: &str = "query($id:ID!){node(id:$id){... on ProjectV2{statusUpdates(first:1,orderBy:{field:CREATED_AT,direction:DESC}){nodes{createdAt body}}}}}";
/// The project field whose option order ranks the queue, as the pass's
/// default `priorityField`.
const PRIORITY_FIELD: &str = "Priority";
/// The options of the project's [`PRIORITY_FIELD`], in the board's order.
const PRIORITY_OPTIONS: &str = "query($id:ID!,$field:String!){node(id:$id){... on ProjectV2{field(name:$field){... on ProjectV2SingleSelectField{options{name}}}}}}";
/// The whole of the operator's comment that releases a `needs-human` hold.
const RELEASE_COMMAND: &str = "/release";

pub enum Checked {
    Dispatch(Request),
    Write(Write),
}

pub fn cap(policy: &Policy, action: &str) -> u64 {
    if action == "dispatch" {
        policy.agent.max_dispatches_per_day
    } else {
        policy.agent.daily_caps.get(action).copied().unwrap_or(0)
    }
}

pub fn parse_batch(source: &str, policy: &Policy) -> Result<Vec<Checked>> {
    ensure!(
        source.len() <= MAX_REQUEST_BYTES,
        "Supervisor requests exceed 32 KiB"
    );
    let mut dispatch = vec![];
    let mut requests = vec![];
    for line in source.lines().filter(|l| !l.trim().is_empty()) {
        let mut value: Value = serde_json::from_str(line)
            .ok()
            .context("Malformed JSON Lines")?;
        if value["action"] == "dispatch" {
            dispatch.push(line);
            requests.extend(
                validate_batch(line, policy)?
                    .into_iter()
                    .map(Checked::Dispatch),
            );
        } else {
            ensure!(
                value["type"] == "supervisor_action",
                "Unknown supervisor action"
            );
            value
                .as_object_mut()
                .context("Unknown supervisor action")?
                .remove("type");
            let write: Write = serde_json::from_value(value)
                .ok()
                .context("Unknown action or unexpected supervisor fields")?;
            requests.push(Checked::Write(write));
        }
    }
    ensure!(
        requests.len() <= MAX_REQUESTS,
        "Supervisor permits at most ten requests"
    );
    // Reuse the closed dispatch protocol, including its per-pass bound.
    validate_batch(&dispatch.join("\n"), policy)?;
    for r in &requests {
        ensure!(cap(policy, r.action()) > 0, "Supervisor action is disabled");
        r.validate(policy)?;
    }
    // Every target is checked before the first write, so a dispatch could not
    // see a comment posted earlier in its own batch; the run would read it.
    let commented: BTreeSet<_> = requests
        .iter()
        .filter(|r| r.action() == "comment")
        .filter_map(Checked::issue)
        .map(|(repo, item)| (repo.to_ascii_lowercase(), item))
        .collect();
    ensure!(
        !requests.iter().any(|r| matches!(r, Checked::Dispatch(d)
            if commented.contains(&(d.repository.to_ascii_lowercase(), d.item)))),
        "A comment and a dispatch on one item"
    );
    Ok(requests)
}

pub(super) fn has_label(issue: &Value, label: &str) -> bool {
    issue["labels"].as_array().is_some_and(|ls| {
        ls.iter().any(|l| {
            l["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(label))
        })
    })
}

fn text(body: &str, max: usize, protocols: bool, footer: &Footer) -> Result<()> {
    ensure!(
        !unsigned(body, footer).is_empty() && body.chars().count() <= max,
        "Empty or oversized supervisor text"
    );
    if protocols {
        // These comments are data, not a new bot-signed plan or poll receipt.
        let lower = body.to_ascii_lowercase();
        ensure!(
            !lower.contains("```")
                && !lower.contains("<!--")
                && !lower.contains("triage found this issue done:"),
            "Supervisor text cannot contain protocol fences or markers"
        );
    }
    Ok(())
}

/// `body` without the attribution the agent may have added, trimmed. Only a
/// line of its own: a configured `generatedBy` may be a word that ends a
/// sentence too.
fn unsigned<'a>(body: &'a str, footer: &Footer) -> &'a str {
    let body = body.trim();
    match body.strip_suffix(footer.as_str()) {
        Some(rest) if rest.is_empty() || rest.ends_with('\n') => rest.trim(),
        _ => body,
    }
}

/// `body` signed with the attribution the agent may have left out, once and
/// last, after `note`.
pub(super) fn signed(body: &str, note: Option<&str>, footer: &Footer) -> String {
    let body = unsigned(body, footer);
    match note {
        Some(note) => format!("{body}\n\n{note}\n\n{footer}"),
        None => format!("{body}\n\n{footer}"),
    }
}

fn target(policy: &Policy, repo: &str, item: u64) -> Result<()> {
    ensure!(
        policy.settings.follow.iter().any(|r| r == repo),
        "Repository outside supervisor policy"
    );
    ensure!(
        (1..=(1u64 << 53) - 1).contains(&item),
        "Malformed supervisor target"
    );
    ensure!(
        ![
            Some(&policy.agent.requests),
            Some(&policy.agent.log),
            policy.agent.state.as_ref()
        ]
        .into_iter()
        .flatten()
        .any(|i| i.repository.eq_ignore_ascii_case(repo) && i.issue == item),
        "The supervisor's own issues are not targets"
    );
    Ok(())
}

impl Checked {
    pub fn action(&self) -> &'static str {
        match self {
            Self::Dispatch(_) => "dispatch",
            Self::Write(w) => match w {
                Write::Comment { .. } => "comment",
                Write::CreateIssue { .. } => "create_issue",
                Write::FileBug { .. } => "file_bug",
                Write::Close { .. } => "close",
                Write::RemoveNeedsHuman { .. } => "remove_needs_human",
                Write::ClearNeedsHuman { .. } => "clear_needs_human",
                Write::ReplaceState { .. } => "replace_state",
                Write::StatusUpdate { .. } => "status_update",
            },
        }
    }

    fn issue(&self) -> Option<(&str, u64)> {
        match self {
            Self::Dispatch(r) => Some((&r.repository, r.item)),
            Self::Write(
                Write::Comment {
                    repository, item, ..
                }
                | Write::Close {
                    repository, item, ..
                }
                | Write::RemoveNeedsHuman { repository, item }
                | Write::ClearNeedsHuman {
                    repository, item, ..
                },
            ) => Some((repository, *item)),
            _ => None,
        }
    }

    fn validate(&self, p: &Policy) -> Result<()> {
        if let Some((repo, item)) = self.issue() {
            target(p, repo, item)?;
        }
        if let Self::Write(w) = self {
            match w {
                Write::Comment { body, .. } => text(body, 4000, true, &p.settings.footer)?,
                Write::CreateIssue { title, body } | Write::FileBug { title, body } => {
                    let repo = p
                        .agent
                        .tracker
                        .as_deref()
                        .context("No configured tracker")?;
                    ensure!(
                        p.settings.follow.iter().any(|r| r == repo),
                        "Tracker outside supervisor repositories"
                    );
                    text(title, 256, true, &p.settings.footer)?;
                    text(body, 4000, true, &p.settings.footer)?;
                }
                Write::ReplaceState { body } => {
                    let state = p
                        .agent
                        .state
                        .as_ref()
                        .context("No configured state issue")?;
                    ensure!(
                        p.settings.follow.contains(&state.repository),
                        "State outside supervisor repositories"
                    );
                    // Memory is deliberately arbitrary, but it is never a dispatch target.
                    ensure!(body.chars().count() <= 60_000, "Oversized supervisor state");
                }
                Write::StatusUpdate {
                    status,
                    summary,
                    notes,
                    ..
                } => {
                    ensure!(p.agent.project.is_some(), "No configured project");
                    ensure!(
                        ["on-track", "at-risk", "off-track"].contains(&status.as_str()),
                        "Unknown project status"
                    );
                    text(summary, SUMMARY_LIMIT, true, &p.settings.footer)?;
                    // The next update reads the queue's count from this line.
                    ensure!(
                        !summary.contains(QUEUE_LINE),
                        "The summary cannot hold the queue's line"
                    );
                    ensure!(notes.len() <= MAX_NOTES, "Too many status notes");
                    for (reference, note) in notes {
                        ensure!(
                            changes::is_ref(reference),
                            "Malformed status note reference"
                        );
                        text(note, NOTE_LIMIT, true, &p.settings.footer)?;
                        ensure!(!note.contains(['\n', '\r']), "A status note is one line");
                    }
                }
                Write::Close {
                    triage_run,
                    triage_comment,
                    ..
                } => {
                    run_id(triage_run, p)?;
                    ensure!(*triage_comment > 0, "Missing triage receipt");
                }
                Write::RemoveNeedsHuman { .. } => (),
                Write::ClearNeedsHuman {
                    evidence, comment, ..
                } => ensure!(
                    match evidence {
                        Evidence::OperatorComment => comment.is_some_and(|c| c > 0),
                        Evidence::Closed => comment.is_none(),
                    },
                    "An operator comment's evidence is its ID, and only it"
                ),
            }
        }
        Ok(())
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Dispatch(r) => r.describe(),
            Self::Write(Write::StatusUpdate {
                no_split: Some(reason),
                ..
            }) => format!(
                "`{}` configured target (no spend split: {reason})",
                self.action()
            ),
            _ => {
                let target = self
                    .issue()
                    .map(|(r, n)| format!("{r} #{n}"))
                    .unwrap_or_else(|| "configured target".into());
                format!("`{}` {target}", self.action())
            }
        }
    }

    /// The issue the log line links to. `receipt` is the write's API answer;
    /// a created issue is only addressable through the number it reports.
    pub fn link(&self, p: &Policy, receipt: Option<&Value>) -> String {
        // A cleared hold links its evidence: the comment, or the closed item.
        if let Self::Write(Write::ClearNeedsHuman {
            repository,
            item,
            comment: Some(id),
            ..
        }) = self
        {
            return format!("https://github.com/{repository}/issues/{item}#issuecomment-{id}");
        }
        let target = self.issue().or_else(|| match self {
            Self::Write(Write::CreateIssue { .. } | Write::FileBug { .. }) => p
                .agent
                .tracker
                .as_deref()
                .zip(receipt.and_then(|r| r["number"].as_u64().filter(|n| *n > 0))),
            Self::Write(Write::ReplaceState { .. }) => p
                .agent
                .state
                .as_ref()
                .map(|s| (s.repository.as_str(), s.issue)),
            _ => None,
        });
        target
            .map(|(r, n)| format!("https://github.com/{r}/issues/{n}"))
            .unwrap_or_default()
    }

    /// Checks the target as it is now; a status update reads its changes.
    pub fn preflight(&mut self, api: &dyn Api, p: &Policy, now: i64) -> Result<()> {
        if let Self::Write(Write::StatusUpdate {
            summary,
            notes,
            body,
            no_split,
            ..
        }) = self
        {
            // A board read the update could not make is no reason to hold
            // back the batch's other requests.
            match status_body(api, p, now, summary, notes) {
                Ok((text, reason)) => {
                    *body = Some(Ok(text));
                    *no_split = reason;
                }
                Err(error) => *body = Some(Err(format!("{error:#}"))),
            }
            return Ok(());
        }
        let this = &*self;
        if let Self::Dispatch(r) = this {
            return super::preflight(api, p, r);
        }
        if let Some((repo, item)) = this.issue() {
            let issue = api.get(&format!("/repos/{repo}/issues/{item}"))?;
            ensure!(issue["labels"].is_array(), "Missing target labels");
            ensure!(
                !has_label(&issue, "epic"),
                "The epic is not a supervisor target"
            );
            match this {
                // Later dispatches refuse the conversation once the note is in
                // it; a run already started would read it unchecked.
                Self::Write(Write::Comment { .. }) => ensure!(
                    !super::in_flight(api, p, repo, item)?,
                    "The item has a run in flight"
                ),
                Self::Write(Write::Close {
                    triage_run,
                    triage_comment,
                    ..
                }) => {
                    ensure!(
                        issue["state"] == "open"
                            && issue.get("pull_request").is_none_or(Value::is_null),
                        "Not an open issue"
                    );
                    // The labels are read now, so a hold the operator added
                    // after the triage receipt still stops the close, as it
                    // stops a dispatch; `triage_done` admits only the pass's
                    // own hold of this receipt.
                    let held = has_label(&issue, NEEDS_HUMAN);
                    ensure!(
                        !(super::HELD_LABELS.iter())
                            .any(|h| *h != NEEDS_HUMAN && has_label(&issue, h)),
                        "The issue is the operator's or held"
                    );
                    triage_done(api, p, (repo, item, held), triage_run, *triage_comment)?;
                }
                Self::Write(Write::RemoveNeedsHuman { .. }) => {
                    ensure!(
                        issue["state"] == "open"
                            && issue["pull_request"].is_object()
                            && has_label(&issue, "needs-human"),
                        "Not a held open pull request"
                    );
                    conflict_only(api, p, repo, item)?;
                }
                Self::Write(Write::ClearNeedsHuman { comment, .. }) => {
                    settled(api, p, repo, item, *comment)?;
                }
                _ => (),
            }
        }
        if let Self::Write(Write::ReplaceState { .. }) = this {
            let s = p
                .agent
                .state
                .as_ref()
                .context("No configured state issue")?;
            let issue = api.get(&format!("/repos/{}/issues/{}", s.repository, s.issue))?;
            ensure!(issue["labels"].is_array(), "Missing state labels");
            ensure!(
                issue.get("pull_request").is_none_or(Value::is_null) && !has_label(&issue, "epic"),
                "State must be an ordinary issue"
            );
        }
        Ok(())
    }

    /// Whether a failure leaves the batch's later requests to go ahead: a
    /// status update is a report on the board, not one of the board's writes.
    pub fn stops_batch(&self) -> bool {
        !matches!(self, Self::Write(Write::StatusUpdate { .. }))
    }

    pub fn apply(&self, api: &dyn Api, p: &Policy, ctx: &Run) -> Result<Value> {
        let (method, path, body) = match self {
            Self::Dispatch(r) => {
                let target = Target {
                    caller: p.settings.dispatch_repo.clone(),
                    workflow: p.settings.dispatch_workflow.clone(),
                    git_ref: ctx.default_branch.clone(),
                };
                // The run is found by its title, so its details are not asked for.
                let github = GitHub {
                    target: &target,
                    run_details: false,
                };
                let start = Start {
                    repository: &r.repository,
                    item: r.item,
                    kind: &r.kind,
                    brief: &r.task(&ctx.run_url),
                    pool: Some(&r.pool),
                    runner: None,
                    base: None,
                };
                // `Api::send` refuses any status but a 2xx itself.
                let send = |path: &str, body: &Value| {
                    Ok((200, api.send("POST", &format!("/{path}"), body)?))
                };
                return match github.start(&start, || Ok(()), send) {
                    StartOutcome::Started(_) | StartOutcome::Unnamed => Ok(Value::Null),
                    StartOutcome::NotStarted(error) | StartOutcome::Ambiguous(error) => Err(error),
                };
            }
            Self::Write(w) => match w {
                Write::Comment {
                    repository,
                    item,
                    body,
                } => (
                    "POST",
                    format!("/repos/{repository}/issues/{item}/comments"),
                    // The bot posts it, so mark it as the agent's: dispatch
                    // admission refuses a conversation holding this note.
                    json!({"body": signed(body, Some(crate::input::SUPERVISOR_NOTE), &p.settings.footer)}),
                ),
                Write::CreateIssue { title, body } => (
                    "POST",
                    format!(
                        "/repos/{}/issues",
                        p.agent.tracker.as_deref().context("No tracker")?
                    ),
                    json!({"title": title, "body": signed(body, Some(crate::input::SUPERVISOR_NOTE), &p.settings.footer), "assignees": [p.settings.operator], "labels": ["needs-human"]}),
                ),
                // Unheld, so the note alone keeps the supervisor from making
                // the agent's words a later run's brief until a person removes it.
                Write::FileBug { title, body } => (
                    "POST",
                    format!(
                        "/repos/{}/issues",
                        p.agent.tracker.as_deref().context("No tracker")?
                    ),
                    json!({"title": title, "body": signed(body, Some(crate::input::SUPERVISOR_NOTE), &p.settings.footer)}),
                ),
                Write::Close {
                    repository, item, ..
                } => (
                    "PATCH",
                    format!("/repos/{repository}/issues/{item}"),
                    json!({"state": "closed", "state_reason": "completed"}),
                ),
                Write::RemoveNeedsHuman { repository, item } => {
                    // A human hold may have replaced the poll's while other requests
                    // were checked. Read the provenance again immediately before DELETE.
                    conflict_only(api, p, repository, *item)?;
                    (
                        "DELETE",
                        format!("/repos/{repository}/issues/{item}/labels/needs-human"),
                        json!({}),
                    )
                }
                Write::ClearNeedsHuman {
                    repository,
                    item,
                    comment,
                    ..
                } => {
                    // The operator may have held it again since the check.
                    settled(api, p, repository, *item, *comment)?;
                    (
                        "DELETE",
                        format!("/repos/{repository}/issues/{item}/labels/needs-human"),
                        json!({}),
                    )
                }
                Write::ReplaceState { body } => {
                    let s = p.agent.state.as_ref().context("No state issue")?;
                    (
                        "PATCH",
                        format!("/repos/{}/issues/{}", s.repository, s.issue),
                        json!({"body": body}),
                    )
                }
                Write::StatusUpdate { status, body, .. } => {
                    let body = match body {
                        Some(Ok(body)) => body,
                        Some(Err(reason)) => bail!("Status body refused: {reason}"),
                        None => bail!("The status update was not prepared"),
                    };
                    (
                        "POST",
                        "/graphql".into(),
                        json!({
                            "query": "mutation($project:ID!,$status:ProjectV2StatusUpdateStatus!,$body:String!){createProjectV2StatusUpdate(input:{projectId:$project,status:$status,body:$body}){statusUpdate{id}}}",
                            "variables": {"project": p.agent.project, "status": status.replace('-', "_").to_ascii_uppercase(), "body": body}
                        }),
                    )
                }
            },
        };
        let receipt = api.send(method, &path, &body)?;
        if self.action() == "status_update" {
            ensure!(
                receipt["errors"].is_null()
                    && receipt["data"]["createProjectV2StatusUpdate"]["statusUpdate"]["id"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty()),
                "Project status update failed"
            );
        }
        Ok(receipt)
    }
}

/// `summary` and `notes` over the project's changes since its last status
/// update, then with `agent.passConfig` the same window's spend split,
/// signed; and why a split asked for was left out.
fn status_body(
    api: &dyn Api,
    p: &Policy,
    now: i64,
    summary: &str,
    notes: &BTreeMap<String, String>,
) -> Result<(String, Option<String>)> {
    let project = p
        .agent
        .project
        .as_deref()
        .context("No configured project")?;
    let forge = super::super::Graphql(api);
    let now = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(now) * 1_000_000)?;
    let last = changes::last_update(&forge, project)?;
    let since = match last {
        Some(since) => since,
        None => now - changes::FIRST_WINDOW,
    };
    let footer = &p.settings.footer;
    let summary = unsigned(summary, footer);
    let previous = match last {
        Some(at) => previous_queue(api, project, at)?,
        None => None,
    };
    let summary = match queue_line(api, p, project, previous)? {
        Some(queue) => format!("{summary}\n\n{queue}"),
        None => summary.to_owned(),
    };
    // Room for the attribution [`signed`] appends.
    let limit = crate::github::STATUS_BODY_LIMIT - footer.as_str().chars().count() - 2;
    let changes = changes::read(
        &forge,
        project,
        since,
        None,
        Some(&p.settings.operator),
        false,
    )?;
    let split = p.agent.pass_config.as_deref().map(|path| {
        let text = std::fs::read_to_string(path).with_context(|| format!("read {path}"))?;
        let config = crate::pass::Config::parse(&text).with_context(|| path.to_owned())?;
        let split = crate::budget::split::read(&forge, &config, since, now)?;
        anyhow::Ok(crate::budget::split::line(&split))
    });
    let summary = Some(summary.as_str());
    let (body, no_split) = match split {
        Some(Ok(line)) => {
            let room = limit.saturating_sub(line.chars().count() + 2);
            match changes.status_body(summary, notes, room) {
                Ok(body) => (format!("{}\n\n{line}", body.trim_end()), None),
                // The body trims its sections to fit, so only a summary and
                // notes that fill the room the line would leave come here.
                Err(_) => (
                    changes.status_body(summary, notes, limit)?,
                    Some("it does not fit".to_owned()),
                ),
            }
        }
        Some(Err(error)) => (
            changes.status_body(summary, notes, limit)?,
            Some(format!("{error:#}")),
        ),
        None => (changes.status_body(summary, notes, limit)?, None),
    };
    Ok((signed(&body, None, footer), no_split))
}

/// The needs-human count the project's newest status update, posted `at`,
/// gave on its [`QUEUE_LINE`], if it gave one. The agent's summary cannot
/// hold that line, so the last one is the executor's.
fn previous_queue(api: &dyn Api, project: &str, at: time::OffsetDateTime) -> Result<Option<u64>> {
    let read = api.graphql(NEWEST_UPDATE, json!({"id": project}))?;
    let newest = &read["node"]["statusUpdates"]["nodes"][0];
    let created = newest["createdAt"].as_str().map(changes::parse_time);
    // An update that is not the one the changes start from says nothing.
    if !matches!(created, Some(Ok(t)) if t == at) {
        return Ok(None);
    }
    Ok(newest["body"]
        .as_str()
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix(QUEUE_LINE))
        .next_back()
        .and_then(|rest| {
            let rest = rest.trim_start();
            let digits = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            rest[..digits].parse().ok()
        }))
}

/// The status update's line on the needs-human queue, from the pass's hold
/// snapshot on the usage issue: its size, its change from `previous`, and
/// its first [`QUEUE_TOP`] by the order of `project`'s Priority options.
/// None without a usage config or a snapshot there.
fn queue_line(
    api: &dyn Api,
    p: &Policy,
    project: &str,
    previous: Option<u64>,
) -> Result<Option<String>> {
    use crate::pass::holds::{HOLDS_MARKER, HOLDS_SCHEMA};
    let Some(usage) = &p.settings.usage else {
        return Ok(None);
    };
    let comments = paged(api, &usage.issue.repository, usage.issue.issue, "comments")?;
    let Some(snapshot) = comments
        .iter()
        .find(|c| {
            c["user"]["login"]
                .as_str()
                .is_some_and(|l| l.eq_ignore_ascii_case(&usage.bot))
                && c["body"]
                    .as_str()
                    .is_some_and(|b| b.starts_with(HOLDS_MARKER))
        })
        .and_then(|c| super::super::health::json_block(c["body"].as_str()?))
        .filter(|s| s["schema"] == HOLDS_SCHEMA)
    else {
        return Ok(None);
    };
    let at = snapshot["at"]
        .as_str()
        .filter(|at| changes::parse_time(at).is_ok())
        .context("The hold snapshot has no time")?;
    let mut queue = vec![];
    for hold in snapshot["holds"]
        .as_array()
        .context("The hold snapshot has no holds")?
        .iter()
        .filter(|h| h["kind"] == "needs-human")
    {
        let item = hold["item"]
            .as_str()
            .filter(|i| i.contains('/') && changes::is_ref(i))
            .context("A needs-human hold names no item")?;
        let priority = hold["priority"].as_str().filter(|p| {
            p.len() <= PRIORITY_LIMIT
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-')
        });
        queue.push((priority, item));
    }
    // As the pass ranks: by option index, unset or unknown last. Stable: the
    // snapshot's order, by repository and number, within a Priority.
    let read = api.graphql(
        PRIORITY_OPTIONS,
        json!({"id": project, "field": PRIORITY_FIELD}),
    )?;
    let options: Vec<&str> = read["node"]["field"]["options"]
        .as_array()
        .map(|o| o.iter().filter_map(|o| o["name"].as_str()).collect())
        .unwrap_or_default();
    queue.sort_by_key(|(priority, _)| {
        priority
            .and_then(|p| options.iter().position(|o| *o == p))
            .unwrap_or(options.len())
    });
    let size = queue.len() as u64;
    let mut line = format!("{QUEUE_LINE} {size}");
    if let Some(previous) = previous {
        let _ = match size.cmp(&previous) {
            std::cmp::Ordering::Greater => {
                write!(line, ", {} more than at the last update", size - previous)
            }
            std::cmp::Ordering::Less => {
                write!(line, ", {} fewer than at the last update", previous - size)
            }
            std::cmp::Ordering::Equal => write!(line, ", as at the last update"),
        };
    }
    let _ = write!(line, ", as of {at}.");
    if !queue.is_empty() {
        let first: Vec<String> = queue
            .iter()
            .take(QUEUE_TOP)
            .map(|(priority, item)| match priority {
                Some(priority) => format!("{item} ({priority})"),
                None => (*item).to_owned(),
            })
            .collect();
        let _ = write!(line, " First by priority: {}.", first.join(", "));
    }
    Ok(Some(line))
}

/// That the operator has settled `item`'s `needs-human` hold, which it
/// still has, after the label's newest addition: with `comment`, the
/// operator's [`RELEASE_COMMAND`] on the issue; without, the item merged,
/// or closed by the operator or the bot, so that its author cannot reopen
/// it unheld.
fn settled(api: &dyn Api, p: &Policy, repo: &str, item: u64, comment: Option<u64>) -> Result<()> {
    let issue = api.get(&format!("/repos/{repo}/issues/{item}"))?;
    ensure!(
        issue["labels"].is_array() && has_label(&issue, "needs-human"),
        "The item is not held needs-human"
    );
    // Evidence older than the newest hold says nothing of it.
    let mut held = None;
    for e in paged(api, repo, item, "events")? {
        let label = e["label"]["name"].as_str();
        if e["event"] == "labeled" && label.is_some_and(|n| n.eq_ignore_ascii_case("needs-human")) {
            let t = millis(&e["created_at"]).context("Label timestamp")?;
            held = held.max(Some(t));
        }
    }
    let held = held.context("No label provenance")?;
    let Some(id) = comment else {
        let closer = issue["closed_by"]["login"].as_str();
        ensure!(
            issue["state"] == "closed"
                && (issue["pull_request"]["merged_at"].is_string()
                    || closer.is_some_and(|l| l == p.settings.operator || l == p.settings.bot)),
            "The item is neither merged nor closed by the operator or the bot"
        );
        let closed = millis(&issue["closed_at"]).context("Closure timestamp")?;
        ensure!(held < closed, "The item was held again after it was closed");
        return Ok(());
    };
    // Taking the poll's hold off a pull request starts its fix rounds again
    // (`MAX_FIX_ROUNDS`); the poll's own `operatorAnswers` reads an answer there.
    ensure!(
        issue.get("pull_request").is_none_or(Value::is_null),
        "An operator comment settles an issue's hold, not a pull request's"
    );
    let c = api.get(&format!("/repos/{repo}/issues/comments/{id}"))?;
    ensure!(
        c["user"]["login"] == p.settings.operator.as_str()
            && c["issue_url"] == format!("https://api.github.com/repos/{repo}/issues/{item}"),
        "Not the operator's comment on this item"
    );
    // The whole comment, as a review's `/send`: an answer, or a comment that
    // restates the hold, releases nothing.
    ensure!(
        c["body"]
            .as_str()
            .is_some_and(|b| b.trim() == RELEASE_COMMAND),
        "The operator's comment is not {RELEASE_COMMAND}"
    );
    let at = millis(&c["created_at"]).context("Operator comment timestamp")?;
    ensure!(held < at, "The operator has not released it since the hold");
    Ok(())
}

fn run_id(url: &str, p: &Policy) -> Result<u64> {
    let prefix = format!(
        "https://github.com/{}/actions/runs/",
        p.settings.dispatch_repo
    );
    let id = url
        .strip_prefix(&prefix)
        .context("Triage run outside configured dispatcher")?;
    ensure!(
        !id.starts_with('0') && id.bytes().all(|b| b.is_ascii_digit()),
        "Malformed triage run link"
    );
    id.parse().context("Malformed triage run ID")
}

fn triage_answer_done(body: &str) -> bool {
    // Match tracker's .github/poll/schedule.cjs readTriage VERDICT_RE:
    // column-zero, case-sensitive fences, optional horizontal whitespace,
    // exactly one complete block, JSON with a done verdict. The named run
    // and time window below replace the scheduler's question/request binding.
    struct Answer {
        verdict: Value,
    }

    impl<'de> Deserialize<'de> for Answer {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
            struct Visitor;

            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Answer;

                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("a triage answer object")
                }

                fn visit_map<M: serde::de::MapAccess<'de>>(
                    self,
                    mut map: M,
                ) -> std::result::Result<Answer, M::Error> {
                    let mut verdict = Value::Null;
                    while let Some(key) = map.next_key::<String>()? {
                        if key == "verdict" {
                            // JSON.parse keeps the last duplicate key.
                            verdict = map.next_value()?;
                        } else {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                    Ok(Answer { verdict })
                }
            }

            d.deserialize_map(Visitor)
        }
    }

    // JavaScript multiline anchors also recognize CR and Unicode separators;
    // the opening fence still requires LF (optionally preceded by one CR).
    let boundary = |c| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}');
    let starts = std::iter::once(0)
        .chain(
            body.char_indices()
                .filter_map(|(i, c)| boundary(c).then_some(i + c.len_utf8())),
        )
        .collect::<Vec<_>>();
    let mut answer = None;
    let mut consumed = 0;
    for &start in &starts {
        if start < consumed {
            continue;
        }
        let Some(tail) = body[start..].strip_prefix("```schedule-triage") else {
            continue;
        };
        let tail = tail.trim_start_matches([' ', '\t']);
        let tail = tail.strip_prefix('\r').unwrap_or(tail);
        let Some(json) = tail.strip_prefix('\n') else {
            continue;
        };
        let json_start = body.len() - json.len();
        if let Some((end, after)) = starts.iter().filter(|&&i| i >= json_start).find_map(|&i| {
            let rest = body[i..]
                .strip_prefix("```")?
                .trim_start_matches([' ', '\t']);
            (rest.is_empty() || rest.starts_with(boundary)).then_some((i, body.len() - rest.len()))
        }) {
            if answer.is_some() {
                return false;
            }
            consumed = after;
            // Ignore unrelated fields like readTriage; their numeric range or
            // escaped Unicode need not fit Rust's typed JSON values.
            answer = Some(serde_json::from_str::<Answer>(&body[json_start..end]).ok());
        }
    }
    answer.flatten().is_some_and(|v| v.verdict == "done")
}

/// That triage run `url` found `item` done in its receipt `comment`, and
/// nothing since outdates it. `held`: the issue has `needs-human`, which
/// only the pass's own hold of this receipt may have put there.
fn triage_done(
    api: &dyn Api,
    p: &Policy,
    (repo, item, held): (&str, u64, bool),
    url: &str,
    comment: u64,
) -> Result<()> {
    let id = run_id(url, p)?;
    let run = api.get(&format!(
        "/repos/{}/actions/runs/{id}",
        p.settings.dispatch_repo
    ))?;
    ensure!(
        run["status"] == "completed"
            && run["conclusion"] == "success"
            && run["path"] == format!(".github/workflows/{}", p.settings.dispatch_workflow)
            && run_title(run["display_title"].as_str().unwrap_or_default())
                == Some(("triage", repo, item)),
        "Not a successful triage run on this issue"
    );
    let c = api.get(&format!("/repos/{repo}/issues/comments/{comment}"))?;
    // A successful run alone says nothing about its finding. Require its explicit
    // done receipt as well, on this issue, rather than trusting request prose.
    ensure!(
        c["user"]["login"] == p.settings.bot
            && c["created_at"].is_string()
            && c["updated_at"] == c["created_at"]
            && c["issue_url"] == format!("https://api.github.com/repos/{repo}/issues/{item}")
            && c["body"].as_str().is_some_and(triage_answer_done),
        "Missing unedited triage done receipt"
    );
    let at = millis(&c["created_at"]).context("Triage receipt timestamp")?;
    let start = millis(&run["created_at"]).context("Triage start timestamp")?;
    let end = millis(&run["updated_at"]).context("Triage end timestamp")?;
    ensure!(at >= start && at <= end, "Receipt outside triage run");
    // The finding is about the issue as triage read it. A later reopen, title
    // or body edit, or discussion outdates it; only the triage run's own
    // closing comments may follow its receipt.
    let (owner, name) = repo.split_once('/').context("Repository")?;
    let edited = api.graphql(
        LAST_EDITED,
        json!({"owner": owner, "name": name, "number": item}),
    )?;
    let issue = &edited["repository"]["issue"];
    ensure!(issue.is_object(), "The issue could not be read");
    ensure!(
        issue["lastEditedAt"].is_null() || millis(&issue["lastEditedAt"]).is_some_and(|e| e < at),
        "The issue was edited after its triage"
    );
    // A verdict closes the issue with no review after it, so triage must have
    // read only collaborators' text: locked from before its run started to
    // now, so that no one else could add, edit or delete any, and written
    // only by collaborators (agent-board#155 left that check to closure).
    let (mut locked, mut unlocked) = (None, None);
    // The `needs-human` labels added since the run ended, and by whom.
    let mut labels = vec![];
    for e in paged(api, repo, item, "events")? {
        let newest = match e["event"].as_str() {
            Some("locked") => Some(&mut locked),
            Some("unlocked") => Some(&mut unlocked),
            _ => None,
        };
        if let Some(newest) = newest {
            let t = millis(&e["created_at"]).context("Lock timestamp")?;
            *newest = (*newest).max(Some(t));
        }
        let label = e["label"]["name"].as_str();
        if e["event"] == "labeled" && label.is_some_and(|n| n.eq_ignore_ascii_case(NEEDS_HUMAN)) {
            let t = millis(&e["created_at"]).context("Label timestamp")?;
            if t > end {
                labels.push(e["actor"]["login"].clone());
            }
        }
        if e["event"] == "reopened" || e["event"] == "renamed" {
            ensure!(
                millis(&e["created_at"]).is_some_and(|t| t < at),
                "The issue was reopened or renamed after its triage"
            );
        }
    }
    // The pass's hold of this receipt. `ask create --apply` must comment on
    // the blocked item before opening its question: adding the same label
    // again emits no event and cannot invalidate an existing pass marker.
    let marker = crate::pass::hold_marker(comment);
    let mut marked = false;
    for later in paged(api, repo, item, "comments")? {
        let created = millis(&later["created_at"]).context("Comment timestamp")?;
        let bot = later["user"]["login"] == p.settings.bot;
        if held
            && !marked
            && bot
            && created > end
            && later["updated_at"] == later["created_at"]
            && later["body"].as_str().and_then(|b| b.lines().next()) == Some(marker.as_str())
        {
            marked = true;
            continue;
        }
        ensure!(
            created <= at || (bot && created <= end),
            "The issue was discussed after its triage"
        );
    }
    // Added once since the run ended, by the bot: none before it (a hold
    // the triage could not answer), none by the operator, and not taken off
    // and put back since. A later ask invalidates the receipt via its comment,
    // even when its additive labeling leaves this event list unchanged.
    ensure!(
        !held || (marked && labels == [json!(p.settings.bot)]),
        "The issue is held, and not by the pass for this triage"
    );
    ensure!(
        locked.is_some_and(|t| t < start && unlocked < Some(t)),
        "The issue was not locked throughout its triage"
    );
    super::conversation(api, repo, item, true)
}

const LAST_EDITED: &str = r#"query($owner:String!,$name:String!,$number:Int!){
  repository(owner:$owner,name:$name){issue(number:$number){lastEditedAt}}
}"#;

fn paged(api: &dyn Api, repo: &str, item: u64, resource: &str) -> Result<Vec<Value>> {
    let mut all = vec![];
    for page in 1..=COMMENT_PAGES {
        let values = api.get(&format!(
            "/repos/{repo}/issues/{item}/{resource}?per_page=100&page={page}"
        ))?;
        let values = values.as_array().context("Incomplete issue evidence")?;
        all.extend(values.iter().cloned());
        if values.len() < 100 {
            return Ok(all);
        }
    }
    bail!("Issue evidence is too long to check")
}

const CONFLICT_QUERY: &str = r#"query($owner:String!,$name:String!,$number:Int!){
  repository(owner:$owner,name:$name){pullRequest(number:$number){
    state isDraft author{login} headRefOid baseRefName mergeable reviewDecision
    commits(last:1){nodes{commit{statusCheckRollup{state contexts(first:100){
      pageInfo{hasNextPage} nodes{__typename
        ... on CheckRun{name status conclusion checkSuite{app{databaseId}} isRequired(pullRequestNumber:$number)}
        ... on StatusContext{context state isRequired(pullRequestNumber:$number)}
      }
    }}}}}
  }}
}"#;

fn conflict_only(api: &dyn Api, p: &Policy, repo: &str, item: u64) -> Result<()> {
    let comments = paged(api, repo, item, "comments")?;
    let marker = crate::poll::find_marker(&comments, &p.settings.bot);
    let id = marker.id.context("No poll stop")?;
    let stop = comments
        .iter()
        .find(|c| c["id"].as_u64() == Some(id))
        .context("No poll stop")?;
    ensure!(
        marker.stopped
            && stop["created_at"].is_string()
            && stop["created_at"] == stop["updated_at"],
        "No unedited poll stop"
    );
    let events = paged(api, repo, item, "events")?;
    let last = events
        .iter()
        .rev()
        .find(|e| e["event"] == "labeled" && e["label"]["name"] == "needs-human")
        .context("No label provenance")?;
    ensure!(
        last["actor"]["login"] == p.settings.bot
            && millis(&last["created_at"])
                .zip(millis(&stop["created_at"]))
                .is_some_and(|(a, b)| a <= b),
        "The hold is not the poll's"
    );
    let (owner, name) = repo.split_once('/').context("Repository")?;
    let facts = api.graphql(
        CONFLICT_QUERY,
        json!({"owner": owner, "name": name, "number": item}),
    )?;
    let pull = &facts["repository"]["pullRequest"];
    ensure!(
        pull["state"] == "OPEN"
            && pull["isDraft"] == false
            && pull["author"]["login"] == p.settings.bot
            && pull["mergeable"] == "CONFLICTING"
            && pull["reviewDecision"] == "APPROVED"
            && pull.pointer("/commits/nodes/0/commit/statusCheckRollup/state")
                == Some(&json!("SUCCESS"))
            && pull["headRefOid"]
                .as_str()
                .is_some_and(|sha| marker.sha.as_deref() == Some(sha)),
        "Merge conflict is not the only problem"
    );
    let contexts = &pull["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"];
    ensure!(
        contexts["pageInfo"]["hasNextPage"] == false && contexts["nodes"].is_array(),
        "Incomplete check evidence"
    );
    let base = pull["baseRefName"]
        .as_str()
        .filter(|b| !b.is_empty())
        .context("Missing base branch")?;
    let branch = api.get(&format!("/repos/{repo}/branches/{}", encode(base)))?;
    let mut rules = vec![];
    for page in 1..=RUN_PAGES {
        let read = api.get(&format!(
            "/repos/{repo}/rules/branches/{}?per_page=100&page={page}",
            encode(base)
        ))?;
        let read = read.as_array().context("Missing branch rules")?;
        for rule in read {
            ensure!(
                rule["type"] != "required_status_checks"
                    || rule["parameters"]["required_status_checks"].is_array(),
                "Incomplete required checks"
            );
        }
        rules.extend(read.iter().cloned());
        if read.len() < 100 {
            break;
        }
        ensure!(page < RUN_PAGES, "Too many branch rules to check");
    }
    let required = crate::poll::required_checks(&json!(rules), &branch)?;
    let checks = crate::poll::classify_checks(pull, &required);
    ensure!(
        checks.failed.is_empty() && checks.pending.is_empty(),
        "Required checks have not all passed"
    );
    Ok(())
}
