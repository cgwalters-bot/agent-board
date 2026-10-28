//! The pass's holds as data (agent-board#307). Each hold is typed where it
//! is decided; both its `hold …` line and its entry in the hold snapshot,
//! which the review app renders instead of deriving its own verdicts, are
//! made from that one value, so the two cannot drift. Nothing here reads
//! the forge, a clock or a file.

use super::lease::Held;
use crate::supervise::health::json_block;
use anyhow::{Context, Result};
use board_core::ItemRef;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

/// The schema of the hold snapshot ([`super::Config::holds`]).
pub const HOLDS_SCHEMA: &str = "agent-board/holds/v1";
/// The first line of the bot's hold snapshot comment, by which the pass
/// finds it again: not the usage comment's.
pub const HOLDS_MARKER: &str = "<!-- agent-board-holds v1 -->";
/// How a board Why that asks the operator something starts, as the review
/// app reads it.
const QUESTION_PREFIX: &str = "Q:";
/// The most characters the hold comment may have, under GitHub's 65,536
/// for a comment with room to spare; past it, holds that do not wait on the
/// operator are left out ([`fit`]).
pub const MAX_COMMENT_CHARS: usize = 60_000;

/// What keeps the pass off an item, or off every new run (`Wip`, `Pools`,
/// `Budget`):
/// an entry's `kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HoldKind {
    Closed,
    /// Labelled `needs-human`, by a person or a triage verdict.
    NeedsHuman,
    /// Labelled `needs-design`.
    NeedsDesign,
    /// Board Status Blocked, set by a person.
    Blocked,
    /// Open dependencies, or a lower stacked slice not yet open.
    WaitingOn,
    /// The bot's or the operator's pull request that closes it is open.
    PrOpen,
    /// A run is in flight on it.
    InFlight,
    /// Its plan's run has ended, or an epic's planning request waits for
    /// its plan.
    Dispatched,
    /// Its last triage or planning request is too recent to ask again.
    TriageCooldown,
    /// An epic's planning request got no plan: the operator's once its one
    /// retry got none either ([`Hold::operator`]).
    EpicUnplanned,
    /// A plan, receipt or request the pass will not act on by itself: an
    /// invalid or refused plan, an apply cut short and not proved, a plan
    /// dispatched by hand, or no plan with triage off.
    NeedsOperator,
    Overlap,
    /// Something it depends on could not be read.
    Unreadable,
    /// The WIP limit.
    Wip,
    /// No pool has room.
    Pools,
    /// The week's spend reached the budget, or has no reading.
    Budget,
    Other,
}

impl HoldKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::NeedsHuman => "needs-human",
            Self::NeedsDesign => "needs-design",
            Self::Blocked => "blocked",
            Self::WaitingOn => "waiting-on",
            Self::PrOpen => "pr-open",
            Self::InFlight => "in-flight",
            Self::Dispatched => "dispatched",
            Self::TriageCooldown => "triage-cooldown",
            Self::EpicUnplanned => "epic-unplanned",
            Self::NeedsOperator => "needs-operator",
            Self::Overlap => "overlap",
            Self::Unreadable => "unreadable",
            Self::Wip => "wip",
            Self::Pools => "pools",
            Self::Budget => "budget",
            Self::Other => "other",
        }
    }

    /// Whether only the operator's action releases a hold of this kind, as
    /// a rule ([`Hold::operator`] may say otherwise). Board Blocked is a
    /// person's hold too, but on whatever they named, so it is not.
    pub fn operator(self) -> bool {
        matches!(
            self,
            Self::NeedsHuman | Self::NeedsDesign | Self::EpicUnplanned | Self::NeedsOperator
        )
    }
}

/// A hold's kind, the reason its line gives, and whether it waits on the
/// operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hold {
    pub kind: HoldKind,
    pub reason: String,
    pub operator: bool,
}

impl Hold {
    /// A hold that waits on the operator as its kind does.
    pub fn new(kind: HoldKind, reason: impl Into<String>) -> Self {
        Self {
            kind,
            reason: reason.into(),
            operator: kind.operator(),
        }
    }

    /// The same hold, waiting on the operator or not: an epic the pass will
    /// retry by itself is not the operator's yet.
    pub fn operator(self, operator: bool) -> Self {
        Self { operator, ..self }
    }
}

/// What a hold keeps the pass off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Every new run: `hold wip|pools|budget: …`.
    Pass,
    /// A stage 4 candidate: `hold OWNER/REPO#N: …`.
    Item(ItemRef),
    /// A stage 3 epic: `hold epic OWNER/REPO#N: …`.
    Epic(ItemRef),
}

/// One hold the pass decided, with what the board says of its item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub scope: Scope,
    pub hold: Hold,
    /// The item's board Priority, where known.
    pub priority: Option<String>,
    /// The item's board Why, where set.
    pub why: Option<String>,
}

impl Entry {
    /// The pass's line for it.
    pub fn line(&self) -> String {
        let reason = &self.hold.reason;
        match &self.scope {
            Scope::Pass => format!("hold {}: {reason}", self.hold.kind.name()),
            Scope::Item(item) => format!("hold {}: {reason}", item.display()),
            Scope::Epic(epic) => format!("hold epic {}: {reason}", epic.display()),
        }
    }

    fn item(&self) -> Option<&ItemRef> {
        match &self.scope {
            Scope::Pass => None,
            Scope::Item(item) | Scope::Epic(item) => Some(item),
        }
    }

    fn published(&self) -> Published {
        let item = self.item();
        Published {
            item: item.map(ItemRef::display),
            url: item.map(|i| format!("https://{}/{}/issues/{}", i.host, i.repository, i.number)),
            epic: matches!(self.scope, Scope::Epic(_)),
            kind: self.hold.kind,
            reason: self.hold.reason.clone(),
            operator: self.hold.operator,
            priority: self.priority.clone(),
            question: self.why.as_deref().and_then(question),
        }
    }
}

/// The WIP limit and the runs holding a slot once stage 4 scheduled, its
/// own starts included; `in_flight` is null where it did not count them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Wip {
    pub limit: u64,
    pub in_flight: Option<usize>,
}

/// What the pass did besides its holds, as its lines say.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PassSummary {
    /// `preview`, `actions` or `local` ([`super::Mode`]).
    pub mode: &'static str,
    /// Stage 4's candidates (`candidates: N …`); null where it did not
    /// read the board.
    pub candidates: Option<usize>,
    /// The runs stages 2 to 4 started, or in a preview would start.
    pub started: usize,
    /// Stage 3's live epics (`epics: N live …`); null where it did not
    /// list them.
    pub live_epics: Option<usize>,
    /// The revisit runs stage 4 started, or in a preview would start;
    /// absent without [`super::Config::revisit`] and null where stage 4 did
    /// not get that far, so that a pass without it publishes what it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revisited: Option<Option<usize>>,
}

/// The hold snapshot ([`HOLDS_SCHEMA`]).
#[derive(Debug, PartialEq, Serialize)]
pub struct Snapshot {
    pub schema: &'static str,
    /// The clock of the pass that wrote it.
    pub at: String,
    /// How many holds [`fit`] left out.
    pub truncated: usize,
    pub wip: Wip,
    pub pass: PassSummary,
    /// The live leases ([`super::Config::leases`]); absent where it is off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leases: Option<Vec<Held>>,
    pub holds: Vec<Published>,
}

/// One entry of the snapshot. Every key is always present: `item`, `url`,
/// `priority` and `question` are null where there is none.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Published {
    /// `OWNER/REPO#N`; null for a hold of every new run.
    pub item: Option<String>,
    pub url: Option<String>,
    /// Whether the item is an epic stage 3 holds.
    pub epic: bool,
    pub kind: HoldKind,
    /// What its line says after `hold …: `.
    pub reason: String,
    /// Whether it waits on the operator ([`Hold::operator`]).
    pub operator: bool,
    pub priority: Option<String>,
    /// The board Why's question, where it starts `Q:`.
    pub question: Option<String>,
}

/// The question a board Why asks, as the review app reads it: what follows
/// a leading `Q:`.
pub fn question(why: &str) -> Option<String> {
    let asked = why.trim().strip_prefix(QUESTION_PREFIX)?.trim();
    (!asked.is_empty()).then(|| asked.to_owned())
}

/// The snapshot of `entries` at `at`, with the pass's `wip` and `pass`: the
/// holds of every new run first, in the order decided, then by repository
/// and number.
pub fn snapshot(entries: &[Entry], at: String, (wip, pass): (Wip, PassSummary)) -> Snapshot {
    let mut sorted: Vec<&Entry> = entries.iter().collect();
    sorted.sort_by_key(|e| {
        e.item()
            .map(|i| (i.repository.to_ascii_lowercase(), i.number))
    });
    Snapshot {
        schema: HOLDS_SCHEMA,
        at,
        truncated: 0,
        wip,
        pass,
        leases: None,
        holds: sorted.into_iter().map(Entry::published).collect(),
    }
}

/// Whether `snapshot` says something other than the published comment
/// `body`, if any: its time alone is no change.
pub fn changed(body: Option<&str>, snapshot: &Snapshot) -> Result<bool> {
    let mut now = serde_json::to_value(snapshot)?;
    let published = body
        .and_then(json_block)
        .filter(|s| s["schema"] == HOLDS_SCHEMA);
    Ok(published.is_none_or(|mut s| {
        s["at"] = Value::Null;
        now["at"] = Value::Null;
        s != now
    }))
}

/// The hold snapshot's comment: its marker, then the JSON the review app
/// reads, compact, with one entry a line.
pub fn render(snapshot: &Snapshot) -> Result<String> {
    let holds = (snapshot.holds.iter())
        .map(serde_json::to_string)
        .collect::<serde_json::Result<Vec<_>>>()?;
    let lines = match holds.is_empty() {
        true => String::new(),
        false => format!("\n{}", holds.join(",\n")),
    };
    let leases = match &snapshot.leases {
        Some(leases) => format!(",\"leases\":{}", serde_json::to_string(leases)?),
        None => String::new(),
    };
    Ok(format!(
        "{HOLDS_MARKER}\nHolds as of {}. Written by `agent-board pass`; the review app reads the JSON below.\n\n```json\n{{\"schema\":{},\"at\":{},\"truncated\":{},\"wip\":{},\"pass\":{}{leases},\"holds\":[{lines}\n]}}\n```\n",
        snapshot.at,
        serde_json::to_string(snapshot.schema)?,
        serde_json::to_string(&snapshot.at)?,
        snapshot.truncated,
        serde_json::to_string(&snapshot.wip)?,
        serde_json::to_string(&snapshot.pass)?,
    ))
}

/// The order holds are left out in: no Priority first, then the lowest
/// (`P3` before `P0`), then the newest (the highest number).
fn left_out_first(hold: &Published) -> (bool, std::cmp::Reverse<String>, std::cmp::Reverse<u64>) {
    let number = (hold.item.as_deref())
        .and_then(|i| i.rsplit_once('#'))
        .and_then(|(_, n)| n.parse().ok())
        .unwrap_or_default();
    let priority = hold.priority.clone();
    (
        priority.is_some(),
        std::cmp::Reverse(priority.unwrap_or_default()),
        std::cmp::Reverse(number),
    )
}

/// `snapshot` within [`MAX_COMMENT_CHARS`] and its comment: where the whole
/// is longer, the item holds that do not wait on the operator are left out,
/// in [`left_out_first`] order, until it fits, and counted in `truncated`.
/// The operator's holds and the holds of every new run are always kept; if
/// they alone do not fit, that is an error.
pub fn fit(snapshot: Snapshot) -> Result<(Snapshot, String)> {
    let holds = &snapshot.holds;
    let mut order: Vec<usize> = (0..holds.len())
        .filter(|&n| holds[n].item.is_some() && !holds[n].operator)
        .collect();
    order.sort_by_key(|&n| left_out_first(&holds[n]));
    let mut order = order.into_iter();
    let mut dropped = BTreeSet::new();
    loop {
        let kept = (holds.iter().enumerate())
            .filter(|(n, _)| !dropped.contains(n))
            .map(|(_, h)| h.clone())
            .collect();
        let fitted = Snapshot {
            schema: snapshot.schema,
            at: snapshot.at.clone(),
            truncated: dropped.len(),
            wip: snapshot.wip.clone(),
            pass: snapshot.pass.clone(),
            leases: snapshot.leases.clone(),
            holds: kept,
        };
        let body = render(&fitted)?;
        let mut over = body.chars().count().saturating_sub(MAX_COMMENT_CHARS);
        if over == 0 {
            return Ok((fitted, body));
        }
        // Each entry is its JSON and a ",\n"; the count may grow a digit,
        // which the next round sees.
        while over > 0 {
            let n = order.next().with_context(|| {
                format!(
                    "the hold snapshot is over {MAX_COMMENT_CHARS} characters with only the operator's holds and those of every new run left"
                )
            })?;
            let len = serde_json::to_string(&holds[n])?.chars().count() + 2;
            over = over.saturating_sub(len);
            dropped.insert(n);
        }
    }
}

/// The JSON of a published hold comment, for tests.
#[cfg(test)]
pub fn parse(body: &str) -> Option<serde_json::Value> {
    json_block(body).filter(|s| s["schema"] == HOLDS_SCHEMA)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::DEFAULT_HOST;
    use serde_json::json;

    fn item(repository: &str, number: u64) -> ItemRef {
        ItemRef {
            host: DEFAULT_HOST.into(),
            repository: repository.into(),
            number,
        }
    }

    fn entry(scope: Scope, kind: HoldKind, reason: &str) -> Entry {
        Entry {
            scope,
            hold: Hold::new(kind, reason),
            priority: None,
            why: None,
        }
    }

    fn entries() -> Vec<Entry> {
        let mut asked = entry(
            Scope::Item(item("o/tracker", 1000)),
            HoldKind::NeedsHuman,
            "labelled needs-human",
        );
        asked.priority = Some("P1".into());
        asked.why = Some("  Q: Which target?\n".into());
        let mut epic = entry(
            Scope::Epic(item("o/tracker", 596)),
            HoldKind::EpicUnplanned,
            "its planning request R got no plan, nor did R0 before it; a person comments on the epic to plan it again",
        );
        epic.why = Some("Waits on the operator".into());
        vec![
            asked,
            entry(
                Scope::Item(item("o/agent-board", 7)),
                HoldKind::Dispatched,
                "plan P was dispatched already, U",
            ),
            epic,
            entry(Scope::Pass, HoldKind::Wip, "2 of 2 runs active"),
        ]
    }

    /// A pass that found 2 of 2 slots taken after starting one run.
    fn tally() -> (Wip, PassSummary) {
        let wip = Wip {
            limit: 2,
            in_flight: Some(2),
        };
        let pass = PassSummary {
            mode: "actions",
            candidates: Some(3),
            started: 1,
            live_epics: None,
            revisited: None,
        };
        (wip, pass)
    }

    /// `revisited` is absent without `revisit`, as `tally` has it, and
    /// otherwise null or a count.
    #[test]
    fn revisited_in_the_summary() {
        for (revisited, expected) in [
            (None, None),
            (Some(None), Some(json!(null))),
            (Some(Some(2)), Some(json!(2))),
        ] {
            let (wip, mut pass) = tally();
            pass.revisited = revisited;
            let value = serde_json::to_value(snapshot(&[], String::new(), (wip, pass))).unwrap();
            assert_eq!(value["pass"].get("revisited"), expected.as_ref());
        }
    }

    /// `leases` is absent where they are off, and otherwise listed, each
    /// with its slot; a change in them publishes.
    #[test]
    fn leases_in_the_snapshot() {
        let at = "2026-10-26T00:00:00Z";
        let off = snapshot(&entries(), at.into(), tally());
        let body = render(&off).unwrap();
        assert!(parse(&body).unwrap().get("leases").is_none(), "{body}");
        let lease = super::super::lease::Lease {
            run: 7,
            item: "o/r#1".into(),
            pool: "claude".into(),
            taken_at: at.into(),
            expires_at: "2026-10-27T00:00:00Z".into(),
            overridden: false,
        };
        let mut on = snapshot(&entries(), at.into(), tally());
        on.leases = Some(vec![Held { slot: 3, lease }]);
        assert!(changed(Some(&body), &on).unwrap());
        let body = render(&on).unwrap();
        assert_eq!(
            parse(&body).unwrap()["leases"],
            json!([{"slot": 3, "run": 7, "item": "o/r#1", "pool": "claude",
                "taken_at": at, "expires_at": "2026-10-27T00:00:00Z"}])
        );
        assert_eq!(parse(&body), Some(serde_json::to_value(&on).unwrap()));
        let (fitted, _) = fit(on).unwrap();
        assert_eq!(fitted.leases.unwrap().len(), 1);
    }

    /// The snapshot of `entries` with no clock, WIP count or summary.
    fn bare(entries: &[Entry]) -> Snapshot {
        snapshot(entries, String::new(), Default::default())
    }

    #[test]
    fn lines() {
        let lines: Vec<String> = entries().iter().map(Entry::line).collect();
        assert_eq!(
            lines,
            [
                "hold o/tracker#1000: labelled needs-human",
                "hold o/agent-board#7: plan P was dispatched already, U",
                "hold epic o/tracker#596: its planning request R got no plan, nor did R0 before it; a person comments on the epic to plan it again",
                "hold wip: 2 of 2 runs active",
            ]
        );
    }

    /// The pass's holds first, then by repository and number (tracker#596
    /// before #1000), each with its URL, whether it waits on the operator,
    /// its Priority and its Why's question; after the WIP limit and the
    /// pass's summary, null where unknown.
    #[test]
    fn snapshot_of_holds() {
        let snapshot = snapshot(&entries(), "2026-10-26T00:00:00Z".into(), tally());
        let expected = json!({
            "schema": HOLDS_SCHEMA,
            "at": "2026-10-26T00:00:00Z",
            "truncated": 0,
            "wip": {"limit": 2, "in_flight": 2},
            "pass": {"mode": "actions", "candidates": 3, "started": 1, "live_epics": null},
            "holds": [
                {"item": null, "url": null, "epic": false, "kind": "wip",
                 "reason": "2 of 2 runs active", "operator": false, "priority": null, "question": null},
                {"item": "o/agent-board#7", "url": "https://github.com/o/agent-board/issues/7",
                 "epic": false, "kind": "dispatched", "reason": "plan P was dispatched already, U",
                 "operator": false, "priority": null, "question": null},
                {"item": "o/tracker#596", "url": "https://github.com/o/tracker/issues/596",
                 "epic": true, "kind": "epic-unplanned",
                 "reason": "its planning request R got no plan, nor did R0 before it; a person comments on the epic to plan it again",
                 "operator": true, "priority": null, "question": null},
                {"item": "o/tracker#1000", "url": "https://github.com/o/tracker/issues/1000",
                 "epic": false, "kind": "needs-human", "reason": "labelled needs-human",
                 "operator": true, "priority": "P1", "question": "Which target?"},
            ],
        });
        assert_eq!(serde_json::to_value(&snapshot).unwrap(), expected);
        let body = render(&snapshot).unwrap();
        assert!(body.starts_with(HOLDS_MARKER), "{body}");
        // Compact, one entry a line.
        let lines: Vec<&str> = body
            .lines()
            .filter(|l| l.starts_with("{\"item\""))
            .collect();
        assert_eq!(lines.len(), 4, "{body}");
        assert_eq!(parse(&body), Some(expected));
        // A hold may wait on the operator otherwise than its kind does.
        let mut cooling = entries();
        cooling[2].hold = cooling[2].hold.clone().operator(false);
        let cooling = bare(&cooling);
        assert!(!cooling.holds[2].operator);
    }

    /// A board of 20 holds for the operator, each asking a long question,
    /// and `others` holds that do not wait on the operator, every fifth
    /// without a Priority and the rest P0 to P3; the last's reason is
    /// `pad` characters longer.
    fn board(others: u64, pad: usize) -> Vec<Entry> {
        let mut all = vec![entry(Scope::Pass, HoldKind::Wip, "8 of 8 runs active")];
        for n in 1..=20 {
            let mut asked = entry(
                Scope::Item(item("o/tracker", 5000 + n)),
                HoldKind::NeedsHuman,
                "labelled needs-human",
            );
            asked.why = Some(format!("Q: {}?", "Which way should this go ".repeat(40)));
            asked.priority = Some("P2".into());
            all.push(asked);
        }
        for n in 1..=others {
            let reason = format!("plan {n} was dispatched already, {}", "r".repeat(300));
            let mut held = entry(
                Scope::Item(item("o/work", n)),
                HoldKind::Dispatched,
                &reason,
            );
            held.priority = (n % 5 != 0).then(|| format!("P{}", n % 4));
            all.push(held);
        }
        if let Some(last) = all.last_mut() {
            last.hold.reason.push_str(&"p".repeat(pad));
        }
        all
    }

    fn chars(body: &str) -> usize {
        body.chars().count()
    }

    /// A comment of exactly [`MAX_COMMENT_CHARS`] is kept whole; one more
    /// character leaves out one hold, the first in [`left_out_first`]
    /// order: one without a Priority.
    #[test]
    fn fit_at_the_limit() {
        let whole = |pad| render(&bare(&board(59, pad))).unwrap();
        let pad = MAX_COMMENT_CHARS - chars(&whole(0));
        let (fitted, body) = fit(bare(&board(59, pad))).unwrap();
        assert_eq!((fitted.truncated, chars(&body)), (0, MAX_COMMENT_CHARS));
        let (fitted, body) = fit(bare(&board(59, pad + 1))).unwrap();
        assert_eq!(fitted.truncated, 1);
        assert!(chars(&body) <= MAX_COMMENT_CHARS);
        assert_eq!(parse(&body).unwrap()["truncated"], 1);
        let kept: BTreeSet<String> = fitted.holds.iter().filter_map(|h| h.item.clone()).collect();
        assert!(
            !kept.contains("o/work#55"),
            "the newest without a Priority goes first"
        );
        assert!(kept.contains("o/work#59"), "the padded one has a Priority");
    }

    /// Far past the limit, every hold for the operator and of every new
    /// run is kept, the others are left out lowest Priority and newest
    /// first, and no more than needed.
    #[test]
    fn fit_far_past_the_limit() {
        let full = bare(&board(240, 0));
        assert!(chars(&render(&full).unwrap()) > 65_536);
        let (fitted, body) = fit(bare(&board(240, 0))).unwrap();
        assert!(chars(&body) <= MAX_COMMENT_CHARS);
        assert!(fitted.truncated > 0);
        assert_eq!(fitted.holds.len() + fitted.truncated, full.holds.len());
        let operator = |s: &Snapshot| {
            s.holds
                .iter()
                .filter(|h| h.operator || h.item.is_none())
                .count()
        };
        assert_eq!(operator(&fitted), 21);
        assert!(
            fitted
                .holds
                .iter()
                .all(|h| h.question.is_none() || h.operator)
        );
        // The left out are the first in order.
        let mut order: Vec<&Published> = full
            .holds
            .iter()
            .filter(|h| !h.operator && h.item.is_some())
            .collect();
        order.sort_by_key(|h| left_out_first(h));
        let (out, kept) = order.split_at(fitted.truncated);
        assert!(out.iter().all(|h| !fitted.holds.contains(h)));
        assert!(kept.iter().all(|h| fitted.holds.contains(h)));
        // One fewer left out would not fit.
        let back = serde_json::to_string(out[out.len() - 1]).unwrap();
        assert!(chars(&body) + chars(&back) + 2 > MAX_COMMENT_CHARS);
        // The operator's questions alone past the limit are an error.
        let mut asked = board(0, 0);
        for hold in &mut asked[1..] {
            hold.why = Some(format!("Q: {}", "q".repeat(4000)));
        }
        assert!(fit(bare(&asked)).is_err());
    }

    /// Only a change of the holds, the WIP count or the pass's summary
    /// publishes: not a later clock, nor the order the pass decided them in.
    /// A comment from before the WIP count and summary is republished.
    #[test]
    fn unchanged_is_not_republished() {
        let at = "2026-10-26T00:00:00Z";
        let published = render(&snapshot(&entries(), at.into(), tally())).unwrap();
        let mut reordered = entries();
        reordered.reverse();
        let later = snapshot(&reordered, "2026-10-26T00:10:00Z".into(), tally());
        assert!(!changed(Some(&published), &later).unwrap());
        let mut fewer = entries();
        fewer.pop();
        let fewer = snapshot(&fewer, "2026-10-26T00:10:00Z".into(), tally());
        assert!(changed(Some(&published), &fewer).unwrap());
        let (mut wip, mut pass) = tally();
        wip.in_flight = Some(1);
        let freed = snapshot(&entries(), at.into(), (wip, tally().1));
        assert!(changed(Some(&published), &freed).unwrap());
        pass.started = 0;
        let idle = snapshot(&entries(), at.into(), (tally().0, pass));
        assert!(changed(Some(&published), &idle).unwrap());
        assert!(changed(None, &later).unwrap());
        let other = published.replace(HOLDS_SCHEMA, "agent-board/pass-holds/v1");
        assert!(changed(Some(&other), &later).unwrap());
        let start = published.find(",\"wip\"").unwrap();
        let end = published.find(",\"holds\"").unwrap();
        let older = format!("{}{}", &published[..start], &published[end..]);
        assert!(parse(&older).is_some(), "{older}");
        assert!(changed(Some(&older), &later).unwrap());
    }

    #[test]
    fn questions() {
        for (why, expected) in [
            ("Q: Which target?", Some("Which target?")),
            ("\n Q:Ship it?  ", Some("Ship it?")),
            ("Q:", None),
            ("Waits on Q: later", None),
            ("", None),
        ] {
            assert_eq!(question(why).as_deref(), expected, "{why:?}");
        }
    }

    #[test]
    fn operator_kinds() {
        let operator: Vec<&str> = [
            HoldKind::Closed,
            HoldKind::NeedsHuman,
            HoldKind::NeedsDesign,
            HoldKind::Blocked,
            HoldKind::WaitingOn,
            HoldKind::PrOpen,
            HoldKind::InFlight,
            HoldKind::Dispatched,
            HoldKind::TriageCooldown,
            HoldKind::EpicUnplanned,
            HoldKind::NeedsOperator,
            HoldKind::Overlap,
            HoldKind::Unreadable,
            HoldKind::Wip,
            HoldKind::Pools,
            HoldKind::Budget,
            HoldKind::Other,
        ]
        .into_iter()
        .inspect(|k| assert_eq!(serde_json::to_value(k).unwrap(), k.name()))
        .filter(|k| k.operator())
        .map(HoldKind::name)
        .collect();
        assert_eq!(
            operator,
            [
                "needs-human",
                "needs-design",
                "epic-unplanned",
                "needs-operator"
            ]
        );
    }
}
