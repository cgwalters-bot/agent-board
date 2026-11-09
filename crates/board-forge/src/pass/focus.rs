//! The epic focus (`holds.epicFocus`): each open P0 epic's progress, the
//! agent runs on it and its children, and what blocks each open child, as
//! typed facts for the review app's Focus view (review#298). It is built
//! from what the pass already read, with no read of its own: stage 3's
//! listing of the epics and their sub-issues (with titles and last changes,
//! which `holds.epicFocus` adds to that listing's queries), the runs in
//! flight, stage 2's pull request steps, stage 4's board and the hold
//! snapshot. It schedules nothing and grants nothing.
//!
//! What the pass did not read is said, not filled in: an epic it could not
//! read counts in `unread_epics`, a source it did not reach is `unread`, and
//! a child it has no fact about is blocked by `unknown`, never by nothing.

use super::epics::{Detail, EpicFacts, Listing};
use super::holds::Published;
use super::*;

pub const SCHEMA: &str = "agent-board/epic-focus/v1";
/// The board Priority whose epics are the focus.
const PRIORITY: &str = "P0";
/// Epics listed, the rest counted in `omitted`.
const MAX_EPICS: usize = 10;
/// Children listed an epic, the open first, the rest counted in its
/// `omitted`; its `progress` counts them all.
const MAX_CHILDREN: usize = 50;
/// Task lines read from an epic's body.
const MAX_TASKS: usize = 100;
/// Runs listed an epic, the rest counted in its `runs_omitted`.
const MAX_RUNS: usize = 8;
const TITLE_CHARS: usize = 120;
const GOAL_CHARS: usize = 240;
const REASON_CHARS: usize = 240;

/// Stage 3's listing as the focus keeps it: the P0 epics read whole, and
/// how many it could not read, whose Priority is so unknown.
#[derive(Debug, Default)]
pub(super) struct Read {
    epics: Vec<EpicFacts>,
    unread: usize,
}

impl Read {
    pub(super) fn new(listed: &Listing) -> Self {
        let mut read = Self::default();
        for facts in listed {
            match facts {
                Ok(facts) if facts.priority.as_deref() == Some(PRIORITY) => {
                    read.epics.push(facts.clone());
                }
                Ok(_) => {}
                Err(_) => read.unread += 1,
            }
        }
        read
    }
}

/// The `OWNER/REPO#N` keys of the open board `items` with nothing under way
/// that stage 4 reads: no person's hold, open dependency or open pull
/// request that closes it, each read whole. A run in flight is matched
/// apart, against the runs listed.
pub(super) fn quiet<'f>(
    items: impl Iterator<Item = &'f Facts>,
    people: &[&str],
) -> BTreeSet<String> {
    let busy = BTreeMap::new();
    (items.filter(|f| hold(f, &busy, people).is_none()))
        .map(|f| key(&f.issue))
        .collect()
}

/// What the focus is made of, as the pass read it this pass.
pub(super) struct Sources<'s> {
    pub read: Option<&'s Read>,
    /// The published holds ([`holds::Snapshot::holds`]), carried ones included.
    pub holds: &'s [Published],
    /// The runs in flight, where they were listed, and those this pass started.
    pub flights: Option<&'s [view::Run]>,
    pub started: &'s [view::Run],
    /// Stage 2's step on each followed pull request.
    pub steps: &'s [poll::Stepped],
    pub quiet: Option<&'s BTreeSet<String>>,
}

/// `text` on one line, without control characters, at most `chars` long;
/// none where nothing is left.
fn clip(text: &str, chars: usize) -> Option<String> {
    let line: String = (text.chars())
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let clipped: String = line.chars().take(chars).collect();
    (!clipped.is_empty()).then_some(clipped)
}

/// The first sentence of `body`'s first rendered paragraph, headings and
/// task lines aside.
fn goal(body: &str) -> Option<String> {
    let mut paragraph = Vec::new();
    for line in crate::review::rendered_lines(body) {
        let line = line.trim();
        let skipped = line.starts_with('#') || task(line).is_some();
        match (line.is_empty() || skipped, paragraph.is_empty()) {
            (true, true) => continue,
            (true, false) => break,
            (false, _) => paragraph.push(line),
        }
    }
    let text = paragraph.join(" ");
    let end = (text.match_indices(['.', '!', '?']))
        .map(|(at, _)| at + 1)
        .find(|&at| text[at..].is_empty() || text[at..].starts_with(' '));
    clip(&text[..end.unwrap_or(text.len())], GOAL_CHARS)
}

/// A task list line's box, ticked or not, and its text.
fn task(line: &str) -> Option<(bool, &str)> {
    let line = line.trim_start();
    let rest = (line.strip_prefix("- ")).or_else(|| line.strip_prefix("* "))?;
    let (done, text) = match rest.get(..4)? {
        "[ ] " => (false, &rest[4..]),
        "[x] " | "[X] " => (true, &rest[4..]),
        _ => return None,
    };
    Some((done, text.trim()))
}

/// The item a task's text starts with, `OWNER/REPO#N`, written as such, as
/// `#N` of the epic's `repository`, or as its GitHub issue or pull request
/// URL; and the text after it.
fn task_item<'t>(text: &'t str, repository: &str) -> Option<(String, &'t str)> {
    let (first, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let first = first.trim_end_matches(['.', ',', ':', ';', ')']);
    let item = if let Some(number) = first.strip_prefix('#') {
        format!("{repository}#{number}")
    } else if let Some(path) = first.strip_prefix("https://github.com/") {
        let mut parts = path.splitn(4, '/');
        let (owner, repo, kind, number) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if !matches!(kind, "issues" | "pull") {
            return None;
        }
        format!("{owner}/{repo}#{number}")
    } else {
        first.to_owned()
    };
    let (repo, number) = crate::changes::parse_ref(&item)?;
    let rest = rest.trim().trim_start_matches(['-', ':']).trim();
    Some((format!("{repo}#{number}"), rest))
}

/// One child of an epic: a sub-issue, a task of its body, or both.
#[derive(Debug)]
struct Child {
    /// `OWNER/REPO#N` as read; none for a task naming no item.
    item: Option<String>,
    title: Option<String>,
    source: &'static str,
    done: bool,
    updated: Option<OffsetDateTime>,
}

impl Child {
    fn key(&self) -> Option<String> {
        self.item.as_ref().map(|i| i.to_ascii_lowercase())
    }
}

/// `epic`'s children once each: its sub-issues, done once closed, then its
/// body's tasks, done once ticked. A task naming a sub-issue is that
/// sub-issue, whose state counts; tasks alike are one.
fn children(epic: &EpicFacts) -> Vec<Child> {
    let mut out: Vec<Child> = Vec::new();
    let census = &epic.census;
    let subs = (census.sub_issues.iter().map(|s| (s, false)))
        .chain(census.closed.iter().map(|s| (s, true)));
    for (sub, done) in subs {
        let item = format!("{}#{}", sub.repo, sub.number);
        let Detail { title, updated } = (epic.sub_details.get(&item.to_ascii_lowercase()))
            .cloned()
            .unwrap_or_default();
        out.push(Child {
            item: Some(item),
            title,
            source: "sub-issue",
            done,
            updated,
        });
    }
    let lines = crate::review::rendered_lines(&epic.body);
    let tasks = lines.into_iter().filter_map(task).take(MAX_TASKS);
    for (done, text) in tasks {
        let (item, title) = match task_item(text, &epic.issue.repository) {
            Some((item, rest)) => (Some(item), rest),
            None => (None, text),
        };
        let title = clip(title, TITLE_CHARS);
        let lower = item.as_ref().map(|i| i.to_ascii_lowercase());
        let same = out.iter_mut().find(|c| match &lower {
            Some(key) => c.key().as_ref() == Some(key),
            None => c.item.is_none() && c.title == title,
        });
        match same {
            Some(child) if child.source == "sub-issue" => child.source = "both",
            Some(_) => {}
            None if item.is_none() && title.is_none() => {}
            None => out.push(Child {
                item,
                title,
                source: "task",
                done,
                updated: None,
            }),
        }
    }
    out
}

/// A child's or epic's blocker kinds, as the reader takes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Blocker {
    /// A hold that waits on the operator: its question, or its reason.
    Operator,
    /// Its pull request is open and waits on review or merge.
    Review,
    /// Its pull request has a failing check, as stage 2 decided.
    Ci,
    /// Another hold of the pass, with its kind and reason.
    Hold,
    /// Stage 4 read it on the board with nothing under way: no hold, no open
    /// pull request, no open dependency, and no run listed on it.
    NotStarted,
    /// The pass has no fact about it.
    Unknown,
}

impl Blocker {
    fn name(self) -> &'static str {
        match self {
            Blocker::Operator => "operator",
            Blocker::Review => "review",
            Blocker::Ci => "ci",
            Blocker::Hold => "hold",
            Blocker::NotStarted => "not-started",
            Blocker::Unknown => "unknown",
        }
    }
}

/// The holds that are an item's own, by key: not stall alarms, blocker
/// cards or overlaps let through, which hold nothing.
fn holds_by_item(holds: &[Published]) -> BTreeMap<String, Vec<&Published>> {
    let mut out: BTreeMap<String, Vec<&Published>> = BTreeMap::new();
    for hold in holds {
        let own = !matches!(
            hold.kind,
            HoldKind::Stalled | HoldKind::Blocking | HoldKind::OverlapIgnored
        );
        if let Some(item) = hold.item.as_ref().filter(|_| own) {
            out.entry(item.to_ascii_lowercase()).or_default().push(hold);
        }
    }
    out
}

/// The pull request a `pr-open` hold's reason names.
fn held_pull(reason: &str) -> Option<&str> {
    let [before, after] = PR_OPEN;
    let pull = reason.strip_prefix(before)?.strip_suffix(after)?;
    crate::changes::parse_ref(pull).map(|_| pull)
}

/// Whether stage 2's step on `pull` says a check of it failed.
fn failing(steps: &[poll::Stepped], pull: &str) -> bool {
    let [before, after] = poll::FAILED_CHECK;
    let step = (steps.iter())
        .find(|s| format!("{}#{}", s.repository, s.number).eq_ignore_ascii_case(pull));
    step.is_some_and(|s| {
        (s.reason.split([',', ';']))
            .map(str::trim)
            .any(|part| part.starts_with(before) && part.ends_with(after))
    })
}

/// What blocks the open item `key`, or none where a run is on it: its own
/// hold, the operator's first, else whether stage 4 saw it with nothing
/// under way (`quiet`, which an epic is not asked), else unknown.
fn blocker(
    key: &str,
    holds: &BTreeMap<String, Vec<&Published>>,
    sources: &Sources,
    running: bool,
    quiet: bool,
) -> Option<Value> {
    if running {
        return None;
    }
    let held = holds.get(key).and_then(|h| {
        let operator = h.iter().find(|h| h.operator);
        operator.or_else(|| h.first()).copied()
    });
    let Some(hold) = held else {
        let kind = match sources.quiet {
            Some(read) if quiet && read.contains(key) => Blocker::NotStarted,
            _ => Blocker::Unknown,
        };
        return Some(json!({"type": kind.name(), "kind": null, "reason": null,
            "since": null, "pull": null}));
    };
    let pull = (hold.kind == HoldKind::PrOpen)
        .then(|| held_pull(&hold.reason))
        .flatten();
    let kind = match pull {
        _ if hold.operator => Blocker::Operator,
        Some(pull) if failing(sources.steps, pull) => Blocker::Ci,
        Some(_) => Blocker::Review,
        None => Blocker::Hold,
    };
    let reason = match kind {
        Blocker::Operator => hold.question.as_deref().unwrap_or(&hold.reason),
        _ => &hold.reason,
    };
    let since = Some(hold.since.as_str()).filter(|s| !s.is_empty());
    Some(json!({"type": kind.name(), "kind": hold.kind.name(),
        "reason": clip(reason, REASON_CHARS), "since": since, "pull": pull}))
}

fn stamp(at: Option<OffsetDateTime>) -> Option<String> {
    at.and_then(|at| at.format(&Rfc3339).ok())
}

fn url_of(item: &str) -> Option<String> {
    let (repository, number) = crate::changes::parse_ref(item)?;
    Some(holds::url(&ItemRef {
        host: DEFAULT_HOST.into(),
        repository: repository.to_owned(),
        number,
    }))
}

/// One epic's card, with its operator-blocked count and last change, which
/// order the cards.
fn epic(
    facts: &EpicFacts,
    holds: &BTreeMap<String, Vec<&Published>>,
    sources: &Sources,
    runs: &[&view::Run],
) -> (usize, Option<OffsetDateTime>, Value) {
    let mut children = children(facts);
    let epic_key = key(&facts.issue);
    let keys: BTreeSet<String> = (children.iter().filter_map(Child::key))
        .chain([epic_key.clone()])
        .collect();
    let mine: Vec<&&view::Run> = runs.iter().filter(|r| keys.contains(&r.key())).collect();
    let running = |key: &str| mine.iter().any(|r| r.key() == key);
    let done = children.iter().filter(|c| c.done).count();
    let total = children.len();
    let changed_at = (children.iter().filter_map(|c| c.updated))
        .chain(facts.detail.updated)
        .max();
    // The open first, as the board orders the card.
    children.sort_by_key(|c| c.done);
    let mut operator = 0;
    let mut listed = Vec::new();
    for child in children.iter().take(MAX_CHILDREN) {
        let blocker = match (child.done, child.key()) {
            (true, _) => None,
            (false, Some(key)) => blocker(&key, holds, sources, running(&key), true),
            (false, None) => blocker("", holds, sources, false, false),
        };
        operator += usize::from(blocker.as_ref().is_some_and(|b| b["type"] == "operator"));
        listed.push(json!({
            "item": child.item, "url": child.item.as_deref().and_then(url_of),
            "title": child.title.as_deref().and_then(|t| clip(t, TITLE_CHARS)),
            "source": child.source, "state": if child.done { "done" } else { "open" },
            "blocker": blocker,
        }));
    }
    // Blocked children past the cut still count.
    for child in children.iter().skip(MAX_CHILDREN).filter(|c| !c.done) {
        let key = child.key().unwrap_or_default();
        let blocked = blocker(&key, holds, sources, running(&key), true);
        operator += usize::from(blocked.is_some_and(|b| b["type"] == "operator"));
    }
    // The epic's own hold, where it has one: it is not stage 4's to start.
    let own = holds
        .contains_key(&epic_key)
        .then(|| blocker(&epic_key, holds, sources, false, false));
    let own = own.flatten();
    operator += usize::from(own.as_ref().is_some_and(|b| b["type"] == "operator"));
    let card = json!({
        "item": facts.issue.display(), "url": holds::url(&facts.issue),
        "title": facts.detail.title.as_deref().and_then(|t| clip(t, TITLE_CHARS)),
        "goal": goal(&facts.body), "changed_at": stamp(changed_at),
        "progress": {"done": done, "total": total}, "operator_blocked": operator,
        "blocker": own,
        "runs": mine.iter().take(MAX_RUNS).map(|r| r.json()).collect::<Vec<_>>(),
        "runs_omitted": mine.len().saturating_sub(MAX_RUNS),
        "children": listed, "omitted": total.saturating_sub(MAX_CHILDREN),
    });
    (operator, changed_at, card)
}

/// The focus at `at`: a card for each P0 epic, the most blocked on the
/// operator first, then the longest unchanged, then by item; and what of
/// its sources the pass read, which alone makes it complete.
pub(super) fn snapshot(at: &str, sources: &Sources) -> Value {
    let holds = holds_by_item(sources.holds);
    let mut runs: Vec<&view::Run> = Vec::new();
    for run in sources
        .flights
        .unwrap_or_default()
        .iter()
        .chain(sources.started)
    {
        if !runs.iter().any(|r| r.id() == run.id()) {
            runs.push(run);
        }
    }
    let epics = sources.read.map(|r| r.epics.as_slice()).unwrap_or_default();
    let mut cards: Vec<_> = (epics.iter())
        .map(|facts| epic(facts, &holds, sources, &runs))
        .collect();
    cards.sort_by(|a, b| {
        let item = |c: &Value| c["item"].as_str().map(str::to_ascii_lowercase);
        (std::cmp::Reverse(a.0), a.1.is_none(), a.1, item(&a.2)).cmp(&(
            std::cmp::Reverse(b.0),
            b.1.is_none(),
            b.1,
            item(&b.2),
        ))
    });
    let read = |done: bool| if done { "complete" } else { "unread" };
    let listing = match sources.read {
        None => "unread",
        Some(read) if read.unread > 0 => "partial",
        Some(_) => "complete",
    };
    let (flights, board) = (sources.flights.is_some(), sources.quiet.is_some());
    json!({
        "schema": SCHEMA, "observed_at": at, "priority": PRIORITY,
        "complete": listing == "complete" && flights && board,
        "sources": {"epics": listing, "runs": read(flights), "board": read(board)},
        "unread_epics": sources.read.map_or(0, |r| r.unread),
        "omitted": cards.len().saturating_sub(MAX_EPICS),
        "epics": cards.into_iter().take(MAX_EPICS).map(|(_, _, c)| c).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests;
