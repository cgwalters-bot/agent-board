//! A readout of existing poll decisions, never an input to them.
use super::*;
use serde::Serialize;

pub const SCHEMA: &str = "agent-board/maintainer-report/v1";
pub const MARKER: &str = "<!-- agent-board-maintainer-report -->";
// GitHub rejects oversized comments. Refuse rather than publish a partial inventory.
const MAX_COMMENT_BYTES: usize = 60_000;

/// A repository's merge queue for its default branch, which `mergeQueue`
/// reads without a branch: a readout of what is queued, not an input to any
/// step (tracker#1356).
pub(super) const QUEUE_QUERY: &str = r#"query($owner: String!, $name: String!, $entries: Int!) {
  repository(owner: $owner, name: $name) {
    mergeQueue { entries(first: $entries) {
      nodes { position state headCommit { oid } pullRequest { number } }
      pageInfo { hasNextPage }
    } }
  }
}"#;
/// Queue entries read per repository; a longer queue is reported unread
/// rather than partial.
const QUEUE_ENTRIES: u64 = 100;
/// The issue and its first page of comments, once visibility is checked.
const PUBLISH_READS: usize = 2;

#[derive(Clone, Debug)]
pub struct Destination {
    pub repository: String,
    pub issue: u64,
}

impl Destination {
    pub(super) fn from_json(value: Option<&Value>) -> Result<Option<Self>> {
        let Some(value) = value else { return Ok(None) };
        check_keys(&[&["repository", "issue"]], value, "maintainerReport")?;
        let repository = value["repository"]
            .as_str()
            .context("maintainerReport.repository is not a string")?;
        validate_repository(repository)?;
        let issue = value["issue"]
            .as_u64()
            .filter(|n| *n > 0)
            .context("maintainerReport.issue is not a positive integer")?;
        Ok(Some(Self {
            repository: repository.into(),
            issue,
        }))
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct Pull {
    pub repository: String,
    pub number: u64,
    pub head: String,
    pub action: String,
    pub reason: String,
    pub ran: bool,
    #[serde(skip)]
    pub queue_event: Option<Value>,
}

impl Pull {
    pub(super) fn observed(
        repo: &str,
        number: u64,
        pull: &Value,
        decision: &Decision,
        ran: bool,
    ) -> Self {
        let queue_event = nodes(pull, "/timelineItems/nodes").last().map(|event| {
            json!({
                "id": event["id"], "created_at": event["createdAt"], "reason": event["reason"],
            })
        });
        Self {
            repository: repo.into(),
            number,
            head: text(pull, "/headRefOid").into(),
            action: decision.action.into(),
            reason: decision.reason.clone(),
            ran,
            queue_event,
        }
    }
}

fn render(
    now: OffsetDateTime,
    apply: bool,
    follow: &BTreeSet<&String>,
    pulls: &BTreeMap<(String, u64), Pull>,
    queues: &BTreeMap<&String, Value>,
    outcome: &Outcome,
) -> Result<String> {
    let mut grouped: BTreeMap<&str, Vec<&Pull>> = BTreeMap::new();
    let mut events = Vec::new();
    for pull in pulls.values() {
        grouped.entry(&pull.action).or_default().push(pull);
        if let Some(event) = &pull.queue_event {
            events.push(json!({"repository": pull.repository, "number": pull.number,
                "event": event, "step": pull}));
        }
    }
    let needs_human: Vec<Value> = outcome
        .waits
        .iter()
        .filter(|w| w.kind != crate::stop::SUPERVISOR_LABEL)
        .map(|w| {
            json!({
                "repository": w.repository, "number": w.number, "head": w.head,
                "kind": w.kind, "reason": w.reason,
            })
        })
        .collect();
    let mut snapshot = json!({
        "schema": SCHEMA,
        "generated_at": now.format(&Rfc3339)?,
        "mode": if apply { "apply" } else { "preview" },
        "repositories": follow,
        "pull_requests": grouped,
        "merge_queue_events": events,
        "merge_queues": queues,
        "needs_human": needs_human,
        "undecided_repositories": outcome.undecided,
        "errors": outcome.errors,
    });
    let supervisor: Vec<Value> = outcome.waits.iter().filter(|w| w.kind == crate::stop::SUPERVISOR_LABEL).map(|w| json!({"repository":w.repository,"number":w.number,"head":w.head,"kind":w.kind,"reason":w.reason})).collect();
    if !supervisor.is_empty() {
        snapshot["needs_supervisor"] = json!(supervisor);
    }
    // Escape backticks in JSON strings so repository-controlled reasons cannot
    // close the protocol fence. JSON readers recover the original text.
    let json = serde_json::to_string_pretty(&snapshot)?.replace('`', "\\u0060");
    let body = format!("{MARKER}\n\n```{SCHEMA}\n{json}\n```\n");
    ensure!(
        body.len() <= MAX_COMMENT_BYTES,
        "maintainer report exceeds {MAX_COMMENT_BYTES} bytes; not publishing a partial report"
    );
    Ok(body)
}

impl<R: Rest, G: Transport> Pass<'_, R, G> {
    pub(super) fn maintainer_report(&mut self, follow: &BTreeSet<&String>, outcome: &mut Outcome) {
        let Some(at) = &self.config.policy.maintainer_report else {
            return;
        };
        // Spend the required visibility reads before any optional queue reads.
        if self.apply.is_some()
            && let Err(error) = self.check_report_visibility(&at.repository, follow)
        {
            outcome.error("maintainer report", &error);
            return;
        }
        // Deferred review starts now have their final outcome; labelled skips
        // also use the poll's existing state rather than reclassifying anything.
        for step in &outcome.steps {
            if let Some(pull) = self
                .report_pulls
                .get_mut(&(step.repository.clone(), step.number))
            {
                pull.head.clone_from(&step.head);
                pull.action = step.action.into();
                pull.reason.clone_from(&step.reason);
                pull.ran = step.ran;
            }
        }
        // Undecided repositories, as outside the envelope, have no inventory
        // and no queue read either; an unread queue is null and an error.
        let mut queues = BTreeMap::new();
        for repo in follow {
            if outcome.undecided.contains(&repo.to_ascii_lowercase()) {
                continue;
            }
            let entries = self.merge_queue(repo).unwrap_or_else(|error| {
                outcome.error(&format!("maintainer report: {repo} merge queue"), &error);
                Value::Null
            });
            queues.insert(*repo, entries);
        }
        let result = (|| -> Result<()> {
            let body = render(
                self.now,
                self.apply.is_some(),
                follow,
                &self.report_pulls,
                &queues,
                outcome,
            )?;
            if self.apply.is_none() {
                outcome.maintainer_report = Some(body);
                return Ok(());
            }
            self.publish_report(at, &body, &mut outcome.lines)
        })();
        if let Err(error) = result {
            outcome.error("maintainer report", &error);
        }
    }

    /// `repo`'s default-branch queue entries in order, empty where it has no
    /// merge queue. A longer queue than is read, or a budget down to what
    /// publishing needs, is an error.
    fn merge_queue(&mut self, repo: &str) -> Result<Value> {
        let (owner, name) = repo.split_once('/').context("repository")?;
        // Standalone poll also reserves the write and, in preview, the unread
        // destination. The hosted pass already keeps a separate write reserve.
        let reserve = PUBLISH_READS + 1 + usize::from(self.apply.is_none());
        let share_too_short = self.apply.is_some()
            && self
                .rest
                .share_left()
                .is_some_and(|left| left <= PUBLISH_READS);
        ensure!(
            self.remaining > reserve && !share_too_short,
            "request budget left for publishing the report"
        );
        self.spend()?;
        let data = self.graphql.graphql(
            QUEUE_QUERY,
            json!({"owner": owner, "name": name, "entries": QUEUE_ENTRIES}),
        )?;
        let queue = data
            .pointer("/repository/mergeQueue")
            .context("merge queue not given")?;
        ensure!(
            queue.is_null()
                || queue.pointer("/entries/pageInfo/hasNextPage") == Some(&Value::Bool(false)),
            "more than {QUEUE_ENTRIES} entries queued"
        );
        let entries = nodes(queue, "/entries/nodes")
            .iter()
            .map(|e| {
                json!({
                    "number": e["pullRequest"]["number"], "head": e["headCommit"]["oid"],
                    "position": e["position"], "state": e["state"],
                })
            })
            .collect();
        Ok(Value::Array(entries))
    }

    fn check_report_visibility(&mut self, repo: &str, follow: &BTreeSet<&String>) -> Result<()> {
        let destination = self.report_repository(repo)?;
        match (
            destination["private"].as_bool(),
            destination.get("visibility"),
        ) {
            (Some(true), None) => {} // Keep accepting the existing private flag.
            (Some(true), Some(visibility))
                if visibility == "private" || visibility == "internal" => {}
            (Some(false), Some(visibility)) if visibility == "public" => {
                // The report names even undecided repositories. Check the whole
                // follow set, reusing the destination read if it is followed.
                let mut checked = BTreeSet::from([repo.to_ascii_lowercase()]);
                for repo in follow {
                    if checked.insert(repo.to_ascii_lowercase()) {
                        let source = self.report_repository(repo)?;
                        ensure!(
                            source["private"] == false && source["visibility"] == "public",
                            "{repo} is not confirmed public; refusing to publish to a public repository"
                        );
                    }
                }
            }
            _ => bail!("{repo}: cannot confirm maintainer report destination visibility"),
        }
        Ok(())
    }

    fn report_repository(&mut self, repo: &str) -> Result<Value> {
        self.get(&format!("repos/{repo}"))
            .with_context(|| format!("{repo}: could not read repository visibility"))
    }

    fn publish_report(
        &mut self,
        at: &Destination,
        body: &str,
        lines: &mut Vec<String>,
    ) -> Result<()> {
        let repo = &at.repository;
        let issue = at.issue;
        let target = self.get(&format!("repos/{repo}/issues/{issue}"))?;
        ensure!(
            target["pull_request"].is_null(),
            "maintainerReport must name an issue, not a pull request"
        );
        let comments = self.paged(
            &format!("repos/{repo}/issues/{issue}/comments?per_page=100"),
            true,
        )?;
        let comment = comments
            .iter()
            .filter(|c| {
                text(c, "/user/login").eq_ignore_ascii_case(&self.config.bot)
                    && text(c, "/body").starts_with(MARKER)
            })
            .min_by_key(|c| c["id"].as_u64().unwrap_or(u64::MAX));
        if comment.is_some_and(|c| text(c, "/body") == body) {
            lines.push("maintainer report: unchanged".into());
            return Ok(());
        }
        if let Some(held) = self.caps.and_then(|c| c.check(Kind::Write)) {
            lines.push(format!("maintainer report: {held}"));
            return Ok(());
        }
        let (method, path) = match comment {
            Some(c) => (
                "PATCH",
                format!(
                    "repos/{repo}/issues/comments/{}",
                    c["id"]
                        .as_u64()
                        .context("maintainer report comment has no id")?
                ),
            ),
            None => ("POST", format!("repos/{repo}/issues/{issue}/comments")),
        };
        if let Some(caps) = self.caps {
            caps.count(Kind::Write);
        }
        self.send(self.rest, method, &path, json!({"body": body}), 0)?;
        lines.push(format!("maintainer report: published to {repo}#{issue}"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_text_cannot_close_the_fence_and_oversized_reports_are_refused() {
        let mut pulls = BTreeMap::new();
        let reason = "untrusted\n```\ntext";
        pulls.insert(
            ("o/r".into(), 1),
            Pull {
                repository: "o/r".into(),
                number: 1,
                head: String::new(),
                action: "wait".into(),
                reason: reason.into(),
                ran: false,
                queue_event: None,
            },
        );
        let outcome = Outcome::default();
        let body = render(
            OffsetDateTime::UNIX_EPOCH,
            false,
            &BTreeSet::new(),
            &pulls,
            &BTreeMap::new(),
            &outcome,
        )
        .unwrap();
        assert_eq!(body.matches("```").count(), 2);
        let json = body
            .split_once(&format!("```{SCHEMA}\n"))
            .unwrap()
            .1
            .split_once("\n```")
            .unwrap()
            .0;
        let snapshot: Value = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot["pull_requests"]["wait"][0]["reason"], reason);
        pulls.values_mut().next().unwrap().reason = "x".repeat(MAX_COMMENT_BYTES);
        assert!(
            render(
                OffsetDateTime::UNIX_EPOCH,
                false,
                &BTreeSet::new(),
                &pulls,
                &BTreeMap::new(),
                &outcome
            )
            .is_err()
        );
    }
}
