//! Typed writes, never agent-selected API paths. Free comments must not become
//! bot-authored plans, poll markers or execution receipts on the next pass.
use super::*;
use serde::Deserialize;

pub const ACTIONS: &[&str] = &[
    "comment",
    "create_issue",
    "file_bug",
    "close",
    "remove_needs_human",
    "replace_state",
    "status_update",
];

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
    ReplaceState {
        body: String,
    },
    StatusUpdate {
        status: String,
        body: String,
    },
}

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

fn text(body: &str, max: usize, protocols: bool) -> Result<()> {
    ensure!(
        !unsigned(body).is_empty() && body.chars().count() <= max,
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

/// `body` without the attribution the agent may have added, trimmed.
fn unsigned(body: &str) -> &str {
    let body = body.trim();
    body.strip_suffix(crate::FOOTER).unwrap_or(body).trim()
}

/// `body` signed with the attribution the agent may have left out, once and
/// last, after `note`.
fn signed(body: &str, note: Option<&str>) -> String {
    let body = unsigned(body);
    match note {
        Some(note) => format!("{body}\n\n{note}\n\n{}", crate::FOOTER),
        None => format!("{body}\n\n{}", crate::FOOTER),
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
                | Write::RemoveNeedsHuman { repository, item },
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
                Write::Comment { body, .. } => text(body, 4000, true)?,
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
                    text(title, 256, true)?;
                    text(body, 4000, true)?;
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
                Write::StatusUpdate { status, body } => {
                    ensure!(p.agent.project.is_some(), "No configured project");
                    ensure!(
                        ["on-track", "at-risk", "off-track"].contains(&status.as_str()),
                        "Unknown project status"
                    );
                    text(body, 4000, true)?;
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
            }
        }
        Ok(())
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Dispatch(r) => r.describe(),
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

    pub fn preflight(&self, api: &dyn Api, p: &Policy) -> Result<()> {
        if let Self::Dispatch(r) = self {
            return super::preflight(api, p, r);
        }
        if let Some((repo, item)) = self.issue() {
            let issue = api.get(&format!("/repos/{repo}/issues/{item}"))?;
            ensure!(issue["labels"].is_array(), "Missing target labels");
            ensure!(
                !has_label(&issue, "epic"),
                "The epic is not a supervisor target"
            );
            match self {
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
                    // stops a dispatch.
                    ensure!(
                        !super::HELD_LABELS.iter().any(|h| has_label(&issue, h)),
                        "The issue is the operator's or held"
                    );
                    triage_done(api, p, repo, item, triage_run, *triage_comment)?;
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
                _ => (),
            }
        }
        if let Self::Write(Write::ReplaceState { .. }) = self {
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

    pub fn apply(&self, api: &dyn Api, p: &Policy, ctx: &Run) -> Result<Value> {
        let (method, path, body) = match self {
            Self::Dispatch(r) => (
                "POST",
                format!(
                    "/repos/{}/actions/workflows/{}/dispatches",
                    p.settings.dispatch_repo, p.settings.dispatch_workflow
                ),
                json!({"ref": ctx.default_branch, "inputs": {"task": r.task(&ctx.run_url), "repo": r.repository, "item": r.item.to_string(), "kind": r.kind, "pool": r.pool}}),
            ),
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
                    json!({"body": signed(body, Some(crate::input::SUPERVISOR_NOTE))}),
                ),
                Write::CreateIssue { title, body } => (
                    "POST",
                    format!(
                        "/repos/{}/issues",
                        p.agent.tracker.as_deref().context("No tracker")?
                    ),
                    json!({"title": title, "body": signed(body, Some(crate::input::SUPERVISOR_NOTE)), "assignees": [p.settings.operator], "labels": ["needs-human"]}),
                ),
                // Unheld, so the note alone keeps the supervisor from making
                // the agent's words a later run's brief until a person removes it.
                Write::FileBug { title, body } => (
                    "POST",
                    format!(
                        "/repos/{}/issues",
                        p.agent.tracker.as_deref().context("No tracker")?
                    ),
                    json!({"title": title, "body": signed(body, Some(crate::input::SUPERVISOR_NOTE))}),
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
                Write::ReplaceState { body } => {
                    let s = p.agent.state.as_ref().context("No state issue")?;
                    (
                        "PATCH",
                        format!("/repos/{}/issues/{}", s.repository, s.issue),
                        json!({"body": body}),
                    )
                }
                Write::StatusUpdate { status, body } => (
                    "POST",
                    "/graphql".into(),
                    json!({
                        "query": "mutation($project:ID!,$status:ProjectV2StatusUpdateStatus!,$body:String!){createProjectV2StatusUpdate(input:{projectId:$project,status:$status,body:$body}){statusUpdate{id}}}",
                        "variables": {"project": p.agent.project, "status": status.replace('-', "_").to_ascii_uppercase(), "body": signed(body, None)}
                    }),
                ),
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

fn triage_done(
    api: &dyn Api,
    p: &Policy,
    repo: &str,
    item: u64,
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
        if e["event"] == "reopened" || e["event"] == "renamed" {
            ensure!(
                millis(&e["created_at"]).is_some_and(|t| t < at),
                "The issue was reopened or renamed after its triage"
            );
        }
    }
    for later in paged(api, repo, item, "comments")? {
        let created = millis(&later["created_at"]).context("Comment timestamp")?;
        ensure!(
            created <= at || (later["user"]["login"] == p.settings.bot && created <= end),
            "The issue was discussed after its triage"
        );
    }
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
