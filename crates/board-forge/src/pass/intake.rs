//! Incremental issue enrollment. Discovery is not permission to implement:
//! only the existing writer-authenticated plan path can authorize that.
use super::*;
use crate::github::field_writes;
use crate::poll::ISSUE_INTAKE_PENDING_LIMIT;

pub(super) const PAGE: usize = 100;
const SHARE: usize = 15;
const LOOKBACK_DAYS: i64 = 2;

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
          nodes {{ __typename id updatedAt ...Facts
            reopens: timelineItems(itemTypes: [REOPENED_EVENT], last: 1) {{ nodes {{ ... on ReopenedEvent {{ createdAt }} }} }}
            projectItems(first: 100) {{ nodes {{ id project {{ id }}
              status: fieldValueByName(name: "Status") {{ ... on ProjectV2ItemFieldSingleSelectValue {{ name updatedAt }} }}
            }} pageInfo {{ hasNextPage }} }} }}
          pageInfo {{ hasNextPage }}
        }}
      }}
    }}
    {facts}"#
    )
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    /// At most one changed-issue listing per configured followed repository,
    /// inside 15% of the pass's requests. Oldest watermarks go first.
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
        self.report
            .issue_intake_pending
            .retain(|r, _| repos.contains(r));
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
        &mut self,
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
        let mut listed = BTreeSet::new();
        let mut ahead = false;
        for node in nodes {
            ensure!(node["__typename"] == "Issue", "intake returned a non-issue");
            let mut facts = Facts::from_node(node)?;
            ensure!(
                facts.issue.number > 0
                    && facts.issue.repository.eq_ignore_ascii_case(repo)
                    && facts.open,
                "intake returned a different repository or non-open issue"
            );
            let updated = facts.updated.context("intake issue lacks updatedAt")?;
            ensure!(updated >= last, "intake timestamps are out of order");
            if updated > self.now {
                // Updated since the pass began, as are all after it: every
                // issue up to then was listed, and the next pass reads these.
                ahead = true;
                break;
            }
            last = updated;
            ensure!(facts.labels_complete, "intake labels are incomplete");
            let members = &node["projectItems"];
            let memberships = members["nodes"]
                .as_array()
                .context("intake memberships missing")?;
            ensure!(memberships.len() <= PAGE, "intake memberships exceed bound");
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
            let member = members_on_board.first().copied();
            if let Some(member) = member {
                let status = member.get("status").context("intake Status is missing")?;
                ensure!(
                    status.is_null() || status["name"].is_string(),
                    "intake Status is unreadable"
                );
            }
            listed.insert(facts.issue.number);
            let pending = self
                .report
                .issue_intake_pending
                .get(repo)
                .is_some_and(|p| p.contains(&facts.issue.number));
            // Only a real reopen newer than the Done value resets that field.
            // Ordinary edits, overlapping discovery and manually marking Done
            // after the reopen must not undo the operator's board decision.
            let reopened = reopened(node, member)?;
            if member.is_some()
                && !reopened
                && (!pending || member.is_some_and(|m| !m["status"].is_null()))
            {
                if pending && self.apply {
                    self.settled(repo, |p| p.remove(&facts.issue.number));
                }
                continue;
            }
            ensure!(
                existing.is_some() == member.is_some(),
                "intake membership disagrees with board"
            );
            let kept_priority = existing.and_then(|f| f.priority.as_ref());
            let priority = board.options.iter().find(|p| {
                !reopened
                    && facts.labels.iter().any(|l| {
                        l.eq_ignore_ascii_case(p)
                            || l.eq_ignore_ascii_case(&format!("priority/{p}"))
                    })
            });
            facts.priority = kept_priority.or(priority).cloned();
            facts.why = existing.and_then(|f| f.why.clone());
            // The board read's handoff evidence holds a card under way.
            facts.promotion = existing.and_then(|f| f.promotion.clone());
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
                // Keep the watermark and explicit retry ownership on a refused
                // add/field write. A lost cache never authorizes repairing a
                // pre-existing member with unset Status. Earlier failures keep
                // theirs: each is resolved where the listing reaches it.
                let owned = self.report.issue_intake_pending.get(repo);
                ensure!(
                    pending || owned.map_or(0, BTreeSet::len) < ISSUE_INTAKE_PENDING_LIMIT,
                    "{ISSUE_INTAKE_PENDING_LIMIT} failed enrollments await repair"
                );
                self.report
                    .issue_intake_pending
                    .entry(repo.to_owned())
                    .or_default()
                    .insert(facts.issue.number);
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
                self.settled(repo, |p| p.remove(&facts.issue.number));
            } else {
                for _ in 0..count {
                    self.caps.count(Kind::Write);
                }
            }
            if members_on_board.is_empty() {
                added.push(facts.issue.display());
            }
            board.items.insert(key(&facts.issue), facts);
        }
        if !more && !ahead && self.apply {
            // An open pending issue is never older than the watermark, so a
            // complete listing that lacks it shows that it was closed.
            self.settled(repo, |p| p.retain(|n| listed.contains(n)));
        }
        if more && !ahead {
            ensure!(
                last > since + time::Duration::seconds(1),
                "intake timestamp overflow; inspect the repository before advancing state"
            );
            Ok(last - time::Duration::seconds(1))
        } else {
            Ok(self.now - time::Duration::seconds(1))
        }
    }

    /// Changes `repo`'s retry ownership, which is left out of state once none.
    fn settled<R>(&mut self, repo: &str, change: impl FnOnce(&mut BTreeSet<u64>) -> R) {
        let pending = &mut self.report.issue_intake_pending;
        if let Some(owned) = pending.get_mut(repo) {
            change(owned);
            if owned.is_empty() {
                pending.remove(repo);
            }
        }
    }
}

/// The listing's bounded native timeline is evidence, not issue prose. Missing
/// evidence fails closed instead of treating an edit as a reopen.
fn reopened(node: &Value, member: Option<&Value>) -> Result<bool> {
    let Some(member) = member.filter(|m| m["status"]["name"] == "Done") else {
        return Ok(false);
    };
    let events = node["reopens"]["nodes"]
        .as_array()
        .context("intake reopen evidence missing")?;
    ensure!(events.len() <= 1, "intake reopen evidence exceeds bound");
    let Some(event) = events.first() else {
        return Ok(false);
    };
    let at = OffsetDateTime::parse(text(event, "/createdAt"), &Rfc3339)
        .context("intake reopen timestamp")?;
    let status_at = OffsetDateTime::parse(text(member, "/status/updatedAt"), &Rfc3339)
        .context("intake Done timestamp")?;
    let updated = OffsetDateTime::parse(text(node, "/updatedAt"), &Rfc3339)?;
    ensure!(at <= updated, "intake reopen is newer than issue update");
    Ok(at > status_at)
}
