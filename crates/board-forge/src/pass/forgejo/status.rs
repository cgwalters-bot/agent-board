//! Stage 5 on Forgejo (docs/forges.md#labels-and-status): each board issue's Status derived as on GitHub ([`derive`]) from its
//! labels, stage 4's runs in flight and starts and its open pull requests,
//! and written back as its one `status/` label where `board.statusLabels`
//! is set. `status/blocked` and `status/backlog` are a person's holds and
//! are never replaced; two `status/` labels, or an unknown one, keep what is
//! there.
use super::*;
use crate::forgejo::{STATUS_SCOPE, parse_status, status_label};
use crate::pass::status::{ItemFacts, Ours, derive, line};
use board_core::Status;

/// Days of closed board issues stage 5 reads, by their last update, to
/// make them Done: closing an issue updates it, so one closed while passes
/// ran is read, and one closed longer ago is left as it is.
const CLOSED_DAYS: i64 = 7;

/// A board issue as stage 5 read it.
struct Item {
    issue: ItemRef,
    labels: Vec<String>,
    facts: ItemFacts,
}

/// The Status `labels` name: `Some(None)` with no `status/` label, and none
/// where it cannot be read, with two or an unknown one.
fn labelled(labels: &[String]) -> Option<Option<Status>> {
    let named: Vec<&String> = (labels.iter())
        .filter(|l| l.starts_with(STATUS_SCOPE))
        .collect();
    match named[..] {
        [] => Some(None),
        [one] => parse_status(&one[STATUS_SCOPE.len()..]).map(Some),
        _ => None,
    }
}

/// What stage 5 knows of an issue with `labels`: open or not, its run in
/// flight, whether the runs were all listed, and its open pull requests of
/// ours and whether they were all read.
fn facts(
    labels: &[String],
    open: bool,
    (run, runs_read): (Option<String>, bool),
    (pulls, pulls_read): (Vec<String>, bool),
) -> ItemFacts {
    let status = labelled(labels);
    ItemFacts {
        complete: status.is_some(),
        status: status.flatten(),
        pull_request: false,
        moving: false,
        open,
        merged: false,
        needs_human: labels.iter().any(|l| l.eq_ignore_ascii_case(NEEDS_HUMAN)),
        run,
        runs_read,
        pulls,
        pulls_read,
    }
}

impl<C: ForgejoTransport> Schedule<'_, C> {
    /// Stage 5: derives each board issue's Status and writes each change as
    /// one `status/` label; or, past `--max-status-changes`, refuses them
    /// all. Off without `board.statusLabels`.
    pub(super) fn status(&mut self) {
        if !self.board.status_labels {
            self.line("stage 5 status: off (no board.statusLabels)".into());
            return;
        }
        let items = match self.status_items() {
            Ok(items) => items,
            Err(error) => {
                self.report.failures += 1;
                self.line(format!("status error: reading the board: {error:#}"));
                return;
            }
        };
        let mut changes = Vec::new();
        let mut kept = 0;
        for item in &items {
            kept += usize::from(!item.facts.complete);
            if matches!(item.facts.status, Some(Status::Blocked | Status::Backlog)) {
                continue;
            }
            if let Some(change) = derive(&item.facts) {
                changes.push((item, change));
            }
        }
        changes.sort_by_key(|(item, _)| {
            let at = &item.issue;
            (at.repository.to_ascii_lowercase(), at.number)
        });
        let mut summary = format!(
            "status: {} change(s) of {} item(s) read",
            changes.len(),
            items.len()
        );
        if kept > 0 {
            summary.push_str(&format!("; {kept} kept, their own facts not read whole"));
        }
        self.line(summary);
        let bound = self.caps.status_changes();
        if changes.len() > bound {
            self.report.failures += 1;
            self.line(format!(
                "status: refused, {} changes are more than --max-status-changes {bound}; nothing is written. Check them, then pass a higher cap once",
                changes.len()
            ));
            for (item, change) in &changes {
                self.line(format!("refused: {}", line(&item.issue, change)));
            }
            return;
        }
        for (item, change) in &changes {
            let said = line(&item.issue, change);
            let Some(label) = status_label(&change.to) else {
                self.report.failures += 1;
                self.line(format!("{said} failed: no status/ label names it"));
                continue;
            };
            if let Some(held) = self.caps.check(Kind::Write) {
                self.line(format!("{held}: {said}"));
                continue;
            }
            if matches!(self.mode, Mode::Preview) {
                self.caps.count(Kind::Write);
                self.line(said);
                continue;
            }
            match self.write_status(item, label) {
                Ok(true) => self.line(said),
                Ok(false) => self.line(format!(
                    "status {}: its labels changed since the board was read; left for the next pass",
                    item.issue.display()
                )),
                Err(error) => {
                    self.report.failures += 1;
                    self.line(format!("{said} failed: {error:#}"));
                }
            }
        }
    }

    /// The board's open issues, and with a membership label its closed ones
    /// updated in the last [`CLOSED_DAYS`], with what stage 5 knows of each.
    /// Stage 4's runs in flight are reused, and listed where it did not.
    fn status_items(&mut self) -> Result<Vec<Item>> {
        let busy = match self.busy.take() {
            Some(busy) => busy,
            None => match self.runs() {
                Ok((_, busy)) => Some(busy),
                Err(error) => {
                    self.report.failures += 1;
                    let why = format!("the runs in flight could not be listed: {error:#}");
                    self.line(format!("status error: {why}"));
                    None
                }
            },
        };
        if busy.is_none() {
            self.line("status: the runs in flight are unknown, so none goes back to Todo".into());
        }
        let config = self.config;
        let ours = Ours {
            operator: &config.operator,
            bot: &config.bot,
            stale_before: None,
        };
        let mut items = Vec::new();
        for (read, _) in self.board_issues(|_| true)?.into_values() {
            let pulls = match self.closing(&read.issue) {
                Ok((pulls, whole)) => {
                    let ours = (pulls.iter())
                        .filter(|p| ours.moving(p.author.as_deref(), p.updated))
                        .map(|p| format!("{}#{}", p.repository, p.number))
                        .collect();
                    (ours, whole)
                }
                Err(why) => {
                    let at = read.issue.display();
                    self.line(format!(
                        "status {at}: {why}, so it does not go back to Todo"
                    ));
                    (Vec::new(), false)
                }
            };
            let run = busy.as_ref().map(|b| b.get(&key(&read.issue)).cloned());
            let run = (run.clone().flatten(), run.is_some());
            items.push(Item {
                facts: facts(&read.labels, true, run, pulls),
                issue: read.issue,
                labels: read.labels,
            });
        }
        items.extend(self.closed()?);
        Ok(items)
    }

    /// The board's closed issues updated in the last [`CLOSED_DAYS`], read
    /// in at most [`LIST_PAGES`] pages a repository; none on a board without
    /// a membership label, whose closed issues are not on it.
    fn closed(&mut self) -> Result<Vec<Item>> {
        let board = self.board;
        let crate::forgejo::Membership::Label(member) = &board.membership else {
            return Ok(Vec::new());
        };
        let since = (self.now - time::Duration::days(CLOSED_DAYS)).format(&Rfc3339)?;
        let query = [
            ("type", "issues"),
            ("state", "closed"),
            ("labels", member.as_str()),
            ("since", since.as_str()),
        ];
        let host = instance(board);
        let mut items = Vec::new();
        for repository in &board.repositories {
            let url = api(board, repository, "issues", &query)?;
            let (issues, whole) = pages(url, LIST_PAGES, |url| listing(self.client, url))?;
            if !whole {
                self.line(format!(
                    "status: the closed issues of {repository} could not all be read in {LIST_PAGES} pages; those read are made Done"
                ));
            }
            for issue in &issues {
                let number = issue["number"].as_u64().context("issue has no number")?;
                let name = format!("{repository}#{number}");
                ensure!(issue["pull_request"].is_null(), "{name} is a pull request");
                ensure!(
                    text(issue, "/repository/full_name").eq_ignore_ascii_case(repository),
                    "{name} reported from another repository"
                );
                ensure!(
                    issue["state"] == "closed",
                    "{name} is listed closed but is not"
                );
                let labels = (issue["labels"].as_array())
                    .with_context(|| format!("{name} has no labels"))?
                    .iter()
                    .map(|l| l["name"].as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
                    .with_context(|| format!("a label of {name} has no name"))?;
                // Only on the board with its membership label.
                if !labels.contains(member) {
                    continue;
                }
                items.push(Item {
                    facts: facts(&labels, false, (None, true), (Vec::new(), true)),
                    issue: ItemRef {
                        host: host.clone(),
                        repository: repository.clone(),
                        number,
                    },
                    labels,
                });
            }
        }
        Ok(items)
    }

    /// Writes `label` to `item` through the bounded label write, which
    /// reads it back, but only while its labels are still those read: false
    /// where a person changed them since.
    fn write_status(&self, item: &Item, label: String) -> Result<bool> {
        let issue = &item.issue;
        let path = format!("issues/{}", issue.number);
        let now = (self
            .client
            .get(&api(self.board, &issue.repository, &path, &[])?)?)
        .body;
        ensure!(
            now["number"] == issue.number && now["pull_request"].is_null(),
            "{} changed identity",
            issue.display()
        );
        let labels = (now["labels"].as_array())
            .context("issue has no labels")?
            .iter()
            .map(|l| l["name"].as_str())
            .collect::<Option<BTreeSet<_>>>()
            .context("a label has no name")?;
        if labels != item.labels.iter().map(String::as_str).collect() {
            return Ok(false);
        }
        let operation = BoardOperation::Label {
            scope: STATUS_SCOPE.into(),
            name: label,
        };
        self.write_on(&self.forge(), issue, &item.labels, operation)?;
        Ok(true)
    }
}
