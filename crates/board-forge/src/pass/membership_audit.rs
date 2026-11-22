//! Report membership gaps without creating cards. An add receipt need not
//! prove who created a card, and even an unset card can affect subsequent
//! intake and Status reconciliation. This census supplies no input to either.

use super::*;

const PAGE: usize = 20;
const CURSOR_LIMIT: usize = 1024;
const INVALID_CURSOR: &str = "does not appear to be a valid cursor";

/// Page positions for a read-only census, scoped to the observed project URL.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipAudit {
    project: String,
    next_repository: Option<String>,
    positions: BTreeMap<String, String>,
}

impl MembershipAudit {
    pub(crate) fn validate(&self) -> Result<()> {
        let url = self
            .project
            .parse()
            .context("invalid membership audit project URL")?;
        crate::github::validate_github_project(&url)?;
        ensure!(
            self.positions.len() <= POLL_REPOSITORY_LIMIT,
            "too many membership audit positions"
        );
        if let Some(repo) = &self.next_repository {
            poll::validate_repository(repo).context("membership audit next repository")?;
        }
        for (repo, cursor) in &self.positions {
            poll::validate_repository(repo).context("membership audit repository")?;
            ensure!(valid_cursor(cursor), "invalid membership audit cursor");
        }
        Ok(())
    }
}

fn valid_cursor(cursor: &str) -> bool {
    !cursor.is_empty() && cursor.len() <= CURSOR_LIMIT && !cursor.chars().any(char::is_control)
}

pub(super) struct Membership {
    project: String,
    issues: BTreeSet<String>,
}

impl Membership {
    pub(super) fn from_snapshot(board: &Board, snapshot: &board_core::Snapshot) -> Option<Self> {
        // The snapshot reader validates the URL shape. Also match the selected
        // board, so no other project's snapshot can establish absence here.
        let mut path = snapshot.project.path_segments()?;
        let owner = path.nth(1)?;
        let project = path.nth(1)?.parse::<u64>().ok()?;
        if !owner.eq_ignore_ascii_case(&board.owner) || project != board.project {
            return None;
        }
        Some(Self {
            project: snapshot.project.to_string(),
            issues: snapshot
                .items
                .iter()
                .filter(|item| item.content_kind == "issue")
                .map(|item| key(&item.identity))
                .collect(),
        })
    }
}

pub(super) fn query() -> String {
    format!(
        r#"query($owner: String!, $name: String!, $cursor: String) {{
      repository(owner: $owner, name: $name) {{
        nameWithOwner
        issues(first: {PAGE}, after: $cursor, orderBy: {{field: CREATED_AT, direction: ASC}}, states: OPEN) {{
          nodes {{ number state }}
          pageInfo {{ hasNextPage endCursor }}
        }}
      }}
    }}"#
    )
}

fn page(
    data: &Value,
    repo: &str,
    previous: Option<&str>,
) -> Result<(BTreeSet<u64>, Option<String>)> {
    let repository = &data["repository"];
    ensure!(
        text(repository, "/nameWithOwner").eq_ignore_ascii_case(repo),
        "membership audit returned a different or missing repository"
    );
    let connection = &repository["issues"];
    let nodes = connection["nodes"]
        .as_array()
        .context("membership audit nodes missing")?;
    ensure!(nodes.len() <= PAGE, "membership audit page exceeds {PAGE}");
    let more = connection["pageInfo"]["hasNextPage"]
        .as_bool()
        .context("membership audit pageInfo missing")?;
    let cursor = if more {
        let at = connection["pageInfo"]["endCursor"]
            .as_str()
            .context("membership audit cursor missing")?;
        ensure!(
            !nodes.is_empty() && valid_cursor(at) && previous != Some(at),
            "membership audit cursor did not advance"
        );
        Some(at.to_owned())
    } else {
        None
    };
    let mut issues = BTreeSet::new();
    for node in nodes {
        ensure!(
            node["state"] == "OPEN",
            "membership audit returned an issue not known open"
        );
        let number = node["number"]
            .as_u64()
            .filter(|n| *n > 0)
            .context("invalid membership audit issue number")?;
        ensure!(issues.insert(number), "duplicate membership audit issue");
    }
    Ok((issues, cursor))
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    pub(super) fn audit_membership(&mut self) {
        let Some(membership) = self.audit_membership.take() else {
            self.line("project membership audit deferred: no complete board snapshot".into());
            return;
        };
        self.client.bounds.enter(Stage::Status);
        match self.audit_page(&membership) {
            Ok(()) => {}
            Err(error) if budget_spent(&error) => {
                self.line("project membership audit deferred: budget".into())
            }
            Err(error) => self.report.error("project membership audit", &error),
        }
    }

    fn audit_page(&mut self, membership: &Membership) -> Result<()> {
        let repos: Vec<_> = self
            .config
            .work_repositories
            .iter()
            .map(|repo| repo.to_ascii_lowercase())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut position = self
            .report
            .membership_audit
            .clone()
            .filter(|position| position.project == membership.project)
            .unwrap_or_else(|| MembershipAudit {
                project: membership.project.clone(),
                next_repository: None,
                positions: BTreeMap::new(),
            });
        position.positions.retain(|repo, _| repos.contains(repo));
        let index = position
            .next_repository
            .as_ref()
            .and_then(|next| repos.iter().position(|repo| repo >= next))
            .unwrap_or(0);
        let repo = repos
            .get(index)
            .context("no membership audit repositories")?;
        position.next_repository = Some(repos[(index + 1) % repos.len()].clone());
        let (owner, name) = repo
            .split_once('/')
            .context("invalid membership audit repository")?;
        let previous = position.positions.get(repo).map(String::as_str);
        let result = self.client.graphql(
            &query(),
            json!({"owner":owner,"name":name,"cursor":previous}),
        );
        if result.as_ref().is_err_and(budget_spent) {
            return result.map(|_| ());
        }
        // Let the other repositories take a turn after a failed read, but do
        // not advance the failed repository's cursor.
        if self.apply {
            self.report.membership_audit = Some(position.clone());
        }
        let (issues, cursor) = match result {
            Err(error) if previous.is_some() && format!("{error:#}").contains(INVALID_CURSOR) => {
                self.line(format!("project membership audit {repo}: saved cursor refused; restarting this repository"));
                if self.apply {
                    position.positions.remove(repo);
                    self.report.membership_audit = Some(position);
                }
                return Ok(());
            }
            result => page(&result?, repo, previous)?,
        };
        let mut missing = 0;
        for number in &issues {
            let issue = format!("{repo}#{number}");
            if !membership.issues.contains(&issue) {
                self.line(format!(
                    "project membership audit {issue}: absent from board snapshot"
                ));
                missing += 1;
            }
        }
        self.line(format!(
            "project membership audit {repo}: {missing} absent of {} open issue(s) read",
            issues.len()
        ));
        if self.apply {
            if let Some(cursor) = cursor {
                position.positions.insert(repo.clone(), cursor);
            } else {
                position.positions.remove(repo);
            }
            self.report.membership_audit = Some(position);
        }
        Ok(())
    }
}
