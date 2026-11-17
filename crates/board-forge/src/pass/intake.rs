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

impl<'a, C: Rest + Transport> Scheduler<'a, C> {
    /// At most one changed-issue listing per configured followed repository,
    /// inside 15% of the pass's requests. Oldest watermarks go first so an
    /// exhausted share does not always exclude the last repository. The
    /// board, which the later stages reuse, and its field schema (a single
    /// read) are read before the share: they would otherwise leave the
    /// listings little.
    pub(super) fn intake(&mut self) {
        let read = self
            .config
            .github_board()
            .context("no intake board")
            .map(|b| crate::github::GitHub {
                transport: Budget {
                    inner: self.client.inner,
                    bounds: self.client.bounds,
                },
                owner: b.owner.clone(),
                number: b.project,
            });
        let read = read.and_then(|github| {
            let (project, _, fields) = github.schema()?;
            Ok((github, project, fields, self.board()?))
        });
        let bounds = self.client.bounds;
        let kept = bounds.kept.get();
        let share = (bounds.limit.get() * SHARE / 100).min(bounds.available());
        bounds
            .kept
            .set(bounds.left.get().saturating_sub(share).max(kept));
        let result = read.and_then(|(github, project, fields, board)| {
            self.intake_within(&github, (&project, &fields), board)
        });
        bounds.kept.set(kept);
        if let Err(error) = result {
            self.line(format!(
                "intake warning: {error:#}; unread issues keep their watermark"
            ));
        }
    }

    fn intake_within(
        &mut self,
        github: &crate::github::GitHub<Budget<'a, C>>,
        (project, fields): (&str, &[Value]),
        mut board: BoardRead,
    ) -> Result<()> {
        let config = self.config;
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
        let mut added = Vec::new();
        let mut skipped = Vec::new();
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
                github,
                (project, fields),
                &repo,
                since,
                (&mut board, &mut added, &mut skipped),
            );
            // Where the repository stops, its watermark stays and the skipped
            // issues are read again.
            let skipped = std::mem::take(&mut skipped);
            if result.is_ok() {
                for (issue, error) in skipped {
                    self.line(format!("intake warning: {issue}: {error:#}; skipped"));
                }
            }
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
        (board, added, skipped): (
            &mut BoardRead,
            &mut Vec<String>,
            &mut Vec<(String, anyhow::Error)>,
        ),
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
            // A node outside the query's scope, or out of the page's order,
            // which bounds the watermark, discredits the page: it stops the
            // repository.
            ensure!(
                text(node, "/repository/nameWithOwner").eq_ignore_ascii_case(repo)
                    && node["state"] == "OPEN",
                "intake returned a different repository or non-open issue"
            );
            let updated = node["updatedAt"]
                .as_str()
                .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
                .context("intake issue lacks updatedAt")?;
            ensure!(
                updated >= last && updated <= self.now,
                "intake timestamps are out of order or in the future"
            );
            last = updated;
            let facts = Facts::from_node(node)?;
            ensure!(facts.issue.number > 0, "intake issue is unnumbered");
            // A truncated or unreadable membership or label list is the
            // issue's own and lasting: the watermark passes it, where
            // retrying would stall every later issue of its repository.
            // Being older than the watermark then, it is not enrolled later.
            let issue = facts.issue.display();
            let (mut facts, member) = match self.enrollment(node, facts, since, project, board) {
                Ok(Some(found)) => found,
                Ok(None) => continue,
                Err(error) => {
                    skipped.push((issue, error));
                    continue;
                }
            };
            let existing = board.items.get(&key(&facts.issue));
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
            let count = writes.len() + usize::from(member.is_none());
            if let Some(held) = self.caps.check_many(Kind::Write, count) {
                bail!("{held}");
            }
            ensure!(
                self.client.bounds.available() >= count + WRITE_RESERVE,
                "intake request share spent"
            );
            if self.apply {
                let item = match member {
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

    /// The listed issue `node`, with its `facts`, and its membership of the
    /// board, where intake enrolls it or repairs its card; an error where
    /// its labels or memberships are incomplete or unreadable.
    fn enrollment<'n>(
        &self,
        node: &'n Value,
        facts: Facts,
        since: OffsetDateTime,
        project: &str,
        board: &BoardRead,
    ) -> Result<Option<(Facts, Option<&'n Value>)>> {
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
        let member = members_on_board.first().copied();
        let since_at = |value: &Value| {
            value
                .as_str()
                .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
                .is_some_and(|at| at >= since)
        };
        let reopened = since_at(&node["timelineItems"]["nodes"][0]["createdAt"]);
        let enrolled = match (member, board.items.get(&key(&facts.issue))) {
            // Only new or reopened issues: an update to an older one, or
            // to one whose card someone removed, does not enroll it.
            (None, None) => facts.created >= since || reopened,
            // Only the bot's own card added since the watermark, which a
            // failed write retains, is an interrupted enrollment: a Status
            // cleared by hand, or on an older card, is left alone.
            (Some(member), Some(card)) if card.status.is_none() => {
                text(member, "/creator/login").eq_ignore_ascii_case(&self.config.bot)
                    && since_at(&member["createdAt"])
            }
            // An issue reopened since the watermark leaves Done for Todo.
            (Some(_), Some(card)) if card.status.as_deref() == Some(DONE) => reopened,
            _ => false,
        };
        Ok(enrolled.then_some((facts, member)))
    }
}
