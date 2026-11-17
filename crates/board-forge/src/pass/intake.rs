//! Incremental issue enrollment. Discovery is not permission to implement:
//! only the existing writer-authenticated plan path can authorize that.
use super::*;
use crate::github::field_writes;

const PAGE: usize = 100;
const SHARE: usize = 15;
const LOOKBACK_DAYS: i64 = 2;
const DONE: &str = "Done";

/// Bound the census independently of the pass's request/time/write budgets.
const BACKFILL_PAGE: usize = 20;
// Enough to get through the fixed reads and one ordinary card on a small
// pass. The pass-wide time, request and write bounds still apply.
const BACKFILL_MIN_REQUESTS: usize = 20 + WRITE_RESERVE;
const CURSOR_LIMIT: usize = 1024;

/// Only pagination state. Losing it repeats reads, never authorizes a write.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueBackfill {
    pub project: String,
    pub next_repository: Option<String>,
    pub positions: BTreeMap<String, String>,
}

impl IssueBackfill {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(valid_cursor(&self.project), "invalid backfill project id");
        ensure!(
            self.positions.len() <= poll::POLL_REPOSITORY_LIMIT,
            "too many backfill repositories"
        );
        if let Some(repo) = &self.next_repository {
            poll::validate_repository(repo).context("backfill next repository")?;
        }
        for (repo, cursor) in &self.positions {
            poll::validate_repository(repo).context("backfill repository")?;
            ensure!(valid_cursor(cursor), "invalid backfill cursor");
        }
        Ok(())
    }
}

fn valid_cursor(value: &str) -> bool {
    !value.is_empty() && value.len() <= CURSOR_LIMIT && !value.chars().any(char::is_control)
}

pub(super) fn backfill_query() -> String {
    format!(
        r#"query($owner: String!, $name: String!, $cursor: String) {{
      repository(owner: $owner, name: $name) {{
        issues(first: {BACKFILL_PAGE}, after: $cursor, orderBy: {{field: CREATED_AT, direction: ASC}}, states: OPEN) {{
          edges {{ cursor node {{ id number state repository {{ nameWithOwner }} }} }}
          pageInfo {{ hasNextPage endCursor }}
        }}
      }}
    }}"#
    )
}

pub(super) const BACKFILL_FRESH: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) { id number state updatedAt repository { nameWithOwner } }
  }
}"#;

pub(super) fn backfill_identity(node: &Value, repo: &str) -> Result<(ItemRef, String)> {
    let number = node["number"]
        .as_u64()
        .context("backfill issue number missing")?;
    let issue = ItemRef {
        host: crate::github::DEFAULT_HOST.into(),
        repository: text(node, "/repository/nameWithOwner").into(),
        number,
    };
    issue.validate()?;
    ensure!(
        issue.repository.eq_ignore_ascii_case(repo),
        "backfill returned another repository"
    );
    ensure!(
        matches!(node["state"].as_str(), Some("OPEN" | "CLOSED")),
        "backfill issue state missing"
    );
    let id = text(node, "/id");
    ensure!(!id.is_empty(), "backfill issue id missing");
    Ok((issue, id.into()))
}

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
    /// Missing old issues are a visibility gap, not permission to start work.
    /// Return only this turn's fresh completion observations for stage 5.
    pub(super) fn backfill(&mut self) -> project_index::Completions {
        let bounds = self.client.bounds;
        let kept = bounds.kept.get();
        let share = bounds
            .share(SHARE)
            .max(BACKFILL_MIN_REQUESTS)
            .min(bounds.available());
        bounds
            .kept
            .set(bounds.left.get().saturating_sub(share).max(kept));
        let mut completions = BTreeMap::new();
        let result = self.backfill_within(&mut completions);
        bounds.kept.set(kept);
        match result {
            Ok(()) => {}
            Err(error) if budget_spent(&error) || error.is::<project_index::IndexMoved>() => self
                .line(format!(
                    "project backfill deferred; unprocessed positions retained: {error:#}"
                )),
            Err(error) => self
                .report
                .error("project backfill; unprocessed positions retained", &error),
        }
        completions
    }

    fn backfill_within(&mut self, completions: &mut project_index::Completions) -> Result<()> {
        let config = self.config;
        let board = config.github_board().context("no backfill board")?;
        let github = crate::github::GitHub {
            transport: Budget {
                inner: self.client.inner,
                bounds: self.client.bounds,
            },
            owner: board.owner.clone(),
            number: board.project,
        };
        let (project, _, fields) = github.schema()?;
        let schema = project_index::Schema::read(&fields)?;
        // A partial project-side read cannot establish absence. Never trust
        // issue.projectItems here: GitHub can return [] for an existing member.
        let mut members = self.index_members(&project)?;
        let repos: Vec<_> = config
            .work_repositories
            .iter()
            .map(|r| r.to_ascii_lowercase())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut position = self
            .report
            .issue_backfill
            .clone()
            .filter(|p| p.project == project)
            .unwrap_or(IssueBackfill {
                project: project.clone(),
                next_repository: None,
                positions: BTreeMap::new(),
            });
        position.positions.retain(|repo, _| repos.contains(repo));
        let repo = position
            .next_repository
            .as_ref()
            .and_then(|next| repos.iter().find(|r| *r >= next))
            .or_else(|| repos.first())
            .context("no backfill repositories")?
            .clone();
        let next = repos
            .iter()
            .position(|r| *r == repo)
            .context("backfill repository disappeared")?;
        // Give a failing repository another turn only after its neighbors;
        // its own cursor never skips the unprocessed issue.
        position.next_repository = Some(repos[(next + 1) % repos.len()].clone());
        if self.apply {
            self.report.issue_backfill = Some(position.clone());
        }
        let (owner, name) = repo.split_once('/').context("backfill repository")?;
        let cursor = position.positions.get(&repo);
        let data = self.client.graphql(
            &backfill_query(),
            json!({"owner":owner,"name":name,"cursor":cursor}),
        )?;
        let connection = &data["repository"]["issues"];
        let edges = connection["edges"]
            .as_array()
            .context("backfill edges missing")?;
        ensure!(
            edges.len() <= BACKFILL_PAGE,
            "backfill page exceeds {BACKFILL_PAGE}"
        );
        let more = connection["pageInfo"]["hasNextPage"]
            .as_bool()
            .context("backfill pageInfo missing")?;
        ensure!(
            !more || !edges.is_empty(),
            "empty nonterminal backfill page"
        );
        // Validate the whole page before its first write, including cursor
        // progress. Per-edge commits below make interrupted writes resumable.
        let mut seen = BTreeSet::new();
        let mut identities = BTreeSet::new();
        for edge in edges {
            let at = edge["cursor"].as_str().context("backfill cursor missing")?;
            ensure!(
                valid_cursor(at) && cursor.map(String::as_str) != Some(at) && seen.insert(at),
                "backfill cursor did not advance"
            );
            let (issue, _) = backfill_identity(&edge["node"], &repo)?;
            ensure!(
                edge["node"]["state"] == "OPEN",
                "backfill listing returned a closed issue"
            );
            ensure!(identities.insert(key(&issue)), "duplicate backfill issue");
        }
        if let Some(last) = edges.last() {
            ensure!(
                connection["pageInfo"]["endCursor"] == last["cursor"],
                "backfill end cursor mismatch"
            );
        }
        let mut added = 0;
        for edge in edges {
            let (issue, id) = backfill_identity(&edge["node"], &repo)?;
            let fresh = self.index_issue(&issue)?;
            ensure!(fresh.id == id, "backfill issue identity changed");
            if fresh.open {
                let mut item = members.get(&key(&issue)).cloned();
                if item.is_none() {
                    // Refresh complete project-side membership at the write
                    // boundary. A concurrent auto-add or a lost add reply must
                    // not cause a second addition on retry.
                    members = self.index_members(&project)?;
                    item = members.get(&key(&issue)).cloned();
                    if item.is_none() {
                        if self.apply {
                            self.index_recheck(&issue, &fresh)?;
                        }
                        self.index_write_slot()?;
                        if self.apply {
                            item = Some(github.add_item(&project, &id)?);
                        }
                        self.line(format!(
                            "project backfill {}: {}add membership",
                            issue.display(),
                            if self.apply { "" } else { "would " }
                        ));
                        added += 1;
                    }
                }
                match self.index_fields(
                    (&github, &project, &schema),
                    item.as_deref(),
                    &issue,
                    fresh,
                ) {
                    Ok(read) => {
                        completions.insert(key(&issue), read);
                    }
                    Err(error) if budget_spent(&error) => return Err(error),
                    // Membership is handled; the next complete sweep retries
                    // this projection. A bad thread must not strand later issues.
                    Err(error) if error.is::<project_index::IndexMoved>() => self.line(format!(
                        "project index {} deferred: {error:#}",
                        issue.display()
                    )),
                    Err(error) => self.report.error(
                        &format!("project index {}; retry on next sweep", issue.display()),
                        &error,
                    ),
                }
            }
            if self.apply {
                position.positions.insert(
                    repo.clone(),
                    edge["cursor"].as_str().context("backfill cursor")?.into(),
                );
                self.report.issue_backfill = Some(position.clone());
            }
        }
        if self.apply && !more {
            position.positions.remove(&repo);
            self.report.issue_backfill = Some(position);
        }
        self.line(format!(
            "project backfill {repo}: {} issue(s) read, {added} membership(s) {}",
            edges.len(),
            if self.apply { "added" } else { "planned" }
        ));
        Ok(())
    }

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
