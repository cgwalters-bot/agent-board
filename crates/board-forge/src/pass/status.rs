//! Stage 5 of a pass (docs/pass.md#order-and-effects): every board item's
//! Status derived from live facts and written where it differs.
//!
//! The rules are [`board_core::derive_status`], which the poll's
//! `statusReconcile` preview shares; this stage maps the pass's facts onto
//! them. Closed issues and closed or merged pull requests are Done; an item
//! labelled `needs-human` is Needs human; a run in flight, or a pull request
//! of ours that is moving ([`Ours`]), is In Progress. Anyone else's open pull
//! request is not our work under way. A derived value that no longer holds
//! goes back to Todo, but only where the absence of work is known: an item
//! whose facts were not read whole keeps its Status.
use super::*;
use crate::Forge;
use crate::github::{GitHub, field_writes};
use board_core::{Item, Status, StatusRules, StatusSignals, derive_status};

/// The pass's rules over [`derive_status`]. Done is released too: an issue
/// open again goes back to Todo like any derived value that no longer holds.
/// Blocked and Backlog are a person's holds, which only Done replaces; the
/// legacy columns (Draft, In Review, Triage) give way to any rule, and an
/// unset Status also falls back to Todo.
pub const PASS_STATUS_RULES: StatusRules<'static> = StatusRules {
    released: &[Status::InProgress, Status::NeedsHuman, Status::Done],
    kept: &[Status::Blocked, Status::Backlog],
};

/// A board item read again just before its write, by the item id the
/// whole-board read kept, to see its Status now: a person may have changed
/// it since. Not through the issue's `projectItems`, which lists nothing to
/// the bot for a repository of another owner, though the item is on the
/// board.
const ITEM_QUERY: &str = r#"query($item: ID!) {
  node(id: $item) { ... on ProjectV2Item {
    id
    project { id number owner { ... on Organization { login } ... on User { login } } }
    status: fieldValueByName(name: "Status") { ... on ProjectV2ItemFieldSingleSelectValue { name } }
  } }
}"#;

/// What stage 5 knows of one board item.
#[derive(Clone, Debug, Default)]
pub struct ItemFacts {
    pub status: Option<Status>,
    /// Its state, labels and Status were read whole.
    pub complete: bool,
    pub pull_request: bool,
    /// A pull request's: it is ours and moving ([`Ours::moving`]).
    pub moving: bool,
    pub open: bool,
    /// A pull request's: merged, rather than closed unmerged.
    pub merged: bool,
    pub needs_human: bool,
    /// A run in flight for it: its URL, or that this pass started it.
    pub run: Option<String>,
    /// Whether every run in flight was listed.
    pub runs_read: bool,
    /// An open issue's open pull requests that are ours and moving,
    /// `OWNER/REPO#N`.
    pub pulls: Vec<String>,
    /// Whether all of them are known.
    pub pulls_read: bool,
}

/// Whose open pull requests are our work under way: the operator's, and the
/// bot's until one has stood still since `stale_before` (`pr.staleDays`),
/// when it waits on someone rather than moves. Anyone else's, a dependency
/// bot's or an upstream contributor's, is not ours to show as In Progress.
#[derive(Clone, Copy, Debug)]
pub struct Ours<'a> {
    pub operator: &'a str,
    pub bot: &'a str,
    pub stale_before: Option<OffsetDateTime>,
}

impl Ours<'_> {
    /// Whether a pull request by `author`, last updated at `updated`, is
    /// ours and moving. An unknown update time is not taken for stale.
    pub fn moving(&self, author: Option<&str>, updated: Option<OffsetDateTime>) -> bool {
        let stale = self.stale_before.zip(updated).is_some_and(|(at, u)| u < at);
        is(author, &[self.operator]) || (is(author, &[self.bot]) && !stale)
    }
}

/// One Status the stage changes.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub from: Option<Status>,
    pub to: Status,
    pub reason: String,
}

/// A Status as the board names it, or `unset`.
fn name(status: Option<&Status>) -> String {
    status
        .and_then(|s| serde_json::to_value(s).ok())
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unset".into())
}

/// The change `facts` derive, or `None` to leave the item as it is.
pub fn derive(facts: &ItemFacts) -> Option<Change> {
    if !facts.complete {
        return None;
    }
    let open_pull = facts.pull_request && facts.open && facts.moving;
    let seen = facts.run.is_some() || open_pull || !facts.pulls.is_empty();
    let active = match seen {
        true => Some(true),
        false if facts.runs_read && facts.pulls_read => Some(false),
        false => None,
    };
    let signals = StatusSignals {
        terminal: !facts.open,
        human_ask: facts.needs_human,
        active,
    };
    let (_, to) = derive_status(facts.status.as_ref(), &signals, &PASS_STATUS_RULES)?;
    if facts.status.as_ref() == Some(&to) {
        return None;
    }
    let reason = match &to {
        Status::Done if facts.merged => "merged".into(),
        Status::Done => "closed".into(),
        Status::NeedsHuman => format!("labelled {NEEDS_HUMAN}"),
        Status::InProgress => match (&facts.run, &facts.pulls[..]) {
            (Some(run), _) => format!("run in flight, {run}"),
            (None, _) if open_pull => "open pull request".into(),
            (None, [one]) => format!("pull request {one} is open"),
            (None, many) => format!("pull requests {} are open", many.join(", ")),
        },
        _ => {
            let open = match facts.status {
                Some(Status::Done) => "open again, ",
                _ => "",
            };
            format!(
                "{open}not labelled {NEEDS_HUMAN}, no run in flight or pull request of ours moving"
            )
        }
    };
    Some(Change {
        from: facts.status.clone(),
        to,
        reason,
    })
}

/// A change's line, `status OWNER/REPO#N: OLD -> NEW (why)`.
fn line(item: &ItemRef, change: &Change) -> String {
    format!(
        "status {}: {} -> {} ({})",
        item.display(),
        name(change.from.as_ref()),
        name(Some(&change.to)),
        change.reason
    )
}

/// How a write went.
enum Wrote {
    Done,
    /// A person changed the Status since the board was read: what it is now.
    Changed(String),
}

impl<'a, C: Rest + Transport + crate::github::GraphQl> Scheduler<'a, C> {
    /// The board through the pass's budget, without borrowing the pass.
    fn github(&self, board: &Board) -> GitHub<Budget<'a, C>> {
        GitHub {
            transport: Budget {
                inner: self.client.inner,
                bounds: self.client.bounds,
            },
            owner: board.owner.clone(),
            number: board.project,
        }
    }

    /// Stage 5: reads the whole board, derives each item's Status from it,
    /// the runs in flight and the open issues' pull requests, and writes each
    /// change; or, past `--max-status-changes`, refuses them all.
    pub(super) async fn status(&mut self, git_ref: &str) -> Result<()> {
        let config = self.config;
        let Some(board) = config.github_board() else {
            self.line("status: off (no board)".into());
            return Ok(());
        };
        let github = self.github(board);
        let snapshot = github.snapshot().await.context("reading the whole board")?;
        // What scheduling read is reused, and what it failed to read, which
        // it reported, is not read again; what it did not reach is read here.
        let runs = "the runs in flight could not be listed";
        let busy = match self.busy.take() {
            Some(busy) => busy,
            None => {
                let listed = self.runs(&config.target(git_ref));
                self.unread(listed.map(|(_, busy)| busy), runs)
            }
        };
        if busy.is_none() {
            self.line(format!("status: {runs}, so none goes back to Todo"));
        }
        let pulls = "the open issues' pull requests could not be read";
        let open = match self.board.take() {
            Some(read) => read,
            None => {
                let read = self.board();
                self.unread(read, pulls)
            }
        };
        if open.is_none() {
            self.line(format!(
                "status: {pulls}, so no open issue goes back to Todo"
            ));
        }
        let days = config.pr.stale_days;
        let ours = Ours {
            operator: &config.operator,
            bot: &config.bot,
            stale_before: (days > 0).then(|| self.now - time::Duration::days(days as i64)),
        };
        let mut changes: Vec<(&Item, Change)> = Vec::new();
        let mut kept = 0;
        for item in &snapshot.items {
            let facts = item_facts(item, busy.as_ref(), open.as_ref(), &ours);
            kept += usize::from(!facts.complete);
            if let Some(change) = derive(&facts) {
                changes.push((item, change));
            }
        }
        changes.sort_by_key(|(item, _)| {
            let at = &item.identity;
            (at.repository.to_ascii_lowercase(), at.number)
        });
        let mut summary = format!(
            "status: {} change(s) of {} item(s) read",
            changes.len(),
            snapshot.items.len()
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
                self.line(format!("refused: {}", line(&item.identity, change)));
            }
            return Ok(());
        }
        for (n, (item, change)) in changes.iter().enumerate() {
            let said = line(&item.identity, change);
            // An applying change needs a read and a write: past the budget or
            // the time bound the rest wait for the next pass, a line for all.
            let bounds = self.client.bounds;
            if self.apply
                && (bounds.left.get() < WRITE_RESERVE + 2 || Instant::now() > bounds.admit_by)
            {
                self.report.failures += 1;
                self.line(format!(
                    "status: the pass's request budget or time is spent; {} change(s) wait for the next pass",
                    changes.len() - n
                ));
                break;
            }
            // Resolved against the board's options before anything is sent,
            // in a preview too.
            let edit = format!("Status={}", name(Some(&change.to)));
            let write = field_writes(&snapshot.fields, &[edit])
                .and_then(|writes| writes.into_iter().next().context("no Status write"));
            let write = match write {
                Ok(write) => write,
                Err(error) => {
                    self.report.failures += 1;
                    self.line(format!("{said} failed: {error:#}"));
                    continue;
                }
            };
            if let Some(held) = self.caps.check(Kind::Write) {
                self.line(format!("{held}: {said}"));
                continue;
            }
            if !self.apply {
                self.caps.count(Kind::Write);
                self.line(said);
                continue;
            }
            match self.write(&github, board, item, change, &write) {
                Ok(Wrote::Done) => self.line(said),
                Ok(Wrote::Changed(now)) => self.line(format!(
                    "status {}: changed to {now} since the board was read; left for the next pass",
                    item.identity.display()
                )),
                Err(error) => {
                    self.report.failures += 1;
                    self.line(format!("{said} failed: {error:#}"));
                }
            }
        }
        Ok(())
    }

    /// `read`'s value, or none where it failed, which is reported as `what`.
    fn unread<T>(&mut self, read: Result<T>, what: &str) -> Option<T> {
        read.map_err(|error| {
            self.report.failures += 1;
            self.line(format!("status error: {what}: {error:#}"));
        })
        .ok()
    }

    /// Writes one change: finds the item on the board, and writes only while
    /// its Status is still what the board read. The write counts against
    /// `--max-writes` once sent, whether or not its answer comes back.
    fn write(
        &self,
        github: &GitHub<Budget<'a, C>>,
        board: &Board,
        item: &Item,
        change: &Change,
        write: &crate::github::FieldWrite,
    ) -> Result<Wrote> {
        let issue = &item.identity;
        let id = item
            .project_item
            .as_deref()
            .with_context(|| format!("{}'s board item has no id", issue.display()))?;
        let data = self.client.graphql(ITEM_QUERY, json!({"item": id}))?;
        let node = &data["node"];
        ensure!(
            text(node, "/id") == id
                && node.pointer("/project/number").and_then(Value::as_u64) == Some(board.project)
                && text(node, "/project/owner/login").eq_ignore_ascii_case(&board.owner),
            "{}'s board item {id} could not be read on this board",
            issue.display()
        );
        let now = node.pointer("/status/name").and_then(Value::as_str);
        let was = name(change.from.as_ref());
        if now.unwrap_or("unset") != was {
            return Ok(Wrote::Changed(now.unwrap_or("unset").to_owned()));
        }
        let project = text(node, "/project/id");
        ensure!(!project.is_empty(), "{}'s board has no id", issue.display());
        self.caps.count(Kind::Write);
        github.set_field(project, id, write)?;
        Ok(Wrote::Done)
    }
}

/// What the snapshot, the runs in flight and the open issues' pull requests
/// say of `item`.
fn item_facts(
    item: &Item,
    busy: Option<&BTreeMap<String, String>>,
    open_issues: Option<&BoardRead>,
    ours: &Ours,
) -> ItemFacts {
    let at = key(&item.identity);
    let pull_request = item.content_kind == "pull_request";
    let open = item.state == "open";
    // A closed issue's and any pull request's own facts are all there is.
    let (pulls, pulls_read) = match (pull_request || !open, open_issues) {
        (true, _) => (vec![], true),
        (false, Some(read)) => match read.items.get(&at) {
            Some(facts) => (
                (facts.pulls.iter())
                    .filter(|p| ours.moving(p.author.as_deref(), p.updated))
                    .map(|p| format!("{}#{}", p.repository, p.number))
                    .collect(),
                facts.pulls_complete,
            ),
            // Open on the whole board, not on the open issues' read.
            None => (vec![], false),
        },
        (false, None) => (vec![], false),
    };
    ItemFacts {
        status: item.status.clone(),
        complete: item.fields_complete
            && matches!(item.content_kind.as_str(), "issue" | "pull_request"),
        pull_request,
        moving: pull_request && ours.moving(item.author.as_deref(), updated(item)),
        open,
        merged: pull_request
            && item
                .timestamps
                .get("mergedAt")
                .is_some_and(|merged| merged.is_some()),
        needs_human: item
            .labels
            .iter()
            .any(|l| l.eq_ignore_ascii_case(NEEDS_HUMAN)),
        run: busy.and_then(|b| b.get(&at).cloned()),
        runs_read: busy.is_some(),
        pulls,
        pulls_read,
    }
}

/// When `item` last changed, where the snapshot read it.
fn updated(item: &Item) -> Option<OffsetDateTime> {
    let at = item.timestamps.get("updatedAt")?.as_deref()?;
    OffsetDateTime::parse(at, &Rfc3339).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::Limits;
    use crate::pass::tests::block;
    use Status::{Backlog, Blocked, Done, Draft, InProgress, InReview, NeedsHuman, Todo};

    /// The live board read on 23 October 2026 by `pass --record`, trimmed
    /// to items that show each rule: the whole-board read, the runs in
    /// flight and the open issues' pull requests.
    const BOARD: &str = include_str!("../../../../fixtures/pass/status-2026-10-23.json");

    /// The recording's changes, one line each, in item order.
    const CHANGES: &[&str] = &[
        "status bootc-dev/bcvk#153: Done -> Todo (open again, not labelled needs-human, no run in flight or pull request of ours moving)",
        "status bootc-dev/bootc#1030: Draft -> In Progress (pull request cgwalters-forge/bootc#5 is open)",
        "status bootc-dev/bootc#2335: Needs human -> Todo (not labelled needs-human, no run in flight or pull request of ours moving)",
        "status bootc-dev/bootc#2396: Done -> Todo (open again, not labelled needs-human, no run in flight or pull request of ours moving)",
        "status bootc-dev/bootc#2423: Draft -> In Progress (pull request cgwalters-forge/bootc#2 is open)",
        "status bootc-dev/bootc#2470: Needs human -> In Progress (open pull request)",
        "status bootc-dev/bootc#2497: In Review -> In Progress (open pull request)",
        "status bootc-dev/bootc#2571: unset -> Todo (not labelled needs-human, no run in flight or pull request of ours moving)",
        "status bootc-dev/containers-image-proxy-rs#130: In Review -> In Progress (open pull request)",
        "status cgwalters-bot/homegit#58: Draft -> Done (closed)",
        "status cgwalters-bot/homegit#110: Draft -> In Progress (open pull request)",
        "status cgwalters-bot/homegit#139: Needs human -> Done (closed)",
        "status cgwalters-bot/homegit#146: Needs human -> Done (closed)",
        "status cgwalters-forge/actions#62: unset -> Todo (not labelled needs-human, no run in flight or pull request of ours moving)",
        "status cgwalters-forge/actions#105: unset -> Done (closed)",
        "status cgwalters-forge/actions#115: unset -> In Progress (pull request cgwalters-forge/actions#116 is open)",
        "status cgwalters-forge/agent-board#10: Todo -> Done (closed)",
        "status cgwalters-forge/agent-board#51: Draft -> Done (closed)",
        "status cgwalters-forge/agent-board#100: Todo -> Done (closed)",
        "status cgwalters-forge/agent-board#101: unset -> Done (closed)",
        "status cgwalters-forge/agentic-job#1286: unset -> In Progress (pull request cgwalters-forge/agentic-job#1814 is open)",
        "status cgwalters-forge/agentic-job#1623: unset -> In Progress (run in flight, https://github.com/cgwalters-forge/tracker/actions/runs/39698121265)",
        "status cgwalters-forge/bootc#45: Needs human -> In Progress (open pull request)",
        "status cgwalters-forge/tracker#11: In Review -> Done (closed)",
        "status cgwalters-forge/tracker#12: In Review -> Needs human (labelled needs-human)",
        "status cgwalters-forge/tracker#14: Draft -> Needs human (labelled needs-human)",
        "status cgwalters-forge/tracker#19: Draft -> Needs human (labelled needs-human)",
        "status cgwalters-forge/tracker#100: Needs human -> Todo (not labelled needs-human, no run in flight or pull request of ours moving)",
        "status cgwalters-forge/tracker#238: In Review -> Done (closed)",
        "status cgwalters-forge/tracker#260: Todo -> Needs human (labelled needs-human)",
        "status cgwalters-forge/tracker#263: Todo -> Needs human (labelled needs-human)",
        "status cgwalters-forge/tracker#596: Needs human -> Todo (not labelled needs-human, no run in flight or pull request of ours moving)",
        "status cgwalters-forge/tracker#674: unset -> Needs human (labelled needs-human)",
        "status cgwalters-forge/tracker#693: unset -> Needs human (labelled needs-human)",
        "status composefs/composefs-rs#348: In Review -> In Progress (pull request composefs/composefs-rs#407 is open)",
        "status ostreedev/ostree#3563: Needs human -> Todo (not labelled needs-human, no run in flight or pull request of ours moving)",
    ];

    /// The pass's configuration with scheduling off, so that this stage
    /// reads the runs and the open issues itself.
    fn config() -> Config {
        crate::pass::tests::config(json!({"wip": 0}))
    }

    /// The recording, its stuck run young, as [`crate::pass::tests::replay`]'s.
    fn replay() -> Replay {
        let mut replay = Replay::from_json(&serde_json::from_str(BOARD).unwrap()).unwrap();
        crate::pass::tests::started(&mut replay, "2026-10-23T19:20:00Z");
        crate::pass::tests::without_epics(replay)
    }

    /// The Status stage's lines of a pass.
    fn stage(report: &Report) -> Vec<&str> {
        let lines = report.lines.iter().map(String::as_str);
        lines.skip_while(|l| !l.starts_with("status")).collect()
    }

    /// A preview of the recorded board derives every rule's change and
    /// counts each as a write; past `--max-status-changes` it refuses them
    /// all, and fails.
    #[test]
    fn replay_derives_status() {
        let replay = replay();
        let preview = |limits| {
            let mode = (Mode::Preview, limits);
            block(run(
                &replay,
                &replay,
                &config(),
                State::default(),
                mode,
                replay.now,
            ))
            .unwrap()
        };
        let report = preview(Limits::default());
        let mut expected =
            vec!["status: 36 change(s) of 46 item(s) read; 2 kept, their own facts not read whole"];
        expected.extend(CHANGES);
        assert_eq!(stage(&report), expected);
        assert_eq!(report.summary, "summary: dispatched 0, writes 36, held 0");
        assert_eq!(report.failures, 0);
        let report = preview(Limits {
            status_changes: Some(35),
            ..Limits::default()
        });
        let lines = stage(&report);
        assert_eq!(
            lines[1],
            "status: refused, 36 changes are more than --max-status-changes 35; nothing is written. Check them, then pass a higher cap once"
        );
        let refused: Vec<String> = CHANGES.iter().map(|l| format!("refused: {l}")).collect();
        assert_eq!(lines[2..], refused);
        assert_eq!(report.summary, "summary: dispatched 0, writes 0, held 0");
        assert_eq!(report.failures, 1);
    }

    /// The recorded board as a forge that takes Status writes: an item's
    /// board memberships come from its recorded node, with the Status a
    /// person set since where `since` names one.
    struct Board {
        replay: Replay,
        since: BTreeMap<&'static str, &'static str>,
        writes: RefCell<Vec<Value>>,
    }

    impl Board {
        /// The recorded board items.
        fn items(&self) -> impl Iterator<Item = &Value> {
            let pages = self.replay.requests.iter().filter_map(|e| {
                e["answer"]
                    .pointer("/data/node/items/nodes")
                    .and_then(Value::as_array)
            });
            pages.flatten()
        }

        fn node(&self, repo: &str, number: &Value) -> Value {
            self.items()
                .find(|n| {
                    text(n, "/content/repository/nameWithOwner") == repo
                        && n["content"]["number"] == *number
                })
                .cloned()
                .unwrap_or_else(|| panic!("{repo}#{number} is not on the board"))
        }

        /// The recorded board item by its id.
        fn item(&self, id: &str) -> Value {
            self.items()
                .find(|n| n["id"] == id)
                .cloned()
                .unwrap_or_else(|| panic!("no board item {id}"))
        }
    }

    impl Broker for Board {
        fn usage(&self, url: &str) -> Result<Value> {
            self.replay.usage(url)
        }

        fn grant(&self, url: &str) -> Result<Value> {
            self.replay.grant(url)
        }
    }

    impl Rest for Board {
        fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
            self.replay.get(url, validator)
        }

        fn send(&self, method: &str, url: &str, _: &Value) -> Result<(u16, Value)> {
            bail!("unexpected {method} {url}")
        }
    }

    impl crate::github::GraphQl for Board {
        async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            Transport::graphql(self, query, variables)
        }
    }

    impl Transport for Board {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            if query.starts_with("mutation") {
                self.writes.borrow_mut().push(variables.clone());
                let option = &variables["value"]["singleSelectOptionId"];
                return Ok(json!({"updateProjectV2ItemFieldValue": {"projectV2Item": {
                    "id": variables["item"], "fieldValueByName": {"optionId": option}}}}));
            }
            // What the bot sees through an issue in a repository of another
            // owner: no board at all, though the item is on it.
            if query.contains("issueOrPullRequest") {
                return Ok(
                    json!({"repository": {"issueOrPullRequest": {"projectItems": {
                    "nodes": [], "totalCount": 0}}}}),
                );
            }
            if query != ITEM_QUERY {
                return self.replay.graphql(query, variables);
            }
            let node = self.item(text(&variables, "/item"));
            let content = &node["content"];
            let at = format!(
                "{}#{}",
                text(content, "/repository/nameWithOwner"),
                content["number"]
            );
            let values = node["fieldValues"]["nodes"].as_array().unwrap();
            let recorded = values
                .iter()
                .find(|v| text(v, "/field/name") == "Status")
                .map(|v| json!({"name": v["name"]}));
            let status = match self.since.get(at.as_str()) {
                Some(name) => json!({"name": name}),
                None => recorded.unwrap_or(Value::Null),
            };
            let project = json!({"id": "PVT_kwDOE9oHIs4BlJLc", "number": 1,
                "owner": {"login": "cgwalters-forge"}});
            Ok(json!({"node": {"id": node["id"], "project": project, "status": status}}))
        }
    }

    /// An applying pass writes the first changes `--max-writes` allows and
    /// holds the rest, a line each; a Status a person changed since the
    /// board was read is left, and takes no write.
    #[test]
    fn caps_hold_status_writes_past_max_writes() {
        let board = Board {
            replay: replay(),
            since: BTreeMap::from([("bootc-dev/bootc#1030", "Todo")]),
            writes: RefCell::new(vec![]),
        };
        let limits = Limits {
            dispatch: Some(0),
            writes: Some(2),
            ..Limits::default()
        };
        let mode = (Mode::Local, limits);
        let config = config();
        let now = board.replay.now;
        let report = block(run(&board, &board, &config, State::default(), mode, now)).unwrap();
        let mut expected = vec![
            "status: 36 change(s) of 46 item(s) read; 2 kept, their own facts not read whole",
            CHANGES[0],
            "status bootc-dev/bootc#1030: changed to Todo since the board was read; left for the next pass",
            CHANGES[2],
        ];
        let held: Vec<String> = CHANGES[3..]
            .iter()
            .map(|l| format!("held by --max-writes: {l}"))
            .collect();
        expected.extend(held.iter().map(String::as_str));
        assert_eq!(stage(&report), expected);
        assert_eq!(
            report.summary,
            "summary: dispatched 0/0, writes 2/2, held 33"
        );
        assert_eq!(report.failures, 0);
        let written: Vec<(String, String)> = board
            .writes
            .borrow()
            .iter()
            .map(|w| {
                (
                    text(w, "/item").into(),
                    text(w, "/value/singleSelectOptionId").into(),
                )
            })
            .collect();
        let item = |repo: &str, n: u64| text(&board.node(repo, &n.into()), "/id").to_owned();
        let todo = option("Todo");
        assert_eq!(
            written,
            [
                (item("bootc-dev/bcvk", 153), todo.clone()),
                (item("bootc-dev/bootc", 2335), todo),
            ]
        );
    }

    /// An item in a repository of another owner, whose issue lists no board
    /// to the bot, is still re-read and written, by its board item's id.
    #[test]
    fn status_writes_reach_items_of_other_owners() {
        let board = Board {
            replay: replay(),
            since: BTreeMap::new(),
            writes: RefCell::new(vec![]),
        };
        let limits = Limits {
            dispatch: Some(0),
            writes: Some(3),
            ..Limits::default()
        };
        let config = config();
        let now = board.replay.now;
        let report = block(run(
            &board,
            &board,
            &config,
            State::default(),
            (Mode::Local, limits),
            now,
        ))
        .unwrap();
        assert_eq!(stage(&report)[1..4], CHANGES[..3]);
        assert_eq!(report.failures, 0, "{:#?}", report.lines);
        let written: Vec<String> = board
            .writes
            .borrow()
            .iter()
            .map(|w| text(w, "/item").to_owned())
            .collect();
        let item = |n: u64| {
            text(
                &board.node(
                    if n == 153 {
                        "bootc-dev/bcvk"
                    } else {
                        "bootc-dev/bootc"
                    },
                    &n.into(),
                ),
                "/id",
            )
            .to_owned()
        };
        assert_eq!(written, [item(153), item(1030), item(2335)]);
        assert!(
            written.iter().all(|id| id.starts_with("PVTI_")),
            "{written:?}"
        );
    }

    /// Past its request budget an applying pass stops writing Status, and
    /// says how many changes wait for the next pass, rather than fail each.
    #[test]
    fn status_writes_stop_at_the_request_budget() {
        let board = Board {
            replay: replay(),
            since: BTreeMap::new(),
            writes: RefCell::new(vec![]),
        };
        // The recording's 16 reads and stage 3's one, then two changes'
        // read and write each.
        let bounds = Bounds::new(
            17 + WRITE_RESERVE + 4,
            Instant::now() + Duration::from_secs(60),
        );
        let state = (
            State::default(),
            Mode::Local,
            Limits::default(),
            board.replay.now,
        );
        let report = block(run_within(&board, &board, &config(), state, &bounds)).unwrap();
        let lines = stage(&report);
        assert_eq!(lines[1..3], CHANGES[..2]);
        assert_eq!(
            lines[3..],
            [
                "status: the pass's request budget or time is spent; 34 change(s) wait for the next pass"
            ]
        );
        assert_eq!(board.writes.borrow().len(), 2);
        assert_eq!(report.failures, 1);
    }

    /// The recorded Status option's id.
    fn option(name: &str) -> String {
        let replay = replay();
        let fields = replay
            .requests
            .iter()
            .find_map(|e| e["answer"].pointer("/data/node/fields/nodes"))
            .unwrap();
        let status = fields
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == "Status")
            .unwrap();
        let options = status["options"].as_array().unwrap();
        let option = options.iter().find(|o| o["name"] == name).unwrap();
        text(option, "/id").to_owned()
    }

    /// What each combination of facts makes of an item's Status.
    #[test]
    fn derive_table() {
        // Facts read whole, of an open issue with nothing under way.
        let base = ItemFacts {
            complete: true,
            open: true,
            runs_read: true,
            pulls_read: true,
            ..ItemFacts::default()
        };
        // The case, the current Status, the facts, and the Status and
        // reason derived.
        type Case = (
            &'static str,
            Option<Status>,
            fn(&mut ItemFacts),
            Option<(Status, &'static str)>,
        );
        let cases: Vec<Case> = vec![
            (
                "closed issue",
                Some(Todo),
                |f| f.open = false,
                Some((Done, "closed")),
            ),
            (
                "closed unset",
                None,
                |f| f.open = false,
                Some((Done, "closed")),
            ),
            (
                "merged pull request",
                Some(InReview),
                |f| {
                    (f.pull_request, f.open, f.merged) = (true, false, true);
                },
                Some((Done, "merged")),
            ),
            (
                "closed pull request",
                Some(Draft),
                |f| (f.pull_request, f.open) = (true, false),
                Some((Done, "closed")),
            ),
            (
                "closed and Done already",
                Some(Done),
                |f| f.open = false,
                None,
            ),
            (
                "Done wins over needs-human",
                Some(NeedsHuman),
                |f| (f.open, f.needs_human) = (false, true),
                Some((Done, "closed")),
            ),
            (
                "Done wins over Blocked",
                Some(Blocked),
                |f| f.open = false,
                Some((Done, "closed")),
            ),
            (
                "needs-human",
                Some(Todo),
                |f| f.needs_human = true,
                Some((NeedsHuman, "labelled needs-human")),
            ),
            (
                "needs-human wins over a run",
                Some(InProgress),
                |f| {
                    f.needs_human = true;
                    f.run = Some("https://github.com/o/r/actions/runs/1".into());
                },
                Some((NeedsHuman, "labelled needs-human")),
            ),
            (
                "needs-human already",
                Some(NeedsHuman),
                |f| f.needs_human = true,
                None,
            ),
            (
                "run in flight",
                Some(Todo),
                |f| f.run = Some("https://github.com/o/r/actions/runs/1".into()),
                Some((
                    InProgress,
                    "run in flight, https://github.com/o/r/actions/runs/1",
                )),
            ),
            (
                "open pull request for it",
                None,
                |f| f.pulls = vec!["o/r#2".into()],
                Some((InProgress, "pull request o/r#2 is open")),
            ),
            (
                "open pull requests for it",
                Some(Draft),
                |f| f.pulls = vec!["o/r#2".into(), "x/y#3".into()],
                Some((InProgress, "pull requests o/r#2, x/y#3 are open")),
            ),
            (
                "an open pull request item of ours, In Review",
                Some(InReview),
                |f| (f.pull_request, f.moving) = (true, true),
                Some((InProgress, "open pull request")),
            ),
            (
                "an open pull request item not ours or not moving",
                Some(InProgress),
                |f| f.pull_request = true,
                Some((
                    Todo,
                    "not labelled needs-human, no run in flight or pull request of ours moving",
                )),
            ),
            (
                "In Progress already",
                Some(InProgress),
                |f| f.pulls = vec!["o/r#2".into()],
                None,
            ),
            (
                "In Progress with nothing under way",
                Some(InProgress),
                |_| {},
                Some((
                    Todo,
                    "not labelled needs-human, no run in flight or pull request of ours moving",
                )),
            ),
            (
                "Needs human, unlabelled",
                Some(NeedsHuman),
                |_| {},
                Some((
                    Todo,
                    "not labelled needs-human, no run in flight or pull request of ours moving",
                )),
            ),
            (
                "Done, open again",
                Some(Done),
                |_| {},
                Some((
                    Todo,
                    "open again, not labelled needs-human, no run in flight or pull request of ours moving",
                )),
            ),
            ("a person's Todo stays", Some(Todo), |_| {}, None),
            ("a person's Backlog stays", Some(Backlog), |_| {}, None),
            ("Draft stays", Some(Draft), |_| {}, None),
            ("In Review stays", Some(InReview), |_| {}, None),
            (
                "unset with nothing under way",
                None,
                |_| {},
                Some((
                    Todo,
                    "not labelled needs-human, no run in flight or pull request of ours moving",
                )),
            ),
            (
                "unlisted runs: unset stays",
                None,
                |f| f.runs_read = false,
                None,
            ),
            (
                "Blocked stays with a run",
                Some(Blocked),
                |f| f.run = Some("https://github.com/o/r/actions/runs/1".into()),
                None,
            ),
            (
                "Blocked stays with an open pull request",
                Some(Blocked),
                |f| f.pulls = vec!["o/r#2".into()],
                None,
            ),
            (
                "Blocked stays labelled",
                Some(Blocked),
                |f| f.needs_human = true,
                None,
            ),
            (
                "Blocked stays with nothing under way",
                Some(Blocked),
                |_| {},
                None,
            ),
            (
                "Backlog stays labelled",
                Some(Backlog),
                |f| f.needs_human = true,
                None,
            ),
            (
                "Backlog stays with an open pull request item",
                Some(Backlog),
                |f| (f.pull_request, f.moving) = (true, true),
                None,
            ),
            (
                "Backlog stays with a run",
                Some(Backlog),
                |f| f.run = Some("https://github.com/o/r/actions/runs/1".into()),
                None,
            ),
            (
                "Done wins over Backlog",
                Some(Backlog),
                |f| (f.pull_request, f.open, f.merged) = (true, false, true),
                Some((Done, "merged")),
            ),
            (
                "unlisted runs: In Progress stays",
                Some(InProgress),
                |f| f.runs_read = false,
                None,
            ),
            (
                "unread pull requests: In Progress stays",
                Some(InProgress),
                |f| f.pulls_read = false,
                None,
            ),
            (
                "unlisted runs, but an open pull request",
                Some(Todo),
                |f| {
                    f.runs_read = false;
                    f.pulls = vec!["o/r#2".into()];
                },
                Some((InProgress, "pull request o/r#2 is open")),
            ),
            (
                "facts not read whole: even closed stays",
                Some(Todo),
                |f| (f.complete, f.open) = (false, false),
                None,
            ),
        ];
        for (case, status, edit, expected) in cases {
            let mut facts = ItemFacts {
                status: status.clone(),
                ..base.clone()
            };
            edit(&mut facts);
            let got = derive(&facts).map(|c| {
                assert_eq!(c.from, status, "{case}");
                (c.to, c.reason)
            });
            let expected = expected.map(|(s, r)| (s, r.to_owned()));
            assert_eq!(got, expected, "{case}");
        }
    }

    /// Whose open pull request makes an item In Progress: the bot's and the
    /// operator's, as the item itself or as one that closes an issue; not a
    /// dependency bot's, not an upstream contributor's, and not the bot's
    /// once it has stood still past `pr.staleDays` with no run in flight.
    #[test]
    fn in_progress_is_our_moving_work() {
        let now = OffsetDateTime::parse("2026-10-30T12:00:00Z", &Rfc3339).unwrap();
        let ours = Ours {
            operator: "op",
            bot: "bot",
            stale_before: Some(now - time::Duration::days(2)),
        };
        let (fresh, stale) = ("2026-10-30T00:00:00Z", "2026-10-20T00:00:00Z");
        let run = "https://github.com/o/t/actions/runs/1";
        // The case; the board item (a pull request, else an issue an open
        // pull request closes); that pull request's author and last update;
        // whether a run is in flight; and the Status an In Progress item
        // takes, `None` to keep it.
        type Case = (
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            bool,
            Option<Status>,
        );
        let cases: [Case; 12] = [
            (
                "the bot's pull request",
                "pull_request",
                "bot",
                fresh,
                false,
                None,
            ),
            (
                "the operator's pull request",
                "pull_request",
                "OP",
                fresh,
                false,
                None,
            ),
            (
                "a dependabot pull request",
                "pull_request",
                "dependabot[bot]",
                fresh,
                false,
                Some(Todo),
            ),
            (
                "someone else's pull request",
                "pull_request",
                "visitor",
                fresh,
                false,
                Some(Todo),
            ),
            (
                "the bot's stale pull request",
                "pull_request",
                "bot",
                stale,
                false,
                Some(Todo),
            ),
            (
                "the bot's stale pull request, a run in flight",
                "pull_request",
                "bot",
                stale,
                true,
                None,
            ),
            (
                "the operator's pull request, however old",
                "pull_request",
                "op",
                stale,
                false,
                None,
            ),
            (
                "an issue the bot's pull request closes",
                "issue",
                "bot",
                fresh,
                false,
                None,
            ),
            (
                "an issue the operator's pull request closes",
                "issue",
                "op",
                fresh,
                false,
                None,
            ),
            (
                "an issue a dependabot pull request closes",
                "issue",
                "dependabot[bot]",
                fresh,
                false,
                Some(Todo),
            ),
            (
                "an upstream issue someone else's pull request closes",
                "issue",
                "visitor",
                fresh,
                false,
                Some(Todo),
            ),
            (
                "an issue the bot's stale pull request closes",
                "issue",
                "bot",
                stale,
                false,
                Some(Todo),
            ),
        ];
        for (case, kind, author, updated, running, expected) in cases {
            let at = ItemRef {
                host: DEFAULT_HOST.into(),
                repository: "up/stream".into(),
                number: 7,
            };
            let item: Item = serde_json::from_value(json!({
                "identity": at, "title": case, "state": "open", "status": "In Progress",
                "priority": null, "turn": null, "fields_complete": true,
                "content_kind": kind, "author": (kind == "pull_request").then_some(author),
                "timestamps": {"updatedAt": updated},
            }))
            .unwrap();
            let mut issue = crate::pass::tests::facts("up/stream", 7, fresh);
            issue.pulls = vec![ClosingPull {
                number: 8,
                repository: "up/stream".into(),
                author: Some(author.into()),
                updated: OffsetDateTime::parse(updated, &Rfc3339).ok(),
            }];
            let read = BoardRead {
                options: vec![],
                items: BTreeMap::from([(key(&at), issue)]),
            };
            let busy = BTreeMap::from_iter(running.then(|| (key(&at), run.to_owned())));
            let facts = item_facts(&item, Some(&busy), Some(&read), &ours);
            let got = derive(&facts).map(|c| c.to);
            assert_eq!(got, expected, "{case}");
        }
    }
}
