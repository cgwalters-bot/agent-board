//! Incremental issue enrollment. Discovery is not permission to implement:
//! only the existing writer-authenticated plan path can authorize that.
use super::*;
use crate::github::field_writes;

const PAGE: usize = 100;
const SHARE: usize = 15;
const LOOKBACK_DAYS: i64 = 2;
const DONE: &str = "Done";

pub(super) fn query(assigned: bool) -> String {
    let facts = if assigned {
        FACTS.replacen("labels(first: 50)", BOARD_ASSIGNEES, 1)
    } else {
        FACTS.to_owned()
    };
    format!(
        r#"query($owner: String!, $name: String!, $since: DateTime!) {{
      repository(owner: $owner, name: $name) {{
        issues(first: {PAGE}, filterBy: {{since: $since}}, orderBy: {{field: UPDATED_AT, direction: ASC}}, states: OPEN) {{
          nodes {{ id updatedAt ...Facts
            timelineItems(itemTypes: [REOPENED_EVENT], last: 1) {{ nodes {{ ... on ReopenedEvent {{ createdAt }} }} }}
            projectItems(first: 100) {{ nodes {{ id createdAt creator {{ login }} project {{ id }} }} pageInfo {{ hasNextPage }} }} }}
          pageInfo {{ hasNextPage }}
        }}
      }}
    }}
    {facts}"#
    )
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    /// At most one changed-issue listing per configured followed repository,
    /// inside 15% of the pass's requests. Oldest watermarks go first so an
    /// exhausted share does not always exclude the last repository.
    pub(super) fn intake(&mut self) {
        let bounds = self.client.bounds;
        let kept = bounds.kept.get();
        let share = (bounds.limit.get() * SHARE / 100).min(bounds.available());
        bounds
            .kept
            .set(bounds.left.get().saturating_sub(share).max(kept));
        let result = self.intake_within();
        bounds.kept.set(kept);
        if let Err(error) = result {
            self.line(format!(
                "intake warning: {error:#}; unread issues keep their watermark"
            ));
        }
    }

    fn intake_within(&mut self) -> Result<()> {
        let config = self.config;
        let board_config = config.github_board().context("no intake board")?;
        let mut seen = BTreeSet::new();
        let mut repos = config
            .follow
            .iter()
            .map(|r| r.to_ascii_lowercase())
            .filter(|r| seen.insert(r.clone()))
            .collect::<Vec<_>>();
        self.report.issue_intake.retain(|r, _| repos.contains(r));
        if self.apply {
            let initial = (self.now - time::Duration::days(LOOKBACK_DAYS)).format(&Rfc3339)?;
            for repo in &repos {
                self.report
                    .issue_intake
                    .entry(repo.clone())
                    .or_insert_with(|| initial.clone());
            }
        }
        repos.sort_by_key(|r| self.report.issue_intake.get(r).cloned());
        let github = crate::github::GitHub {
            transport: Budget {
                inner: self.client.inner,
                bounds: self.client.bounds,
            },
            owner: board_config.owner.clone(),
            number: board_config.project,
        };
        let (project, _, fields) = github.schema()?;
        let mut board = self.board()?;
        let mut added = Vec::new();
        for repo in repos {
            if self.client.bounds.share_spent() {
                break;
            }
            let since = self
                .report
                .issue_intake
                .get(&repo)
                .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
                .unwrap_or(self.now - time::Duration::days(LOOKBACK_DAYS))
                .min(self.now);
            if self.apply {
                self.report
                    .issue_intake
                    .insert(repo.clone(), since.format(&Rfc3339)?);
            }
            let result = self.intake_repo(
                &github,
                (&project, &fields),
                &repo,
                since,
                (&mut board, &mut added),
            );
            match result {
                Ok(watermark) if self.apply => {
                    self.report
                        .issue_intake
                        .insert(repo, watermark.format(&Rfc3339)?);
                }
                Ok(_) => (),
                Err(error) => self.line(format!(
                    "intake warning: {repo}: {error:#}; watermark retained"
                )),
            }
        }
        self.line(format!(
            "intake: {} new issue(s){}",
            added.len(),
            if added.is_empty() {
                String::new()
            } else {
                format!(" ({})", added.join(", "))
            }
        ));
        self.board = Some(Some(board));
        Ok(())
    }

    fn intake_repo(
        &self,
        github: &crate::github::GitHub<Budget<'_, C>>,
        (project, fields): (&str, &[Value]),
        repo: &str,
        since: OffsetDateTime,
        (board, added): (&mut BoardRead, &mut Vec<String>),
    ) -> Result<OffsetDateTime> {
        let (owner, name) = repo.split_once('/').context("intake repository")?;
        let data = self.client.graphql(
            &query(self.config.assign_holds),
            json!({
                "owner": owner, "name": name, "since": since.format(&Rfc3339)?
            }),
        )?;
        let connection = &data["repository"]["issues"];
        let nodes = connection["nodes"]
            .as_array()
            .context("intake issues have no nodes")?;
        ensure!(nodes.len() <= PAGE, "intake page exceeds {PAGE}");
        let more = connection["pageInfo"]["hasNextPage"]
            .as_bool()
            .context("intake has no pageInfo")?;
        let mut last = since;
        for node in nodes {
            let mut facts = Facts::from_node(node)?;
            ensure!(
                facts.issue.number > 0
                    && facts.issue.repository.eq_ignore_ascii_case(repo)
                    && facts.open,
                "intake returned a different repository or non-open issue"
            );
            let updated = facts.updated.context("intake issue lacks updatedAt")?;
            ensure!(
                updated >= last && updated <= self.now,
                "intake timestamps are out of order or in the future"
            );
            last = updated;
            ensure!(facts.labels_complete, "intake labels are incomplete");
            let members = &node["projectItems"];
            let memberships = members["nodes"]
                .as_array()
                .context("intake memberships missing")?;
            ensure!(
                memberships
                    .iter()
                    .all(|m| !text(m, "/id").is_empty() && !text(m, "/project/id").is_empty()),
                "intake membership is unreadable"
            );
            let members_on_board = memberships
                .iter()
                .filter(|m| m["project"]["id"] == project)
                .collect::<Vec<_>>();
            ensure!(members_on_board.len() <= 1, "duplicate intake membership");
            ensure!(
                !members_on_board.is_empty() || members["pageInfo"]["hasNextPage"] == false,
                "intake membership is incomplete"
            );
            let existing = board.items.get(&key(&facts.issue));
            let since_at = |value: &Value| {
                value
                    .as_str()
                    .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
                    .is_some_and(|at| at >= since)
            };
            let reopened = since_at(&node["timelineItems"]["nodes"][0]["createdAt"]);
            match (members_on_board.first(), existing) {
                // Only new or reopened issues: an update to an older one, or
                // to one whose card someone removed, does not enroll it.
                (None, None) if facts.created >= since || reopened => (),
                // Only the bot's own card added since the watermark, which a
                // failed write retains, is an interrupted enrollment: a Status
                // cleared by hand, or on an older card, is left alone.
                (Some(member), Some(card)) if card.status.is_none() => {
                    if !(text(member, "/creator/login").eq_ignore_ascii_case(&self.config.bot)
                        && since_at(&member["createdAt"]))
                    {
                        continue;
                    }
                }
                // An issue reopened since the watermark leaves Done for Todo.
                (Some(_), Some(card)) if card.status.as_deref() == Some(DONE) => {
                    if !reopened {
                        continue;
                    }
                }
                _ => continue,
            }
            let kept_priority = existing.and_then(|f| f.priority.as_ref());
            let priority = board.options.iter().find(|p| {
                facts.labels.iter().any(|l| {
                    l.eq_ignore_ascii_case(p) || l.eq_ignore_ascii_case(&format!("priority/{p}"))
                })
            });
            facts.priority = kept_priority.or(priority).cloned();
            facts.why = existing.and_then(|f| f.why.clone());
            facts.status = Some("Todo".into());
            let mut edits = Vec::new();
            if let Some(priority) = priority.filter(|_| kept_priority.is_none()) {
                edits.push(format!(
                    "{}={priority}",
                    self.config.github_board().context("board")?.priority_field
                ));
            }
            edits.push("Status=Todo".into());
            let writes = field_writes(fields, &edits)?;
            let count = writes.len() + usize::from(members_on_board.is_empty());
            if let Some(held) = self.caps.check_many(Kind::Write, count) {
                bail!("{held}");
            }
            ensure!(
                self.client.bounds.available() >= count + WRITE_RESERVE,
                "intake request share spent"
            );
            if self.apply {
                let item = match members_on_board.first() {
                    Some(member) => text(member, "/id").to_owned(),
                    None => {
                        let id = node["id"]
                            .as_str()
                            .filter(|id| !id.is_empty())
                            .context("intake issue lacks id")?;
                        self.caps.count(Kind::Write);
                        github.add_item(project, id)?
                    }
                };
                ensure!(!item.is_empty(), "intake membership lacks id");
                for write in writes {
                    self.caps.count(Kind::Write);
                    github.set_field(project, &item, &write)?;
                }
            } else {
                for _ in 0..count {
                    self.caps.count(Kind::Write);
                }
            }
            added.push(facts.issue.display());
            board.items.insert(key(&facts.issue), facts);
        }
        // No search index is involved. One second overlaps timestamp precision;
        // a page wholly at that boundary cannot safely advance: report it.
        if more {
            ensure!(
                last > since + time::Duration::seconds(1),
                "intake timestamp overflow; inspect the repository before advancing state"
            );
            Ok(last - time::Duration::seconds(1))
        } else {
            Ok(self.now - time::Duration::seconds(1))
        }
    }
}
