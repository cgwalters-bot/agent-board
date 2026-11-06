//! The pass's holds as data (agent-board#307). Each hold is typed where it
//! is decided; both its `hold …` line and its entry in the hold snapshot,
//! which the review app renders instead of deriving its own verdicts, are
//! made from that one value, so the two cannot drift. Nothing here reads
//! the forge, a clock or a file.
//!
//! A hold that stays the same for hours where the pass itself is stuck, and
//! a pass that moves nothing on for hours, raise stall alarms in the same
//! snapshot (agent-board#454): `since` and `progress_at` carry over from the
//! published one ([`carry`]), and the alarms are worked out again from them
//! each pass ([`alarm`]), so that one clears itself once its cause is gone.
//!
//! With `holds.needsYou` the snapshot also lists what waits on the operator
//! (tracker#976), ranked, from the same holds and what the pass read of its
//! followed pull requests, proposals and digest ([`needs_you`]): nothing is
//! inferred that they do not say.

use super::lease::Held;
use crate::github::DEFAULT_HOST;
use crate::poll::{approval, validate_repository};
use crate::supervise::health::json_block;
use anyhow::{Result, ensure};
use board_core::ItemRef;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

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
/// The most children waiting on the operator a not-live epic's entry names
/// ([`not_live`]); it counts the rest.
const MAX_NAMED: usize = 3;
/// `stall.holdHours` when left out, and its bounds: a week at most.
const HOLD_HOURS: u64 = 6;
const HOLD_HOURS_MIN: u64 = 1;
const HOLD_HOURS_LIMIT: u64 = 7 * 24;
/// `stall.idleHours` when left out, and its bounds.
const IDLE_HOURS: u64 = 4;
const IDLE_HOURS_MIN: u64 = 1;
const IDLE_HOURS_LIMIT: u64 = 7 * 24;
/// What a time in a hold's reason reads as when two passes' holds are
/// compared ([`normalised`]).
const TIME_PLACEHOLDER: &str = "<time>";
/// The shape of an RFC 3339 time up to its seconds, and of its offset; `d`
/// is a digit.
const TIME_SHAPE: &[u8] = b"dddd-dd-ddTdd:dd:dd";
const OFFSET_SHAPE: &[u8] = b"dd:dd";
/// The most hold alarms a snapshot has an entry each for ([`alarm`]); past
/// them, one entry on the holds issue counts the rest, so that alarms alone
/// cannot fill the comment.
const MAX_STALLS: usize = 10;
/// The most characters of a stalled hold's reason its alarm repeats.
const MAX_STALL_REASON: usize = 300;
/// How the idle alarm's reason names the pass log ([`alarm`]).
const PASS_LOG: &str = "; see the pass log ";
/// The schema of the needs-you list in the hold snapshot ([`needs_you`]).
/// v2 added the sources of [`Reading`], typed references and
/// recommendations, and made `kind` and `source` open strings.
pub const NEEDS_YOU_SCHEMA: &str = "agent-board/needs-you/v2";
/// Former item cap, retained as a regression-test boundary only.
#[cfg(test)]
pub const MAX_NEEDS_YOU: usize = 100;
/// How a hold's reason names the pull request whose approval it waits on
/// (`approve pull request OWNER/REPO#N to merge`, [`approval_of`]).
pub const APPROVE_PREFIX: &str = "approve pull request ";
pub const APPROVE_SUFFIX: &str = " to merge";
/// How long an unchanged needs-you list goes unwritten ([`changed`]): past
/// it, the pass writes it again for its `validated_at` alone.
const NEEDS_YOU_REFRESH_HOURS: u64 = 1;
/// The most characters of a hold's question or reason an ask repeats; the
/// hold's entry has it whole. The list doubles the operator's holds, which
/// [`fit`] never leaves out.
pub const MAX_ASK: usize = 500;

/// What keeps the pass off an item, or off every new run (`Wip`, `Pools`,
/// `Budget`):
/// an entry's `kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HoldKind {
    Closed,
    /// Labelled `needs-human`, by a person or a triage verdict.
    NeedsHuman,
    /// Triage needs a follow-up issue number, not a human decision. Left
    /// visible to the controller without labelling or assigning the source.
    NeedsIssue,
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
    /// An epic due to be planned, behind `epics.focus`.
    Focus,
    /// An epic's planning request got no plan: the operator's once its one
    /// retry got none either ([`Hold::operator`]).
    EpicUnplanned,
    /// A plan, receipt or request the pass will not act on by itself: an
    /// invalid or refused plan, an apply cut short and not proved, a plan
    /// dispatched by hand (research, or a triage of another issue), or no
    /// plan with triage off.
    NeedsOperator,
    /// Its parent is not a live epic; the operator's only where its
    /// Priority is among the board's first ([`Hold::operator`]).
    EpicNotLive,
    /// Outside every live epic, with no plan, under the triage scope
    /// `epics`: no triage run plans it.
    OutsideEpics,
    Overlap,
    /// Not a hold of an item: an open pull request that holds items by
    /// overlap, has stood still for `overlap.staleHours` and that only a
    /// person can move, so that it is one card rather than a stall alarm
    /// on each item it holds ([`super::blockers`]).
    Blocking,
    /// Not a hold: an open pull request overlaps it, but the pass's
    /// `overlap` setting let it start anyway, so the overlap stays visible.
    OverlapIgnored,
    /// Something it depends on could not be read.
    Unreadable,
    /// The WIP limit.
    Wip,
    /// No pool has room.
    Pools,
    /// The week's spend reached the budget, or has no reading.
    Budget,
    Other,
    /// Not a hold of its own: a stall alarm ([`alarm`]), on the item whose
    /// hold stayed the same too long, or on the holds issue where the pass
    /// moved nothing on for too long.
    Stalled,
}

impl HoldKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::NeedsHuman => "needs-human",
            Self::NeedsIssue => "needs-issue",
            Self::NeedsDesign => "needs-design",
            Self::Blocked => "blocked",
            Self::WaitingOn => "waiting-on",
            Self::PrOpen => "pr-open",
            Self::InFlight => "in-flight",
            Self::Dispatched => "dispatched",
            Self::TriageCooldown => "triage-cooldown",
            Self::Focus => "focus",
            Self::EpicUnplanned => "epic-unplanned",
            Self::NeedsOperator => "needs-operator",
            Self::EpicNotLive => "epic-not-live",
            Self::OutsideEpics => "outside-epics",
            Self::Overlap => "overlap",
            Self::Blocking => "blocking",
            Self::OverlapIgnored => "overlap-ignored",
            Self::Unreadable => "unreadable",
            Self::Wip => "wip",
            Self::Pools => "pools",
            Self::Budget => "budget",
            Self::Other => "other",
            Self::Stalled => "stalled",
        }
    }

    /// Whether only the operator's action releases a hold of this kind, as
    /// a rule ([`Hold::operator`] may say otherwise). Board Blocked is a
    /// person's hold too, but on whatever they named, so it is not; nor is
    /// `needs-design`, a parked design that asks the operator nothing.
    pub fn operator(self) -> bool {
        matches!(
            self,
            Self::NeedsHuman
                | Self::EpicUnplanned
                | Self::NeedsOperator
                | Self::EpicNotLive
                | Self::Blocking
                | Self::Stalled
        )
    }

    /// Whether a hold of this kind, the same for `stall.holdHours`, says the
    /// pass itself is stuck, which raises an alarm on an epic or a
    /// top-Priority item ([`alarm`]): an `other` hold (a plan waiting on
    /// another, say), an `unreadable` one or an `overlap` that never clears.
    /// The others wait by design and are left alone: on a person
    /// (`needs-human`, `needs-design`, `blocked`, and every hold that waits
    /// on the operator, already the operator's to see), on a pull request,
    /// a run or a date (`closed`, `waiting-on`, `pr-open`, `in-flight`,
    /// `dispatched`, `triage-cooldown`), on an epic or a triage scope
    /// (`focus`, `epic-not-live`, `outside-epics`), or are no hold at all
    /// (`overlap-ignored`, `stalled`). The holds of every new run are
    /// pacing, which the idle alarm covers.
    pub fn stalls(self) -> bool {
        matches!(self, Self::Other | Self::Unreadable | Self::Overlap)
    }
}

/// `stall` in the configuration ([`super::Config::stall`]): on wherever
/// `holds` is, and this only tunes it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Stall {
    /// How long a hold that [`HoldKind::stalls`] may stay the same on an
    /// epic or a top-Priority item before it raises an alarm.
    #[serde(default = "hold_hours")]
    pub hold_hours: u64,
    /// How long an applying pass with candidates may start, apply and merge
    /// nothing before it raises an alarm.
    #[serde(default = "idle_hours")]
    pub idle_hours: u64,
}

fn hold_hours() -> u64 {
    HOLD_HOURS
}

fn idle_hours() -> u64 {
    IDLE_HOURS
}

impl Default for Stall {
    fn default() -> Self {
        Self {
            hold_hours: HOLD_HOURS,
            idle_hours: IDLE_HOURS,
        }
    }
}

impl Stall {
    pub fn check(&self) -> Result<()> {
        ensure!(
            (HOLD_HOURS_MIN..=HOLD_HOURS_LIMIT).contains(&self.hold_hours),
            "stall holdHours is not {HOLD_HOURS_MIN} to {HOLD_HOURS_LIMIT}"
        );
        ensure!(
            (IDLE_HOURS_MIN..=IDLE_HOURS_LIMIT).contains(&self.idle_hours),
            "stall idleHours is not {IDLE_HOURS_MIN} to {IDLE_HOURS_LIMIT}"
        );
        Ok(())
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
    /// A stage 4 candidate under a parent that is not a live epic, and that
    /// parent: a line of its own like [`Scope::Item`]'s, but one entry for
    /// all of the parent's children in the snapshot ([`snapshot`]).
    Under(ItemRef, ItemRef),
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
        match (self.hold.kind, self.item()) {
            (HoldKind::OverlapIgnored, Some(item)) => {
                return format!("overlap ignored: {}: {reason}", item.display());
            }
            (HoldKind::Blocking, Some(pull)) => {
                return format!("blocker {}: {reason}", pull.display());
            }
            _ => {}
        }
        match &self.scope {
            Scope::Pass => format!("hold {}: {reason}", self.hold.kind.name()),
            Scope::Item(item) | Scope::Under(item, _) => {
                format!("hold {}: {reason}", item.display())
            }
            Scope::Epic(epic) => format!("hold epic {}: {reason}", epic.display()),
        }
    }

    fn item(&self) -> Option<&ItemRef> {
        match &self.scope {
            Scope::Pass => None,
            Scope::Item(item) | Scope::Epic(item) | Scope::Under(item, _) => Some(item),
        }
    }

    fn published(&self) -> Published {
        let item = self.item();
        Published {
            item: item.map(ItemRef::display),
            url: item.map(url),
            epic: matches!(self.scope, Scope::Epic(_)),
            kind: self.hold.kind,
            reason: self.hold.reason.clone(),
            operator: self.hold.operator,
            priority: self.priority.clone(),
            question: self.why.as_deref().and_then(question),
            since: String::new(),
            carried: false,
            watched: false,
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
    /// Bounded daily confirmed effects; absent in older v1 snapshots and
    /// previews. Accounting alone never makes an idle pass publish.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overhead: Option<Overhead>,
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
    /// When an applying pass last started a run, applied an epic plan or
    /// merged a pull request, as far as the published snapshots say: this
    /// snapshot's `at` where this pass did, or none before it said
    /// ([`carry`]).
    pub progress_at: String,
}

/// Today and at most the seven preceding UTC calendar days.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overhead {
    pub today: OverheadDay,
    pub history: Vec<OverheadDay>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverheadDay {
    pub day: String,
    pub complete: bool,
    pub passes: u64,
    #[serde(flatten)]
    pub effects: super::Effects,
}

const OVERHEAD_HISTORY_DAYS: i64 = 7;

/// Carry only a whole, well-formed aggregate. Unknown history is never zero.
/// The injected pass clock is converted to UTC, including across rollover.
pub fn overhead(
    previous: Option<&Value>,
    now: OffsetDateTime,
    effects: &super::Effects,
) -> Overhead {
    let today = now.to_offset(time::UtcOffset::UTC).date();
    let parse_day = |day: &str| {
        OffsetDateTime::parse(&format!("{day}T00:00:00Z"), &Rfc3339)
            .ok()
            .map(|t| t.date())
    };
    let carried = previous
        .filter(|p| p["schema"] == HOLDS_SCHEMA)
        .and_then(|p| {
            let at = parse_time(p["at"].as_str()?)?;
            let old: Overhead = serde_json::from_value(p["pass"]["overhead"].clone()).ok()?;
            let day = parse_day(&old.today.day)?;
            if at > now
                || at.to_offset(time::UtcOffset::UTC).date() != day
                || day > today
                || old.history.len() > OVERHEAD_HISTORY_DAYS as usize
            {
                return None;
            }
            let mut seen = BTreeSet::new();
            for entry in std::iter::once(&old.today).chain(&old.history) {
                let d = parse_day(&entry.day)?;
                if !seen.insert(d)
                    || d > day
                    || (day - d).whole_days() > OVERHEAD_HISTORY_DAYS
                    || entry.effects.starts.len() > START_KINDS.len()
                    || entry
                        .effects
                        .starts
                        .keys()
                        .any(|k| !START_KINDS.contains(&k.as_str()))
                {
                    return None;
                }
            }
            Some(old)
        });
    let mut result = Overhead {
        today: OverheadDay {
            day: today.to_string(),
            complete: false,
            passes: 0,
            effects: super::Effects::default(),
        },
        history: Vec::new(),
    };
    if let Some(mut old) = carried {
        if old.today.day == result.today.day {
            result.today = old.today;
        } else {
            result.today.complete =
                parse_day(&old.today.day).is_some_and(|d| (today - d).whole_days() == 1);
            old.history.push(old.today);
        }
        result.history = old
            .history
            .into_iter()
            .filter(|d| {
                parse_day(&d.day)
                    .is_some_and(|day| (today - day).whole_days() <= OVERHEAD_HISTORY_DAYS)
            })
            .collect();
        result.history.sort_by(|a, b| b.day.cmp(&a.day));
    }
    let day = &mut result.today;
    let mut add = |a: &mut u64, b: u64| {
        if let Some(sum) = a.checked_add(b) {
            *a = sum;
        } else {
            day.complete = false;
            *a = u64::MAX;
        }
    };
    add(&mut day.passes, 1);
    add(&mut day.effects.comments_created, effects.comments_created);
    add(&mut day.effects.comments_edited, effects.comments_edited);
    for (kind, count) in &effects.starts {
        add(day.effects.starts.entry(kind.clone()).or_default(), *count);
    }
    result
}

/// Fixed vocabulary keeps carried data bounded as well as the day count.
const START_KINDS: &[&str] = &[
    "fix",
    "rebase",
    "continue",
    "answer",
    "review",
    "review rerun",
    "check rerun",
    "run rerun",
    "planning",
    "implement",
    "triage",
    "research",
    "revisit",
    "sweep",
    "close",
];

/// The hold snapshot ([`HOLDS_SCHEMA`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Snapshot {
    /// Per-repository observed work; absent means unavailable, not empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatchable_work: Option<Value>,
    /// The pass view ([`super::view`]); absent unless `holds.passView`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pass_view: Option<Value>,
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
    /// What waits on the operator ([`needs_you`]); absent unless
    /// `holds.needsYou`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_you: Option<NeedsYou>,
    pub holds: Vec<Published>,
}

/// One entry of the snapshot. Every key is always present: `item`, `url`,
/// `priority` and `question` are null where there is none.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Published {
    /// `OWNER/REPO#N`; null for a hold of every new run.
    pub item: Option<String>,
    pub url: Option<String>,
    /// Whether the item is an epic stage 3 holds, or a parent that is not
    /// a live epic (`epic-not-live`).
    pub epic: bool,
    pub kind: HoldKind,
    /// What its line says after `hold …: `.
    pub reason: String,
    /// Whether it waits on the operator ([`Hold::operator`]).
    pub operator: bool,
    pub priority: Option<String>,
    /// The board Why's question, where it starts `Q:`.
    pub question: Option<String>,
    /// Since when the snapshots have had this hold ([`carry`]).
    #[serde(default)]
    pub since: String,
    /// Kept from the published snapshot by a pass that did not read it
    /// again ([`carry`]), which raises no alarm on it; not published.
    #[serde(skip)]
    pub carried: bool,
    /// One whose `since` a stall alarm is raised from ([`alarm`]), so that
    /// [`fit`] keeps it, and so its `since`, past the other holds; not
    /// published.
    #[serde(skip)]
    pub watched: bool,
}

/// What makes two passes' holds the same: the item, whether it is an epic,
/// the kind and the reason, its times aside ([`normalised`]).
type Same = (Option<String>, bool, String, String);

impl Published {
    fn same(&self) -> Same {
        let (item, kind) = (self.item.clone(), self.kind.name().to_owned());
        (item, self.epic, kind, normalised(&self.reason))
    }
}

/// The question a board Why asks, as the review app reads it: what follows
/// a leading `Q:`.
pub fn question(why: &str) -> Option<String> {
    let asked = why.trim().strip_prefix(QUESTION_PREFIX)?.trim();
    (!asked.is_empty()).then(|| asked.to_owned())
}

fn url(item: &ItemRef) -> String {
    // A Forgejo pass's host is its instance, scheme and path included.
    let base = match item.host.contains("://") {
        true => item.host.clone(),
        false => format!("https://{}", item.host),
    };
    format!("{base}/{}/issues/{}", item.repository, item.number)
}

/// How the snapshot orders an item: by repository and number.
fn order(item: &ItemRef) -> (String, u64) {
    (item.repository.to_ascii_lowercase(), item.number)
}

/// The one entry for `epic`, not live, and its held `children`: it waits
/// on the operator if any of them does, with their highest Priority, and
/// names up to [`MAX_NAMED`] of those children, highest Priority first.
fn not_live(epic: &ItemRef, children: &[&Entry]) -> Published {
    let mut waiting = children.to_vec();
    waiting.retain(|c| c.hold.operator);
    waiting.sort_by_key(|c| (c.priority.clone(), c.item().map(order)));
    let mut reason = format!(
        "not a live epic, so its {} open child issue(s) wait until the operator or the bot labels it epic",
        children.len()
    );
    let named: Vec<String> = (waiting.iter().take(MAX_NAMED))
        .map(|c| {
            let item = c.item().map(ItemRef::display).unwrap_or_default();
            format!("{item} ({})", c.priority.as_deref().unwrap_or_default())
        })
        .collect();
    if !named.is_empty() {
        reason.push_str(&format!(", among them {}", named.join(", ")));
    }
    if waiting.len() > MAX_NAMED {
        reason.push_str(&format!(" and {} more", waiting.len() - MAX_NAMED));
    }
    Published {
        item: Some(epic.display()),
        url: Some(url(epic)),
        epic: true,
        kind: HoldKind::EpicNotLive,
        reason,
        operator: !waiting.is_empty(),
        priority: children.iter().filter_map(|c| c.priority.clone()).min(),
        question: None,
        since: String::new(),
        carried: false,
        watched: false,
    }
}

/// The snapshot of `entries` at `at`, with the pass's `wip` and `pass`: the
/// holds of every new run first, in the order decided, then by repository
/// and number. The children held under a parent that is not a live epic
/// ([`Scope::Under`]) are one entry for that parent ([`not_live`]), so
/// that the review app asks the operator once an epic, not once a child.
/// Each hold is new as of `at`, as is the pass's progress, until [`carry`].
pub fn snapshot(entries: &[Entry], at: String, (wip, mut pass): (Wip, PassSummary)) -> Snapshot {
    let mut under: BTreeMap<(String, u64), (&ItemRef, Vec<&Entry>)> = BTreeMap::new();
    let mut sorted = Vec::new();
    for entry in entries {
        match &entry.scope {
            Scope::Under(_, epic) => {
                let (_, children) = under.entry(order(epic)).or_insert((epic, Vec::new()));
                children.push(entry);
            }
            _ => sorted.push((entry.item().map(order), entry.published())),
        }
    }
    for (key, (epic, children)) in under {
        sorted.push((Some(key), not_live(epic, &children)));
    }
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    pass.progress_at = at.clone();
    let holds = (sorted.into_iter())
        .map(|(_, published)| Published {
            since: at.clone(),
            ..published
        })
        .collect();
    Snapshot {
        dispatchable_work: None,
        pass_view: None,
        schema: HOLDS_SCHEMA,
        at,
        truncated: 0,
        wip,
        pass,
        leases: None,
        needs_you: None,
        holds,
    }
}

/// `reason` with each RFC 3339 time in it (`2026-10-28T18:24:18Z`, with or
/// without fractional seconds, `Z` or an offset) as [`TIME_PLACEHOLDER`], so
/// that a hold whose only change is a time is the same hold. What else a
/// time is part of stays: plan `6477159835@T` is another plan than
/// `6477159836@T`.
fn normalised(reason: &str) -> String {
    let bytes = reason.as_bytes();
    let mut out = String::with_capacity(reason.len());
    let (mut copied, mut at) = (0, 0);
    while at < bytes.len() {
        match time_len(&bytes[at..]) {
            // A time starts with an ASCII digit and ends with one or `Z`,
            // so both ends are on character boundaries.
            Some(len) => {
                out.push_str(&reason[copied..at]);
                out.push_str(TIME_PLACEHOLDER);
                at += len;
                copied = at;
            }
            None => at += 1,
        }
    }
    out.push_str(&reason[copied..]);
    out
}

/// Whether `text` starts with `shape` ([`TIME_SHAPE`]).
fn shaped(text: &[u8], shape: &[u8]) -> bool {
    text.len() >= shape.len()
        && (shape.iter().zip(text)).all(|(&s, &c)| match s {
            b'd' => c.is_ascii_digit(),
            _ => c == s,
        })
}

/// The length of the RFC 3339 time `text` starts with, if it does.
fn time_len(text: &[u8]) -> Option<usize> {
    if !shaped(text, TIME_SHAPE) {
        return None;
    }
    let mut len = TIME_SHAPE.len();
    if text.get(len) == Some(&b'.') {
        let digits = (text[len + 1..].iter())
            .take_while(|c| c.is_ascii_digit())
            .count();
        if digits > 0 {
            len += 1 + digits;
        }
    }
    match text.get(len) {
        Some(b'Z' | b'z') => len += 1,
        Some(b'+' | b'-') if shaped(&text[len + 1..], OFFSET_SHAPE) => {
            len += 1 + OFFSET_SHAPE.len();
        }
        _ => {}
    }
    Some(len)
}

fn parse_time(at: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(at, &Rfc3339).ok()
}

/// `since`, a published time, where it is one no later than `at`.
fn not_after(since: &Value, at: &str) -> Option<String> {
    let since = since.as_str()?;
    (parse_time(since)? <= parse_time(at)?).then(|| since.to_owned())
}

/// Whether `hours` or more went by from `since` to `at`; not where either
/// is no time.
fn over(since: &str, at: &str, hours: u64) -> bool {
    let (Some(since), Some(at)) = (parse_time(since), parse_time(at)) else {
        return false;
    };
    at - since >= time::Duration::hours(hours as i64)
}

/// How an entry is ordered in the snapshot ([`snapshot`]): the holds of
/// every new run first, then by repository and number.
fn published_order(hold: &Published) -> Option<(String, u64)> {
    let (repository, number) = hold.item.as_deref()?.rsplit_once('#')?;
    Some((repository.to_ascii_lowercase(), number.parse().ok()?))
}

/// Whether a published entry is one of stage 4's: an item's hold, or the
/// one for a parent that is not a live epic, not an alarm.
fn of_stage_4(hold: &Published) -> bool {
    hold.item.is_some()
        && (!hold.epic || hold.kind == HoldKind::EpicNotLive)
        && hold.kind != HoldKind::Stalled
}

/// `text` cut to `limit` characters, marked where it was.
fn clip(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// Of stage 4's `ranked` candidates, those free to start, for the idle
/// alarm: all but those its `entries` hold for a reason that waits by
/// design, as [`HoldKind::stalls`] has it, or on the operator. A candidate
/// in flight, with a pull request open or waiting on another is no stall.
/// Where a hold of every new run (pools, say) stopped it partway, only the
/// candidates held for a reason that stalls count: the rest wait on
/// pacing. Holds of items that were never candidates count for nothing.
pub fn free(ranked: Option<&[ItemRef]>, entries: &[Entry]) -> Option<usize> {
    let ranked: BTreeSet<(String, u64)> = ranked?.iter().map(order).collect();
    let items = |stuck: bool| -> BTreeSet<(String, u64)> {
        (entries.iter())
            .filter(|e| matches!(e.scope, Scope::Item(_)))
            .filter(|e| (!e.hold.operator && e.hold.kind.stalls()) == stuck)
            .filter_map(|e| e.item().map(order))
            .filter(|item| ranked.contains(item))
            .collect()
    };
    let paced = entries.iter().any(|e| e.scope == Scope::Pass);
    Some(match paced {
        true => items(true).len(),
        false => ranked.len() - items(false).len(),
    })
}

/// The holds of `previous`, the JSON of the published snapshot, if any.
fn previous_holds(previous: Option<&Value>) -> impl Iterator<Item = &Value> {
    (previous.filter(|s| s["schema"] == HOLDS_SCHEMA))
        .and_then(|s| s["holds"].as_array())
        .into_iter()
        .flatten()
}

/// `snapshot`, made at its `at`, with what carries over from `previous`,
/// the JSON of the published snapshot if any. Where stage 4 did not get as
/// far as its candidates (`considered` false: the WIP limit, pools or the
/// budget held every new run), its holds there are kept as they were, as
/// the last it read, with the alarms raised on them, but marked
/// [`Published::carried`] so that no new alarm is. Each hold that was there the same ([`Same`]) keeps
/// that one's `since`, and the pass's `progress_at` is the published one
/// unless this pass `progressed`; the rest stay at `at`. A time that does
/// not parse, or is later than `at`, carries nothing over.
pub fn carry(
    snapshot: &mut Snapshot,
    previous: Option<&Value>,
    (progressed, considered): (bool, bool),
) {
    let at = snapshot.at.clone();
    let carried = |since: &Value| not_after(since, &at);
    if !considered {
        let had: BTreeSet<(Option<String>, bool)> = (snapshot.holds.iter())
            .map(|h| (h.item.clone(), h.epic))
            .collect();
        let previous: Vec<Published> = previous_holds(previous)
            .filter_map(|h| serde_json::from_value::<Published>(h.clone()).ok())
            .collect();
        let kept: BTreeSet<(Option<String>, bool)> = (previous.iter())
            .filter(|h| of_stage_4(h) && !had.contains(&(h.item.clone(), h.epic)))
            .map(|h| (h.item.clone(), h.epic))
            .collect();
        let kept: Vec<Published> = (previous.into_iter())
            .filter(|h| kept.contains(&(h.item.clone(), h.epic)))
            .map(|h| Published { carried: true, ..h })
            .collect();
        if !kept.is_empty() {
            snapshot.holds.extend(kept);
            // Stable: the holds of every new run keep the order decided.
            snapshot.holds.sort_by_key(published_order);
        }
    }
    let mut since: BTreeMap<Same, String> = BTreeMap::new();
    for hold in previous_holds(previous) {
        let (Some(kind), Some(reason), Some(at)) = (
            hold["kind"].as_str(),
            hold["reason"].as_str(),
            carried(&hold["since"]),
        ) else {
            continue;
        };
        let item = hold["item"].as_str().map(str::to_owned);
        let same = (
            item,
            hold["epic"] == true,
            kind.to_owned(),
            normalised(reason),
        );
        since.entry(same).or_insert(at);
    }
    for hold in snapshot.holds.iter_mut().filter(|h| !h.carried) {
        hold.since = since.get(&hold.same()).unwrap_or(&at).clone();
    }
    let progress = previous
        .filter(|s| s["schema"] == HOLDS_SCHEMA)
        .and_then(|s| carried(&s["pass"]["progress_at"]));
    if let (false, Some(progress)) = (progressed, progress) {
        snapshot.pass.progress_at = progress;
    }
}

/// What [`alarm`] watches for, and where it says so.
pub struct Watch<'a> {
    pub stall: &'a Stall,
    /// The board's first Priority options (P0 and P1), at which an item's
    /// hold may stall.
    pub top: &'a [String],
    /// The holds issue, which the idle alarm is on.
    pub on: &'a ItemRef,
    /// This pass's log, in GitHub Actions.
    pub log: Option<String>,
    /// Stage 4's candidates free to start ([`free`]); none where it did not
    /// get as far as its candidates.
    pub free: Option<usize>,
    /// The items, `OWNER/REPO#N`, held only by pull requests the pass saw
    /// move or that wait on a person ([`super::blockers`]), whose holds
    /// raise no alarm of their own.
    pub blocked: &'a BTreeSet<String>,
}

/// Adds the stall alarms to `snapshot`, once [`carry`] has carried over its
/// times from `previous`, and returns a `stall …` line each. Each is a
/// [`HoldKind::Stalled`] entry that waits on the operator, so that the
/// review app shows it as one of theirs:
///
/// - after each hold that [`HoldKind::stalls`], on an epic or an item at a
///   `top` Priority, does not already wait on the operator, was read by
///   this pass and has been the same for `holdHours` or more, one on its
///   item since then; past [`MAX_STALLS`] of them, one on the holds issue
///   that counts the rest;
/// - where the pass has candidates free to start ([`free`]) and no progress
///   for `idleHours` or more, one on the holds issue, since its last
///   progress, naming the log of the pass that first said so, which it
///   keeps while the alarm stands.
///
/// The alarms' reasons count no time, and repeat the hold's reason with
/// its times set aside ([`normalised`]), so that a snapshot that stays
/// stalled stays the same.
pub fn alarm(snapshot: &mut Snapshot, previous: Option<&Value>, watch: &Watch) -> Vec<String> {
    let at = snapshot.at.clone();
    let hold_hours = watch.stall.hold_hours;
    let item = watch.on.display();
    let on_holds_issue = |reason: String, since: String| Published {
        item: Some(item.clone()),
        url: Some(url(watch.on)),
        epic: false,
        kind: HoldKind::Stalled,
        reason,
        operator: true,
        priority: None,
        question: None,
        since,
        carried: false,
        watched: false,
    };
    let mut lines = Vec::new();
    let mut holds = Vec::with_capacity(snapshot.holds.len());
    let (mut raised, mut more) = (0, Vec::new());
    for hold in std::mem::take(&mut snapshot.holds) {
        let priority = (hold.priority.as_ref()).is_some_and(|p| watch.top.contains(p));
        let watched =
            hold.item.is_some() && hold.kind.stalls() && !hold.operator && (hold.epic || priority);
        let hold = Published { watched, ..hold };
        let stalled = (hold.item.clone())
            .filter(|_| watched && !hold.carried && over(&hold.since, &at, hold_hours))
            .filter(|item| !watch.blocked.contains(item));
        let Some(stalled) = stalled else {
            holds.push(hold);
            continue;
        };
        let reason = format!(
            "the same hold for {hold_hours} hours or more, since {}: {}",
            hold.since,
            clip(&normalised(&hold.reason), MAX_STALL_REASON)
        );
        let epic = if hold.epic { "epic " } else { "" };
        lines.push(format!("stall hold {epic}{stalled}: {reason}"));
        if raised == MAX_STALLS {
            more.push((stalled, hold.since.clone()));
            holds.push(hold);
            continue;
        }
        raised += 1;
        let alarm = Published {
            kind: HoldKind::Stalled,
            reason,
            operator: true,
            question: None,
            ..hold.clone()
        };
        holds.push(hold);
        holds.push(alarm);
    }
    snapshot.holds = holds;
    if let Some(since) = more.iter().map(|(_, since)| since.clone()).min() {
        let named: Vec<&str> = (more.iter().take(MAX_NAMED))
            .map(|(i, _)| i.as_str())
            .collect();
        let mut reason = format!(
            "{} more hold(s) the same for {hold_hours} hours or more, among them {}",
            more.len(),
            named.join(", ")
        );
        if more.len() > MAX_NAMED {
            reason.push_str(&format!(" and {} more", more.len() - MAX_NAMED));
        }
        reason.push_str("; the pass's stall lines name them all");
        snapshot.holds.push(on_holds_issue(reason, since));
    }
    let pass = &snapshot.pass;
    let idle_hours = watch.stall.idle_hours;
    let idle = over(&pass.progress_at, &at, idle_hours);
    if let (true, Some(candidates @ 1..)) = (idle, watch.free) {
        let since = pass.progress_at.clone();
        let stands =
            format!("no apply, dispatch or merge for {idle_hours} hours or more, since {since}");
        let said = format!("{stands}, with {candidates} candidate(s) free to start");
        // The log of the pass that first said so, while the alarm stands.
        let first = previous_holds(previous)
            .filter(|h| h["kind"] == HoldKind::Stalled.name() && h["item"] == item.as_str())
            .filter(|h| h["since"] == since.as_str())
            .filter_map(|h| h["reason"].as_str())
            .filter(|r| r.starts_with(&format!("{stands}, ")))
            .find_map(|r| r.rsplit_once(PASS_LOG))
            .map(|(_, log)| log.to_owned());
        let reason = match first.or_else(|| watch.log.clone()) {
            Some(log) => format!("{said}{PASS_LOG}{log}"),
            None => said,
        };
        lines.push(format!("stall idle: {reason}"));
        snapshot.holds.push(on_holds_issue(reason, since));
    }
    lines
}

/// How much of one source of the needs-you list this pass read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Read {
    /// Read whole: nothing from it waits on the operator but what is listed.
    Complete,
    /// Read, but some of its issues not (`unreadable` holds), or, in stage
    /// 4, some candidates not considered; any of them may wait on the
    /// operator.
    Partial,
    /// Not read by this pass; for stage 4, what it lists is the last a pass
    /// read ([`carry`]).
    Unread,
    /// Not configured.
    Off,
}

/// Each source of the needs-you list: stage 3's epics, stage 4's items,
/// stage 2's followed pull requests (`reviews`), the operator-action
/// proposals (`proposals`) and the newest maintain digest (`digest`). The
/// pass's own alarms on the holds issue are read whenever it publishes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Sources {
    pub epics: Read,
    pub items: Read,
    pub reviews: Read,
    pub proposals: Read,
    pub digest: Read,
    #[serde(flatten)]
    pub inbox: BTreeMap<String, Read>,
}

/// The `source` of the asks each of [`Reading`]'s own sources lists.
pub const REVIEWS: &str = "reviews";
pub const PROPOSALS: &str = "proposals";
pub const DIGEST: &str = "digest";
pub const DECISIONS: &str = "decisions";
pub const DRAFTS: &str = "drafts";

/// What a [`Reference`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    /// An issue to act on: answer, label or decide.
    Issue,
    /// A pull request to act on: answer it, or take its label off.
    PullRequest,
    /// Approve a pull request at `head`.
    ApprovePullRequest,
    /// Review a pull request at `head`.
    ReviewPullRequest,
    /// Reply to a pull request's author.
    ReplyPullRequest,
    /// Close a stale pull request.
    ClosePullRequest,
    /// Check and apply the operator-action proposal in `comment`.
    OperatorAction,
    /// Source: a hold of the pass on the issue.
    Hold,
    /// Source: the comment `comment` on the issue.
    Comment,
}

/// What to act on, or where an ask was read: typed data, which a reader
/// makes its own link from, never an address to follow.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    #[serde(rename = "type")]
    pub kind: RefKind,
    pub repository: String,
    pub number: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

impl Reference {
    pub fn new(kind: RefKind, repository: &str, number: u64) -> Self {
        Self {
            kind,
            repository: repository.to_owned(),
            number,
            comment: None,
            head: None,
        }
    }

    /// Whether it names a valid repository, number, comment and full head
    /// commit, as one published before must to be kept ([`published_asks`]).
    fn valid(&self) -> bool {
        validate_repository(&self.repository).is_ok()
            && self.number > 0
            && self.comment.is_none_or(|c| c > 0)
            && (self.head.as_deref()).is_none_or(full_head)
    }

    /// The address it is on, made from its own fields.
    pub fn url(&self) -> String {
        let pulls = matches!(
            self.kind,
            RefKind::PullRequest
                | RefKind::ApprovePullRequest
                | RefKind::ReviewPullRequest
                | RefKind::ReplyPullRequest
                | RefKind::ClosePullRequest
        );
        let path = if pulls { "pull" } else { "issues" };
        let anchor = (self.comment.map(|c| format!("#issuecomment-{c}"))).unwrap_or_default();
        format!(
            "https://{DEFAULT_HOST}/{}/{path}/{}{anchor}",
            self.repository, self.number
        )
    }
}

/// Whether `head` is a full head commit, 40 lower-case hex digits.
pub fn full_head(head: &str) -> bool {
    head.len() == 40 && head.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// One thing that waits on the operator: a hold of theirs, one an item; a
/// followed pull request's wait; an operator-action proposal, one a
/// comment; or a digest entry, one a pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ask {
    /// The same for the ask from pass to pass, lower case: `owner/repo#n`
    /// for a hold or a review wait, with `/comment-ID` for a proposal and
    /// `/digest` for a digest entry.
    pub key: String,
    /// `OWNER/REPO#N` of the issue or pull request it is on.
    pub item: String,
    /// `epics`, `items`, `pass` for an alarm on the holds issue, or one of
    /// [`REVIEWS`], [`PROPOSALS`] and [`DIGEST`].
    pub source: String,
    /// The hold's kind, or `review-…`, `operator-action` or `digest-…`.
    pub kind: String,
    pub priority: Option<String>,
    /// The hold's `since` ([`carry`]); for the others, when the pass first
    /// saw it at this head, or its comment's time.
    pub waiting_since: String,
    /// The one-line question.
    pub ask: String,
    /// The one-line recommendation, where one was given.
    pub recommendation: Option<String>,
    /// [`Reference::url`] of `action`.
    pub target: String,
    /// The head commit the ask is on, where it is on one.
    pub head: Option<String>,
    pub action: Reference,
    pub source_ref: Reference,
    /// Kept from the published list, as its source was not read again
    /// whole ([`Collected::unread`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub carried: bool,
    /// Optional v2 extension: stranded PRs represented by this card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<AskGroup>,
    #[serde(default, flatten)]
    pub chain: EpicChain,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EpicChain {
    pub epic: String,
    #[serde(rename = "topEpic")]
    pub top_epic: String,
}

impl Default for EpicChain {
    fn default() -> Self {
        Self {
            epic: "unknown".into(),
            top_epic: "unknown".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskGroup {
    pub reason: String,
    pub numbers: Vec<u64>,
}

pub mod requests;

/// The needs-you list ([`NEEDS_YOU_SCHEMA`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NeedsYou {
    /// Fresh per-member informational assessments, never carried from old asks.
    /// Absent members (including unread/carried ones) are unknown, not quick.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub review_buckets: BTreeMap<String, crate::poll::drafts::Assessment>,
    pub schema: &'static str,
    /// Whether every source was read whole ([`Read::Complete`] or off) and
    /// none was left out, so that an empty list means nothing waits.
    pub complete: bool,
    /// The clock of the pass that last read it, which may only confirm it.
    pub validated_at: String,
    /// The clock of the pass that last changed it, `validated_at` aside.
    pub changed_at: String,
    pub sources: Sources,
    /// How much of the bot's summaries of the held issues this pass read:
    /// they enrich asks, but are no source of them.
    pub summaries: Read,
    /// The lowest ranked decisions whose identities were left out because
    /// even the compact inventory, with no detail card left, did not fit
    /// ([`fit`]); `complete` is then false. Otherwise zero.
    pub omitted: usize,
    /// `omitted` by each left-out identity's `source`; absent where none is.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub omitted_sources: BTreeMap<String, usize>,
    /// How many decisions, including identities whose detail did not fit.
    pub count: usize,
    /// Canonical decisions represented before display truncation. On a partial
    /// read these are known counts, never an assertion about unread sources.
    pub counts: Counts,
    /// Non-actionable observations, deliberately excluded from all ask counts.
    pub context: ContextFold,
    pub items: Vec<Ask>,
    /// Ranked decision inventory; incomplete when `omitted` is nonzero.
    pub identities: Vec<Identity>,
    pub epics: BTreeMap<String, EpicActivity>,
    pub details_omitted: usize,
    /// Optional list-level extension; strict v2 Ask records are unchanged.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub requests: Vec<requests::Request>,
    /// Source-specific carry state, including aliases hidden by semantic dedupe.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub request_sources: Vec<requests::SourceAsk>,
    /// Optional v2 extension; bodies are reused only after metadata validation.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub summary_cache: BTreeMap<String, Summary>,
    /// Per-target cheap carry check; unknown retains the last check clock.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub carry_validation: BTreeMap<String, Value>,
    /// Optional extension with `holds.approvalEvidence`: absent where it is
    /// off or stage 2 was not read this pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_evidence: Option<ApprovalEvidence>,
}

/// The most approval rows [`ApprovalEvidence`] describes, highest ranked
/// first; the rest are counted in its `omitted`.
pub const MAX_APPROVAL_EVIDENCE: usize = 50;

/// Evidence for the uncarried approval rows of the list, by their `key`,
/// each at the head its row asks to approve; only this pass's reads, never
/// a previous list's. A row without an entry has unknown evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ApprovalEvidence {
    /// [`approval::EVIDENCE_SCHEMA`].
    pub schema: &'static str,
    /// The pass clock it was read at, which alone is no change.
    pub observed_at: String,
    /// Rows whose evidence was read but left out: past
    /// [`MAX_APPROVAL_EVIDENCE`], or by [`fit`] for size.
    pub omitted: usize,
    pub items: BTreeMap<String, approval::Evidence>,
}

impl ApprovalEvidence {
    /// The evidence `fresh` read for `items`' approval rows: a row carried
    /// from an earlier list, grouped, or at another head than the evidence
    /// has none.
    fn of(items: &[Ask], fresh: &BTreeMap<String, approval::Evidence>, at: &str) -> Self {
        let mut evidence = Self {
            schema: approval::EVIDENCE_SCHEMA,
            observed_at: at.to_owned(),
            omitted: 0,
            items: BTreeMap::new(),
        };
        for ask in items {
            let Some(read) = fresh.get(&ask.key).filter(|e| bound(ask, e)) else {
                continue;
            };
            if evidence.items.len() < MAX_APPROVAL_EVIDENCE {
                evidence.items.insert(ask.key.clone(), read.clone());
            } else {
                evidence.omitted += 1;
            }
        }
        evidence
    }
}

/// Whether `evidence` may describe `ask`: an uncarried, ungrouped approval
/// row of stage 2 at the evidence's head.
fn bound(ask: &Ask, evidence: &approval::Evidence) -> bool {
    !ask.carried
        && ask.group.is_none()
        && ask.source == REVIEWS
        && ask.action.kind == RefKind::ApprovePullRequest
        && ask.head.as_deref() == Some(evidence.assessed_head.as_str())
}

/// One decision of the inventory, published in its compact form
/// ([`CompactIdentity`]) and read back in either form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "CompactIdentity", into = "CompactIdentity")]
pub struct Identity {
    pub key: String,
    /// Detailed card representing this identity, possibly a grouped card.
    pub detail_key: String,
    pub item: String,
    pub kind: String,
    pub source: String,
    pub waiting_since: String,
    pub carried: bool,
    /// A review wait backed by needs-human, not an unlabelled poll refusal.
    /// Older publications lack this evidence and require a full source read.
    pub label_backed: bool,
    pub priority: Option<String>,
    pub epic: String,
    pub top_epic: String,
}

/// [`Identity`] as published: `key` is left out where it is `item` in lower
/// case, `detail_key` where it is `key`, `topEpic` where it is `epic`,
/// `priority` where there is none and `carried` where false, so that far
/// more decisions fit in one comment. The full form, as published before,
/// reads the same.
#[derive(Serialize, Deserialize)]
struct CompactIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail_key: Option<String>,
    item: String,
    kind: String,
    source: String,
    waiting_since: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    carried: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    label_backed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    priority: Option<String>,
    epic: String,
    #[serde(rename = "topEpic", default, skip_serializing_if = "Option::is_none")]
    top_epic: Option<String>,
}

impl From<CompactIdentity> for Identity {
    fn from(c: CompactIdentity) -> Self {
        let key = c.key.unwrap_or_else(|| c.item.to_ascii_lowercase());
        Self {
            detail_key: c.detail_key.unwrap_or_else(|| key.clone()),
            key,
            top_epic: c.top_epic.unwrap_or_else(|| c.epic.clone()),
            item: c.item,
            kind: c.kind,
            source: c.source,
            waiting_since: c.waiting_since,
            carried: c.carried,
            label_backed: c.label_backed,
            priority: c.priority,
            epic: c.epic,
        }
    }
}

impl From<Identity> for CompactIdentity {
    fn from(i: Identity) -> Self {
        Self {
            detail_key: (i.detail_key != i.key).then_some(i.detail_key),
            key: (i.key != i.item.to_ascii_lowercase()).then_some(i.key),
            top_epic: (i.top_epic != i.epic).then_some(i.top_epic),
            item: i.item,
            kind: i.kind,
            source: i.source,
            waiting_since: i.waiting_since,
            carried: i.carried,
            label_backed: i.label_backed,
            priority: i.priority,
            epic: i.epic,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpicActivity {
    pub active: bool,
    #[serde(rename = "activeWhy")]
    pub active_why: Vec<String>,
    pub complete: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub decisions: usize,
    pub groups: usize,
    pub members: usize,
}

/// The most context facts of each source: holds, steps, runs.
const MAX_CONTEXT: [usize; 3] = [20, 10, 10];

/// The `source` of each [`ContextFact`], in the order `context.items`
/// lists them, so that [`fit`] cuts runs first and holds last.
pub const CONTEXT_HOLD: &str = "hold";
pub const CONTEXT_STEP: &str = "step";
pub const CONTEXT_RUN: &str = "run";
/// The `context.sources` key of the usage snapshot `context.limits` reads.
const CONTEXT_USAGE: &str = "usage";

/// The usage snapshot's fields `context.limits` keeps of a held pool, and
/// of the budget; the rest is its pacing's own detail.
const POOL_FIELDS: &[&str] = &[
    "reason",
    "used_percent",
    "limit_percent",
    "resets_at",
    "next_dispatch_at",
    "observed_at",
];
const BUDGET_FIELDS: &[&str] = &["reason", "weekly_usd", "spent_usd", "since"];

/// The `context.limits` key of the budget, beside the pools' names.
pub const BUDGET_LIMIT: &str = "budget";

/// The limits' readings that move from pass to pass: like observation
/// clocks, they alone are no change ([`requests::strip_observation_times`]).
pub(super) const LIMIT_READINGS: &[&str] = &[
    "used_percent",
    "limit_percent",
    "resets_at",
    "next_dispatch_at",
    "observed_at",
    "spent_usd",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ContextFold {
    pub wip: Wip,
    pub observed_at: String,
    /// How much of each source besides the holds this pass read: an unread
    /// one says nothing of what it would have listed.
    pub sources: BTreeMap<&'static str, Read>,
    pub omitted: usize,
    pub items: Vec<ContextFact>,
    /// The pools that hold new runs, and the budget where it does, by name.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub limits: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ContextFact {
    /// [`CONTEXT_HOLD`], [`CONTEXT_STEP`] or [`CONTEXT_RUN`].
    pub source: &'static str,
    pub item: Option<String>,
    /// A hold's kind, a step's [`poll::Progress`] or a run's conclusion.
    pub kind: String,
    pub reason: String,
    pub url: Option<String>,
    /// When it began, where its source says: a hold's `since`, when a
    /// step's head last moved, a run's last update.
    pub since: Option<String>,
    pub observed_at: Option<String>,
}

/// What the pass observed besides its holds, for the `context` fold, with
/// how much of each it read; none of it is an ask.
#[derive(Clone, Debug)]
pub struct Observed {
    /// Stage 2's step for each followed pull request no person moves.
    pub steps: (Read, Vec<ContextFact>),
    /// The caller's runs that failed in the last day.
    pub runs: (Read, Vec<ContextFact>),
    /// The usage snapshot the pass paced by.
    pub usage: (Read, Option<Value>),
}

impl Observed {
    pub fn off() -> Self {
        Self {
            steps: (Read::Off, Vec::new()),
            runs: (Read::Off, Vec::new()),
            usage: (Read::Off, None),
        }
    }
}

/// The held pools of the usage snapshot `usage`, and its budget where a
/// budget hold says it holds, each cut to the fields that say why and until
/// when.
fn limits(usage: Option<&Value>, budget_held: bool) -> BTreeMap<String, Value> {
    let keep = |entry: &Value, fields: &[&str]| {
        let kept = fields
            .iter()
            .filter_map(|f| Some((f.to_string(), entry.get(*f)?.clone())));
        Value::Object(kept.collect())
    };
    let Some(usage) = usage else {
        return BTreeMap::new();
    };
    let pools = usage["pools"].as_object().into_iter().flatten();
    let mut limits: BTreeMap<_, _> = pools
        .filter(|(_, p)| p["hold"] == true)
        .map(|(name, p)| (name.clone(), keep(p, POOL_FIELDS)))
        .collect();
    if budget_held && usage["budget"].is_object() {
        limits.insert(BUDGET_LIMIT.into(), keep(&usage["budget"], BUDGET_FIELDS));
    }
    limits
}

fn context(snapshot: &Snapshot, previous: Option<&Value>, observed: &Observed) -> ContextFold {
    let previous = published_list(previous);
    let holds: Vec<_> = snapshot
        .holds
        .iter()
        .filter(|h| !h.operator && h.kind != HoldKind::Closed)
        .map(|h| {
            let reason = one_line(&h.reason, MAX_ASK).unwrap_or_default();
            let observed_at = if h.carried {
                previous
                    .and_then(|p| p["context"]["items"].as_array())
                    .into_iter()
                    .flatten()
                    .find(|p| {
                        p["source"] == CONTEXT_HOLD
                            && p["item"].as_str() == h.item.as_deref()
                            && p["kind"] == h.kind.name()
                            && p["reason"] == reason.as_str()
                    })
                    .and_then(|p| not_after(&p["observed_at"], &snapshot.at))
            } else {
                Some(snapshot.at.clone())
            };
            ContextFact {
                source: CONTEXT_HOLD,
                item: h.item.clone(),
                kind: h.kind.name().into(),
                reason,
                url: h.url.clone(),
                since: Some(h.since.clone()).filter(|s| !s.is_empty()),
                observed_at,
            }
        })
        .collect();
    let mut omitted = 0;
    let mut items = Vec::new();
    let all = [holds, observed.steps.1.clone(), observed.runs.1.clone()];
    for (mut facts, max) in all.into_iter().zip(MAX_CONTEXT) {
        facts.sort_by(|a, b| (&a.item, &a.kind, &a.reason).cmp(&(&b.item, &b.kind, &b.reason)));
        omitted += facts.len().saturating_sub(max);
        items.extend(facts.into_iter().take(max));
    }
    let budget_held = snapshot.holds.iter().any(|h| h.kind == HoldKind::Budget);
    ContextFold {
        wip: snapshot.wip.clone(),
        observed_at: snapshot.at.clone(),
        sources: BTreeMap::from([
            (CONTEXT_STEP, observed.steps.0),
            (CONTEXT_RUN, observed.runs.0),
            (CONTEXT_USAGE, observed.usage.0),
        ]),
        omitted,
        items,
        limits: limits(observed.usage.1.as_ref(), budget_held),
    }
}

impl NeedsYou {
    /// Whether the card `ask` is an actionable approval: a descriptor of its
    /// own offers the supported review at its full head. Only the descriptor
    /// says so, never the card's path or kind alone.
    fn actionable_approval(&self, ask: &Ask) -> bool {
        ask.action.kind == RefKind::ApprovePullRequest
            && (ask.action.head.as_deref()).is_some_and(full_head)
            && ask.head == ask.action.head
            && self.requests.iter().any(|r| {
                r.key == ask.key
                    && r.target == ask.action
                    && (r.actions.iter()).any(|a| a.kind == requests::REVIEW_AT_HEAD && a.supported)
            })
    }

    /// Evicts detail only; the complete identity inventory stays intact.
    /// The lowest ranked card goes, actionable approvals
    /// ([`Self::actionable_approval`]) only once no other card is left, so
    /// that the rest keep Priority, then age, order.
    fn cut(&mut self) -> Option<Ask> {
        let at = (self.items.iter())
            .rposition(|a| !self.actionable_approval(a))
            .or_else(|| self.items.len().checked_sub(1))?;
        let ask = self.items.remove(at);
        let repo = ask.action.repository.to_ascii_lowercase();
        let numbers = ask
            .group
            .as_ref()
            .map(|g| g.numbers.clone())
            .unwrap_or_else(|| vec![ask.action.number]);
        for number in numbers {
            self.review_buckets.remove(&format!("{repo}#{number}"));
        }
        if let Some(evidence) = &mut self.approval_evidence
            && evidence.items.remove(&ask.key).is_some()
        {
            evidence.omitted += 1;
        }
        self.requests.retain(|r| r.key != ask.key);
        self.request_sources.retain(|r| r.key != ask.key);
        self.summary_cache.remove(&ask.key);
        self.details_omitted = self.omitted
            + (self.identities)
                .iter()
                .filter(|i| !self.items.iter().any(|a| a.key == i.detail_key))
                .count();
        Some(ask)
    }

    /// Leaves out the lowest ranked row's approval evidence, counted in its
    /// `omitted`, before [`fit`] leaves out any hold or detail card: it is
    /// detail of a row, never a decision. False where none is left.
    pub(super) fn evict_evidence(&mut self) -> bool {
        let Some(evidence) = self.approval_evidence.as_mut() else {
            return false;
        };
        let lowest = (self.items.iter().rev())
            .map(|a| &a.key)
            .find(|k| evidence.items.contains_key(*k))
            .or_else(|| evidence.items.keys().next_back())
            .cloned();
        let Some(key) = lowest else {
            return false;
        };
        evidence.items.remove(&key);
        evidence.omitted += 1;
        true
    }

    /// Rebinds this pass's observations after identity revalidation, then
    /// applies the cap. Removed rows must neither consume slots nor count
    /// as omitted qualifying evidence. No previous publication is reused.
    pub(super) fn retain_evidence(&mut self, fresh: Option<&BTreeMap<String, approval::Evidence>>) {
        self.approval_evidence =
            fresh.map(|fresh| ApprovalEvidence::of(&self.items, fresh, &self.validated_at));
    }

    fn evidence_len(&self) -> usize {
        self.approval_evidence.as_ref().map_or(0, |e| e.items.len())
    }
}

/// What one of the needs-you list's own sources read this pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collected {
    pub review_buckets: BTreeMap<String, crate::poll::drafts::Assessment>,
    /// Stage 2's evidence for its approval waits, by key: none where it
    /// read none, or `holds.approvalEvidence` is off.
    pub approval_evidence: Option<BTreeMap<String, approval::Evidence>>,
    pub read: Read,
    pub asks: Vec<Ask>,
    /// Where `read` is partial, the repositories (`owner/repo`) and items
    /// (`owner/repo#n`), lower case, not read, whose published asks are
    /// kept; where it is unread, all of them are.
    pub unread: BTreeSet<String>,
}

impl Collected {
    pub fn off() -> Self {
        Self::of(Read::Off)
    }

    pub fn of(read: Read) -> Self {
        Self {
            read,
            review_buckets: BTreeMap::new(),
            approval_evidence: None,
            asks: Vec::new(),
            unread: BTreeSet::new(),
        }
    }
}

/// A trusted bot summary's one-line question and recommendation
/// ([`summary`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub question: Option<String>,
    pub recommendation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

/// What [`needs_you`] knows of the pass besides its snapshot.
pub struct Reading<'a> {
    pub operator: &'a str,
    /// Whether stage 3 is configured (`epics`).
    pub epics: bool,
    /// Whether stage 4 decided a hold, or a start, for each of its ranked
    /// candidates.
    pub considered: bool,
    /// The board's Priority options in order; none where it was not read.
    pub options: &'a [String],
    /// The holds issue.
    pub on: &'a ItemRef,
    pub reviews: Collected,
    pub proposals: Collected,
    pub digest: Collected,
    pub inbox: BTreeMap<String, Collected>,
    pub observed: Observed,
    /// How much of the summaries was read, and each read by its issue's
    /// key, none where the issue has none.
    pub summaries: (Read, BTreeMap<String, Option<Summary>>),
}

/// What a bot summary `body` asks and recommends, read as data: the rest
/// of its first `**Decision needed:**` and `**Recommendation:**` line (the
/// colon inside or outside the bold, the line perhaps a list item), one
/// line each. One that is empty is absent, so that the hold's own question
/// or reason stands.
pub fn summary(body: &str) -> Summary {
    let field = |labels: &[&str]| {
        body.lines()
            .map(|l| l.trim().strip_prefix("- ").unwrap_or(l.trim()))
            .find_map(|l| {
                let (label, rest) = l.split_once(':')?;
                let label = label.trim().trim_matches('*').trim();
                labels
                    .iter()
                    .any(|s| label.eq_ignore_ascii_case(s))
                    .then(|| rest.trim_start_matches('*').trim())
            })
            .and_then(|rest| one_line(rest, MAX_ASK))
    };
    Summary {
        question: field(&["Decision needed", "Question for the operator"]),
        recommendation: field(&["Recommendation", "Recommended"]),
        revision: None,
    }
}

/// The first line of `text` with text on it, cut to `limit` characters;
/// none where it has none.
pub fn one_line(text: &str, limit: usize) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(clip(line, limit))
}

/// The pull request `OWNER/REPO#N` whose approval a hold's `reason` waits
/// on ([`APPROVE_PREFIX`]).
fn approval_of(reason: &str) -> Option<&str> {
    reason
        .strip_prefix(APPROVE_PREFIX)?
        .strip_suffix(APPROVE_SUFFIX)
}

/// The needs-you list of `previous`, the published snapshot's JSON, if any.
fn published_list(previous: Option<&Value>) -> Option<&Value> {
    (previous.filter(|s| s["schema"] == HOLDS_SCHEMA))
        .map(|s| &s["needs_you"])
        .filter(|n| n["schema"] == NEEDS_YOU_SCHEMA)
}

/// The asks of the published list `previous` from `sources`, where each is
/// still well formed: its references valid, its `target` made again from
/// its action and its texts cut again.
fn published_asks(previous: Option<&Value>, sources: &[&str]) -> Vec<Ask> {
    let list = published_list(previous);
    let items = list
        .and_then(|n| n["items"].as_array())
        .into_iter()
        .flatten();
    let aliases = list
        .and_then(|n| n["request_sources"].as_array())
        .into_iter()
        .flatten()
        .map(|source| &source["ask"]);
    let aliases: Vec<Ask> = aliases
        .filter_map(|a| serde_json::from_value::<Ask>(a.clone()).ok())
        .filter(|a| {
            a.action.valid() && a.source_ref.valid() && parse_time(&a.waiting_since).is_some()
        })
        .collect();
    let items = items.filter(|item| {
        !aliases
            .iter()
            .any(|a| item["key"] == a.key && item["source"] == a.source)
    });
    aliases
        .iter()
        .cloned()
        .chain(items.filter_map(|a| serde_json::from_value::<Ask>(a.clone()).ok()))
        .filter(|a| sources.contains(&a.source.as_str()))
        .filter(|a| a.action.valid() && a.source_ref.valid())
        .filter(|a| parse_time(&a.waiting_since).is_some())
        .filter_map(|a| {
            Some(Ask {
                target: group_target(&a).unwrap_or_else(|| a.action.url()),
                ask: one_line(&a.ask, MAX_ASK)?,
                recommendation: (a.recommendation.as_deref()).and_then(|r| one_line(r, MAX_ASK)),
                ..a
            })
        })
        .collect()
}

/// Whether the time `a` is earlier than `b`; not where either is no time.
fn earlier(a: &str, b: &str) -> bool {
    matches!((parse_time(a), parse_time(b)), (Some(a), Some(b)) if a < b)
}

/// The group reason of stranded drafts, whose target lists only drafts.
const STRANDED_DRAFTS: &str = "drafts to inspect and promote";

/// The group reason of clean, green midstream drafts (`review-promote`),
/// whose target lists only drafts too.
const PROMOTE_DRAFTS: &str = "drafts waiting for promotion";

/// A list URL made only from validated repository/PR identities. Older v2
/// consumers ignore `group` and can still open the representative PR action.
fn group_target(ask: &Ask) -> Option<String> {
    let group = ask.group.as_ref()?;
    if group.numbers.is_empty() || group.numbers.contains(&0) {
        return None;
    }
    let query = format!(
        "is:pr is:open{}",
        if group.reason == STRANDED_DRAFTS || group.reason == PROMOTE_DRAFTS {
            " draft:true"
        } else {
            ""
        }
    );
    let query: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    Some(format!(
        "https://{DEFAULT_HOST}/{}/pulls?q={query}",
        ask.action.repository
    ))
}

/// Stranded PRs are housekeeping, not one independent decision per head.
/// Keep the lowest numbered PR as a parser-compatible representative and
/// publish membership optionally; never combine approvals with these groups.
/// Midstream drafts waiting for promotion are grouped the same way.
fn group_stranded(items: Vec<Ask>) -> Vec<Ask> {
    const NEEDS_HUMAN: &str = "review-needs-human";
    const PROMOTE: &str = "review-promote";
    let mut kept = Vec::new();
    let mut groups: BTreeMap<(String, String), Vec<Ask>> = BTreeMap::new();
    for ask in items {
        if ask.source != REVIEWS || !matches!(ask.kind.as_str(), NEEDS_HUMAN | PROMOTE) {
            kept.push(ask);
            continue;
        }
        let reason = ask
            .group
            .as_ref()
            .map(|g| g.reason.clone())
            .unwrap_or_else(|| {
                if ask.kind == PROMOTE {
                    PROMOTE_DRAFTS.into()
                } else if ask.ask.contains("draft with no checkpoint")
                    || ask.ask.starts_with("a stranded draft;")
                {
                    STRANDED_DRAFTS.into()
                } else if ask.ask.starts_with("fix round ") && ask.ask.contains(" is blocked:") {
                    "fix round blocked".into()
                } else if ask.ask.contains("fix rounds") && ask.ask.contains(" used") {
                    "fix rounds exhausted".into()
                } else {
                    ask.ask.clone()
                }
            });
        groups
            .entry((ask.action.repository.to_ascii_lowercase(), reason))
            .or_default()
            .push(ask);
    }
    for ((_, reason), mut asks) in groups {
        asks.sort_by_key(|a| a.action.number);
        let mut representative = asks[0].clone();
        let mut numbers = BTreeSet::new();
        for ask in asks {
            numbers.extend(
                ask.group
                    .map(|g| g.numbers)
                    .unwrap_or_else(|| vec![ask.action.number]),
            );
            representative.carried |= ask.carried;
            if earlier(&ask.waiting_since, &representative.waiting_since) {
                representative.waiting_since = ask.waiting_since;
            }
        }
        let repo = representative
            .action
            .repository
            .rsplit('/')
            .next()
            .unwrap_or_default();
        representative.ask = format!("{repo}: {} {reason}", numbers.len());
        representative.group = Some(AskGroup {
            reason,
            numbers: numbers.into_iter().collect(),
        });
        representative.target =
            group_target(&representative).unwrap_or_else(|| representative.action.url());
        kept.push(representative);
    }
    kept
}

/// What `snapshot`, once [`alarm`] has added its alarms, has wait on the
/// operator, with `reading`'s own sources: each hold with `operator` true
/// on an item, but not on a closed one; so not the holds the pass releases
/// itself (a run in flight or rerun, a cooldown, a retry), nor
/// `needs-design` or board Blocked. A hold's ask is its issue's bot
/// summary's question where it has one, else its board Why's, else its
/// reason; where its summary was not read, the same hold (key, kind and
/// `since`) keeps the question and recommendation published. A source's ask
/// keeps the `waiting_since` published for the same key, kind and head;
/// where a source was not read whole, the asks published from what it did
/// not read are kept, marked `carried`. Holds on the operator's approval of
/// one pull request are one ask, the most urgent's, waiting since the
/// earliest; with that pull request's review wait, live or carried, they are
/// one ask, the review's with the hold's Priority and question. Ranked by
/// Priority in the board's order (an unknown one after, none last; without
/// the board, in the published list's order), then the longest waiting,
/// then key; a key listed twice (the holds issue's two alarms) is listed
/// once, where it ranks first, its asks joined. `changed_at` carries over from
/// `previous`, the published snapshot's JSON, where the list was the same
/// but for `summaries`.
pub fn needs_you(snapshot: &Snapshot, previous: Option<&Value>, reading: &Reading) -> NeedsYou {
    let at = &snapshot.at;
    let on = reading.on.display();
    let unreadable = |epic: bool| {
        (snapshot.holds.iter())
            .any(|h| h.item.is_some() && h.epic == epic && h.kind == HoldKind::Unreadable)
    };
    let read = |listed: bool, epic: bool, whole: bool| match listed {
        false => Read::Unread,
        true if !whole || unreadable(epic) => Read::Partial,
        true => Read::Complete,
    };
    let sources = Sources {
        epics: match reading.epics {
            false => Read::Off,
            true => read(snapshot.pass.live_epics.is_some(), true, true),
        },
        // Where stage 4 did not get to its candidates, its holds are carried.
        items: read(
            snapshot.pass.candidates.is_some(),
            false,
            reading.considered,
        ),
        reviews: reading.reviews.read,
        proposals: reading.proposals.read,
        digest: reading.digest.read,
        inbox: reading
            .inbox
            .iter()
            .map(|(k, c)| (k.clone(), c.read))
            .collect(),
    };
    // Without the board's options, the order the published list ranked its
    // Priorities in stands in for them, so that the order does not move.
    let mut options = reading.options.to_vec();
    if options.is_empty() {
        let listed = (published_list(previous)
            .and_then(|n| n.get("identities").unwrap_or(&n["items"]).as_array()))
        .into_iter();
        for priority in listed.flatten().filter_map(|a| a["priority"].as_str()) {
            if !options.iter().any(|o| o == priority) {
                options.push(priority.to_owned());
            }
        }
    }
    let rank = |a: &Ask| {
        let since = parse_time(&a.waiting_since);
        let known = (a.priority.as_ref()).and_then(|p| options.iter().position(|o| o == p));
        let priority = (
            a.priority.is_none(),
            known.is_none(),
            known,
            a.priority.clone(),
        );
        (priority, since.is_none(), since, a.key.clone())
    };
    let published_holds = published_asks(previous, &["epics", "items"]);
    // Each hold's ask, with the review wait it is the same ask as, if any.
    let mut approvals: BTreeMap<String, (Ask, bool)> = BTreeMap::new();
    let mut items: Vec<Ask> = Vec::new();
    for h in (snapshot.holds.iter()).filter(|h| h.operator && h.kind != HoldKind::Closed) {
        let Some(item) = h.item.clone() else {
            continue;
        };
        let Some((repository, number)) =
            (item.rsplit_once('#')).and_then(|(r, n)| Some((r.to_owned(), n.parse::<u64>().ok()?)))
        else {
            continue;
        };
        let key = item.to_ascii_lowercase();
        let source = match h.epic && h.kind != HoldKind::EpicNotLive {
            _ if item == on => "pass",
            true => "epics",
            false => "items",
        };
        let said = match reading.summaries.1.get(&key) {
            Some(said) => said.clone().unwrap_or_default(),
            // Unread: as last published for the same hold with the same Why
            // question, where that was not the fallback to its reason.
            None => (published_holds.iter())
                .filter(|_| {
                    previous_holds(previous).any(|p| {
                        (p["item"] == item.as_str() && p["epic"] == h.epic)
                            && p["question"].as_str() == h.question.as_deref()
                    })
                })
                .find(|p| {
                    (p.key == key && p.kind == h.kind.name() && p.waiting_since == h.since)
                        && p.source_ref.kind == RefKind::Hold
                })
                .map(|p| Summary {
                    question: Some(p.ask.clone()).filter(|q| *q != h.reason),
                    recommendation: p.recommendation.clone(),
                    revision: None,
                })
                .unwrap_or_default(),
        };
        let asked = said.question.clone().or_else(|| h.question.clone());
        let kind = match h.kind {
            HoldKind::Blocking => RefKind::PullRequest,
            _ => RefKind::Issue,
        };
        let action = Reference::new(kind, &repository, number);
        let ask = Ask {
            key,
            item,
            source: source.into(),
            kind: h.kind.name().into(),
            priority: h.priority.clone(),
            waiting_since: h.since.clone(),
            ask: asked.clone().unwrap_or_else(|| h.reason.clone()),
            recommendation: said.recommendation,
            // A blocking hold's url is its pull request's issue address.
            target: match h.kind {
                HoldKind::Blocking => action.url(),
                _ => h.url.clone().unwrap_or_else(|| action.url()),
            },
            head: None,
            source_ref: Reference::new(RefKind::Hold, &repository, number),
            action,
            carried: h.carried,
            group: None,
            chain: EpicChain::default(),
        };
        let Some(pull) = approval_of(&h.reason).map(str::to_ascii_lowercase) else {
            items.push(ask);
            continue;
        };
        // Another hold on the same approval: one ask, the most urgent's.
        let mut joined = (ask, asked.is_some());
        if let Some(mut other) = approvals.remove(&pull) {
            if rank(&other.0) < rank(&joined.0) {
                std::mem::swap(&mut joined, &mut other);
            }
            if earlier(&other.0.waiting_since, &joined.0.waiting_since) {
                joined.0.waiting_since = other.0.waiting_since;
            }
        }
        approvals.insert(pull, joined);
    }
    let published = published_asks(previous, &[REVIEWS, PROPOSALS, DIGEST, DECISIONS, DRAFTS]);
    let own: Vec<_> = [
        (REVIEWS, &reading.reviews),
        (PROPOSALS, &reading.proposals),
        (DIGEST, &reading.digest),
    ]
    .into_iter()
    .chain(reading.inbox.iter().map(|(k, c)| (k.as_str(), c)))
    .collect();
    let mut asks: Vec<Ask> = (own.iter())
        .flat_map(|(_, c)| c.asks.iter().cloned())
        .collect();
    for (source, collected) in own {
        if matches!(collected.read, Read::Complete | Read::Off) {
            continue;
        }
        for old in published.iter().filter(|p| p.source == source) {
            let mut ask = old.clone();
            if let Some(group) = &mut ask.group {
                // A freshly read decision supersedes the old housekeeping
                // membership, even when another read in its repository failed.
                group.numbers.retain(|number| {
                    !collected.asks.iter().any(|fresh| {
                        fresh
                            .action
                            .repository
                            .eq_ignore_ascii_case(&ask.action.repository)
                            && fresh.action.number == *number
                    })
                });
                let Some(number) = group.numbers.iter().min().copied() else {
                    continue;
                };
                if number != ask.action.number {
                    ask.action.number = number;
                    ask.source_ref.number = number;
                    ask.item = format!("{}#{number}", ask.action.repository);
                    ask.key = ask.item.to_ascii_lowercase();
                    // Only the old representative's head was published.
                    ask.head = None;
                    ask.action.head = None;
                    ask.source_ref.head = None;
                }
            }
            let item = ask.item.to_ascii_lowercase();
            let repository = item.rsplit_once('#').map_or(item.as_str(), |(r, _)| r);
            let unread = collected.read == Read::Unread
                || collected.unread.contains(&item)
                || collected.unread.contains(repository);
            let listed = |a: &Ask| {
                a.source == ask.source
                    && a.head == ask.head
                    && requests::identity(a, &reading.on.host)
                        == requests::identity(&ask, &reading.on.host)
            };
            if unread
                && (ask.group.is_some() || (!items.iter().any(listed) && !asks.iter().any(listed)))
            {
                asks.push(Ask {
                    carried: true,
                    ..ask
                });
            }
        }
    }
    let evicted: Vec<Identity> = (published_list(previous))
        .and_then(|n| n["identities"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|i| serde_json::from_value::<Identity>(i.clone()).ok())
        .filter(|i| i.source == REVIEWS && !published.iter().any(|p| p.key == i.detail_key))
        .collect();
    // A carried ask is joined with its hold as a live one is.
    for mut ask in asks {
        // A labelled-list shortcut cannot recover why the poll originally
        // stopped. Keep its published category while it is still labelled;
        // fresh explicit reasons (especially drafts) take precedence, and
        // the poll reads a labelled draft as one, so this ask is no longer
        // a draft: never restore the drafts category, whose list hides it.
        if ask.source == REVIEWS
            && ask.kind == "review-needs-human"
            && ask.ask == "labelled needs-human"
            && let Some(group) = published
                .iter()
                .filter(|p| p.source == REVIEWS)
                .filter(|p| {
                    p.action
                        .repository
                        .eq_ignore_ascii_case(&ask.action.repository)
                })
                .filter_map(|p| p.group.as_ref())
                .filter(|g| g.reason != STRANDED_DRAFTS)
                .find(|g| g.numbers.contains(&ask.action.number))
        {
            ask.ask = group.reason.clone();
        }
        // A review wait whose card [`fit`] left out published no head: its
        // identity keeps its age, or each pass would restart it and so
        // change the list.
        let seen = (published.iter())
            .find(|p| p.key == ask.key && p.kind == ask.kind && p.head == ask.head)
            .map(|p| p.waiting_since.as_str())
            .or_else(|| {
                let source = |i: &&Identity| i.key == ask.key && i.kind == ask.kind;
                (ask.source == REVIEWS && ask.group.is_none())
                    .then(|| evicted.iter().find(source))
                    .flatten()
                    .map(|i| i.waiting_since.as_str())
            })
            .and_then(|since| not_after(&Value::from(since), at));
        if let Some(since) = seen.filter(|s| earlier(s, &ask.waiting_since)) {
            ask.waiting_since = since;
        }
        let hold = (ask.action.kind == RefKind::ApprovePullRequest && ask.source == REVIEWS)
            .then(|| approvals.remove(&ask.item.to_ascii_lowercase()))
            .flatten();
        if let Some((hold, asked)) = hold {
            ask.priority = ask.priority.or(hold.priority);
            if earlier(&hold.waiting_since, &ask.waiting_since) {
                ask.waiting_since = hold.waiting_since;
            }
            if asked {
                ask.ask = hold.ask;
            }
            ask.recommendation = hold.recommendation.or(ask.recommendation);
        }
        items.push(ask);
    }
    items.extend(approvals.into_values().map(|(ask, _)| ask));
    let ungrouped = items.clone();
    items = group_stranded(items);
    items.sort_by_key(rank);
    let evidence = items.clone();
    let mut first: BTreeMap<String, usize> = BTreeMap::new();
    let mut kept: Vec<Ask> = Vec::with_capacity(items.len());
    for ask in items {
        let identity = requests::identity(&ask, &reading.on.host);
        let decision_key = format!("{identity}@{}", ask.head.as_deref().unwrap_or_default());
        match first.get(&decision_key) {
            Some(&n) => {
                let since = if earlier(&ask.waiting_since, &kept[n].waiting_since) {
                    ask.waiting_since.clone()
                } else {
                    kept[n].waiting_since.clone()
                };
                if ask.source == "pass" {
                    kept[n].ask = format!("{}; {}", kept[n].ask, ask.ask);
                } else if ask.source == REVIEWS && kept[n].source == DIGEST {
                    let priority = kept[n].priority.clone();
                    let since = kept[n].waiting_since.clone();
                    kept[n] = ask;
                    kept[n].priority = kept[n].priority.clone().or(priority);
                    if earlier(&since, &kept[n].waiting_since) {
                        kept[n].waiting_since = since;
                    }
                }
                kept[n].waiting_since = since;
            }
            None => {
                first.insert(decision_key, kept.len());
                kept.push(ask);
            }
        }
    }
    // Preserve v2's unique per-card keys. Multiple semantic decisions can
    // describe one legacy card; new readers use identity + revision, not key.
    kept.sort_by_key(rank);
    let semantic = kept;
    let mut kept: Vec<Ask> = Vec::new();
    let mut cards: BTreeMap<String, usize> = BTreeMap::new();
    for ask in &semantic {
        if let Some(&n) = cards.get(&ask.key) {
            kept[n].ask = format!("{}; {}", kept[n].ask, ask.ask);
        } else {
            cards.insert(ask.key.clone(), kept.len());
            kept.push(ask.clone());
        }
    }
    for ask in &mut kept {
        ask.ask = one_line(&ask.ask, MAX_ASK).unwrap_or_default();
        ask.recommendation = (ask.recommendation.as_deref()).and_then(|r| one_line(r, MAX_ASK));
    }
    let omitted = 0;
    let mut counts = Counts {
        decisions: semantic.len(),
        groups: semantic.iter().filter(|a| a.group.is_some()).count(),
        members: semantic
            .iter()
            .map(|a| a.group.as_ref().map_or(1, |g| g.numbers.len()))
            .sum(),
    };
    let requests: Vec<_> = semantic
        .iter()
        .filter(|ask| kept.iter().any(|card| card.key == ask.key))
        .map(|ask| requests::describe(ask, &evidence, snapshot, reading, previous))
        .collect();
    let mut request_sources = Vec::new();
    for request in &requests {
        for ask in evidence.iter().filter(|ask| {
            matches!(
                ask.source.as_str(),
                REVIEWS | PROPOSALS | DIGEST | DECISIONS | DRAFTS
            ) && requests::identity(ask, &reading.on.host) == request.identity
                && ask.head == request.target.head
        }) {
            let raw = [&reading.reviews, &reading.proposals, &reading.digest]
                .into_iter()
                .chain(reading.inbox.values())
                .flat_map(|c| &c.asks)
                .find(|raw| raw.key == ask.key && raw.source_ref == ask.source_ref);
            let original = raw.unwrap_or(ask);
            let previous_source = requests::previous_source(ask, previous);
            let mut source = requests::SourceAsk {
                key: request.key.clone(),
                // Keep grouped membership and this source's question, not
                // the raw representative or the combined legacy card text.
                ask: ask.clone(),
                observed_at: if raw.is_some() && !original.carried {
                    Some(snapshot.at.clone())
                } else {
                    previous_source.as_ref().and_then(|s| s.observed_at.clone())
                },
                source_time: if matches!(
                    original.source.as_str(),
                    PROPOSALS | DIGEST | DECISIONS | DRAFTS
                ) {
                    raw.map(|a| a.waiting_since.clone())
                        .or_else(|| previous_source.and_then(|s| s.source_time))
                } else {
                    None
                },
            };
            if source.ask.source == REVIEWS {
                // Review waits have no source edit clock; preserve wait age,
                // rather than changing the publication on every observation.
                source.ask.waiting_since = ask.waiting_since.clone();
            }
            if !request_sources.contains(&source) {
                request_sources.push(source);
            }
        }
    }
    let mut identities: Vec<_> = kept
        .iter()
        .flat_map(|card| {
            let members = card
                .group
                .as_ref()
                .map(|g| g.numbers.clone())
                .unwrap_or_else(|| vec![card.action.number]);
            members
                .into_iter()
                .map(|number| {
                    let ask = if card.group.is_some() {
                        ungrouped
                            .iter()
                            .find(|a| {
                                a.source == card.source
                                    && a.action.number == number
                                    && a.action
                                        .repository
                                        .eq_ignore_ascii_case(&card.action.repository)
                            })
                            .unwrap_or(card)
                    } else {
                        card
                    };
                    let item = if card.group.is_some() {
                        format!("{}#{number}", card.action.repository)
                    } else {
                        ask.item.clone()
                    };
                    let key = if card.group.is_some() {
                        item.to_ascii_lowercase()
                    } else {
                        ask.key.clone()
                    };
                    Identity {
                        key,
                        detail_key: card.key.clone(),
                        item,
                        kind: ask.kind.clone(),
                        source: ask.source.clone(),
                        waiting_since: ask.waiting_since.clone(),
                        carried: ask.carried,
                        label_backed: !ask.carried
                            && ask.kind == "review-needs-human"
                            && !ask.ask.starts_with(crate::poll::LEFT_TO_OPERATOR),
                        priority: ask.priority.clone(),
                        epic: "unknown".into(),
                        top_epic: "unknown".into(),
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect();
    // Detail eviction must not erase a decision when its source cannot be
    // refreshed next pass. Only a complete read can retire that identity.
    if let Some(old) = published_list(previous).and_then(|n| n["identities"].as_array()) {
        for value in old {
            let Ok(mut identity) = serde_json::from_value::<Identity>(value.clone()) else {
                continue;
            };
            if let Some(carried) = identities
                .iter_mut()
                .find(|i| i.key == identity.key && i.carried)
            {
                carried.waiting_since = identity.waiting_since.clone();
                carried.priority = identity.priority.clone();
                carried.label_backed = identity.label_backed;
            }
            let repository = identity
                .item
                .rsplit_once('#')
                .map(|(r, _)| r.to_ascii_lowercase())
                .unwrap_or_default();
            let item = identity.item.to_ascii_lowercase();
            let unread = match identity.source.as_str() {
                "epics" | "items" => {
                    let read = if identity.source == "epics" {
                        sources.epics
                    } else {
                        sources.items
                    };
                    matches!(read, Read::Partial | Read::Unread)
                        && !snapshot.holds.iter().any(|h| {
                            h.kind != HoldKind::Unreadable
                                && h.item
                                    .as_ref()
                                    .is_some_and(|i| i.eq_ignore_ascii_case(&identity.item))
                        })
                }
                REVIEWS | PROPOSALS | DIGEST | DECISIONS | DRAFTS => {
                    let collected = match identity.source.as_str() {
                        REVIEWS => &reading.reviews,
                        PROPOSALS => &reading.proposals,
                        DIGEST => &reading.digest,
                        // An inbox source no longer configured lists nothing.
                        inbox => match reading.inbox.get(inbox) {
                            Some(collected) => collected,
                            None => continue,
                        },
                    };
                    collected.read == Read::Unread
                        || collected.read == Read::Partial
                            && (collected.unread.contains(&repository)
                                || collected.unread.contains(&item))
                }
                _ => continue,
            };
            if unread
                && !identities.iter().any(|i| i.key == identity.key)
                && !published_list(previous)
                    .and_then(|p| p["items"].as_array())
                    .is_some_and(|items| items.iter().any(|i| i["key"] == identity.key))
                && identity
                    .key
                    .starts_with(&identity.item.to_ascii_lowercase())
                && identity.item.rsplit_once('#').is_some_and(|(repo, n)| {
                    validate_repository(repo).is_ok() && n.parse::<u64>().is_ok_and(|n| n > 0)
                })
            {
                identity.carried = true;
                identities.push(identity);
                // A decision carried without its card is still a known one.
                counts.decisions += 1;
                counts.members += 1;
            }
        }
    }
    identities.sort_by_key(|i| {
        let known = i
            .priority
            .as_ref()
            .and_then(|p| options.iter().position(|o| o == p));
        let since = parse_time(&i.waiting_since);
        (
            (
                i.priority.is_none(),
                known.is_none(),
                known,
                i.priority.clone(),
            ),
            since.is_none(),
            since,
            i.key.clone(),
        )
    });
    let items = kept;
    let cached = published_list(previous).map(|p| &p["summary_cache"]);
    let summary_cache = items
        .iter()
        .filter_map(|ask| {
            if ask.source_ref.kind != RefKind::Hold || ask.source == "pass" {
                return None;
            }
            let said = reading
                .summaries
                .1
                .get(&ask.key)
                .cloned()
                .flatten()
                .or_else(|| {
                    cached.and_then(|c| serde_json::from_value::<Summary>(c[&ask.key].clone()).ok())
                })?;
            Some((ask.key.clone(), said))
        })
        .collect();
    let read = [
        sources.epics,
        sources.items,
        sources.reviews,
        sources.proposals,
        sources.digest,
    ];
    let details_omitted = identities
        .iter()
        .filter(|i| !items.iter().any(|a| a.key == i.detail_key))
        .count();
    let approval_evidence = (reading.reviews.approval_evidence.as_ref())
        .map(|fresh| ApprovalEvidence::of(&items, fresh, at));
    let mut list = NeedsYou {
        review_buckets: reading.reviews.review_buckets.clone(),
        schema: NEEDS_YOU_SCHEMA,
        complete: omitted == 0
            && read
                .iter()
                .chain(sources.inbox.values())
                .all(|r| matches!(r, Read::Complete | Read::Off)),
        validated_at: at.clone(),
        changed_at: at.clone(),
        sources,
        summaries: reading.summaries.0,
        omitted,
        omitted_sources: BTreeMap::new(),
        count: identities.len(),
        counts,
        context: context(snapshot, previous, &reading.observed),
        items,
        identities,
        epics: BTreeMap::new(),
        details_omitted,
        requests,
        request_sources,
        summary_cache,
        carry_validation: BTreeMap::new(),
        approval_evidence,
    };
    preserve_changed_at(&mut list, previous);
    list
}

pub(super) fn preserve_changed_at(list: &mut NeedsYou, previous: Option<&Value>) {
    let previous = published_list(previous);
    // How much of the summaries was read is no change: what they said is
    // kept where they were not.
    let content = |list: &Value| {
        let mut list = list.clone();
        if let Some(fields) = list.as_object_mut() {
            for field in ["validated_at", "changed_at", "summaries", "summary_cache"] {
                fields.remove(field);
            }
            requests::strip_observation_times(fields);
        }
        list
    };
    // Where [`fit`] cut the published list's tail, the same list is this
    // one cut as far: its approval evidence first, then its cards.
    let same = previous.filter(|p| {
        let mut cut = list.clone();
        let published = p["items"].as_array().map_or(0, Vec::len);
        let evidence = (p["approval_evidence"]["items"].as_object()).map_or(0, |e| e.len());
        while cut.evidence_len() > evidence && cut.evict_evidence() {}
        while cut.items.len() > published && cut.cut().is_some() {}
        serde_json::to_value(&cut).is_ok_and(|now| content(p) == content(&now))
    });
    if let Some(since) = same.and_then(|p| not_after(&p["changed_at"], &list.validated_at)) {
        list.changed_at = since;
    }
}

/// Whether `snapshot` says something other than the published comment
/// `body`, if any: its time alone is no change, nor the needs-you list's
/// `validated_at` until [`NEEDS_YOU_REFRESH_HOURS`] after the published one,
/// so that a list nothing changes is written again that often; the pass
/// view's likewise ([`super::view::comparable`], [`super::view::due`]).
pub fn changed(body: Option<&str>, snapshot: &Snapshot) -> Result<bool> {
    let mut now = serde_json::to_value(snapshot)?;
    let validated = |s: &mut Value| {
        let list = s.get_mut("needs_you").and_then(Value::as_object_mut)?;
        requests::strip_observation_times(list);
        list.remove("validated_at")?.as_str().map(str::to_owned)
    };
    let refresh = validated(&mut now);
    Ok(body.and_then(published).is_none_or(|mut s| {
        let last = validated(&mut s);
        let due = refresh.as_ref().is_some_and(|at| {
            let last = last.unwrap_or_default();
            parse_time(&last).is_none() || over(&last, at, NEEDS_YOU_REFRESH_HOURS)
        });
        let due = due || super::view::due(s.get("pass_view"), now.get("pass_view"));
        s["at"] = Value::Null;
        now["at"] = Value::Null;
        for view in [&mut s, &mut now]
            .into_iter()
            .filter_map(|s| s.get_mut("pass_view"))
        {
            super::view::comparable(view);
        }
        // A pass count or UTC rollover alone must not cause a comment write.
        if let Some(pass) = s["pass"].as_object_mut() {
            pass.remove("overhead");
        }
        if let Some(pass) = now["pass"].as_object_mut() {
            pass.remove("overhead");
        }
        due || s != now
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
    let needs_you = match &snapshot.needs_you {
        Some(list) => format!(",\"needs_you\":{}", serde_json::to_string(list)?),
        None => String::new(),
    };
    let work = match &snapshot.dispatchable_work {
        Some(work) => format!(",\"dispatchable_work\":{}", serde_json::to_string(work)?),
        None => String::new(),
    };
    let view = match &snapshot.pass_view {
        Some(view) => format!(",\"pass_view\":{}", serde_json::to_string(view)?),
        None => String::new(),
    };
    Ok(format!(
        "{HOLDS_MARKER}\nHolds as of {}. Written by `agent-board pass`; the review app reads the JSON below.\n\n```json\n{{\"schema\":{},\"at\":{},\"truncated\":{},\"wip\":{},\"pass\":{}{leases}{needs_you}{work}{view},\"holds\":[{lines}\n]}}\n```\n",
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
/// is longer, the work inventory and then the pass view go whole, then the
/// needs-you list's summary cache, then its
/// context and approval evidence (lowest ranked first, counted), then
/// the item holds that do not wait on the operator are left out,
/// in [`left_out_first`] order, then the holds a stall alarm watches
/// ([`Published::watched`]), then the stall alarms, each with the hold it
/// is on, then those on the holds issue, likewise, until it fits, and
/// counted in `truncated`. So alarms
/// never keep the holds from being published, and a hold's stall clock,
/// the `since` the next pass carries over ([`carry`]), outlasts the other
/// holds: no alarm is kept without its hold. The operator's own holds and
/// the holds of every new run are kept ahead of needs-you detail cards,
/// which go lowest ranked first, actionable approvals last
/// ([`NeedsYou::cut`]). Should the inventory and protected holds still not
/// fit, [`cap`] leaves out more, honestly counted, so that size
/// never keeps the inbox from being published: the body is always at most
/// [`MAX_COMMENT_CHARS`].
pub fn fit(mut snapshot: Snapshot) -> Result<(Snapshot, String)> {
    // Optional work detail never evicts the operator inbox or stall clocks.
    if snapshot.dispatchable_work.is_some()
        && render(&snapshot)?.chars().count() > MAX_COMMENT_CHARS
    {
        snapshot.dispatchable_work = None;
    }
    if snapshot.pass_view.is_some() && render(&snapshot)?.chars().count() > MAX_COMMENT_CHARS {
        snapshot.pass_view = None;
    }
    let holds = &snapshot.holds;
    let on = |h: &Published| (h.item.clone(), h.epic);
    let stalled = |h: &Published| h.kind == HoldKind::Stalled;
    let alarms: BTreeSet<_> = holds.iter().filter(|h| stalled(h)).map(on).collect();
    let bases: BTreeSet<_> = holds.iter().filter(|h| !stalled(h)).map(on).collect();
    // An alarm and the hold it is on go together, as one group, by the
    // first of them; any other entry is a group of its own.
    let tied = |h: &Published| alarms.contains(&on(h)) && bases.contains(&on(h));
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (n, hold) in holds.iter().enumerate() {
        if hold.item.is_none() || (hold.operator && !stalled(hold)) {
            continue;
        }
        let first = (holds.iter()).position(|h| on(h) == on(hold));
        let group = first.filter(|_| tied(hold)).unwrap_or(n);
        groups.entry(group).or_default().push(n);
    }
    // The alarms on the holds issue (idle, and the one that counts the
    // stalled holds past [`MAX_STALLS`]) last: each speaks for many.
    let rank = |n: usize| match &holds[n] {
        h if stalled(h) && !tied(h) => 3,
        h if tied(h) => 2,
        h if h.watched => 1,
        _ => 0,
    };
    let mut order: Vec<Vec<usize>> = groups.into_values().collect();
    order.sort_by_key(|group| {
        let n = group[0];
        (rank(n), left_out_first(&holds[n]))
    });
    let mut order = order.into_iter();
    let mut dropped = BTreeSet::new();
    let mut needs_you = snapshot.needs_you.clone();
    let without = |dropped: &BTreeSet<usize>, needs_you: &Option<NeedsYou>| Snapshot {
        dispatchable_work: snapshot.dispatchable_work.clone(),
        pass_view: snapshot.pass_view.clone(),
        schema: snapshot.schema,
        at: snapshot.at.clone(),
        truncated: dropped.len(),
        wip: snapshot.wip.clone(),
        pass: snapshot.pass.clone(),
        leases: snapshot.leases.clone(),
        needs_you: needs_you.clone(),
        holds: (holds.iter().enumerate())
            .filter(|(n, _)| !dropped.contains(n))
            .map(|(_, h)| h.clone())
            .collect(),
    };
    loop {
        let fitted = without(&dropped, &needs_you);
        let body = render(&fitted)?;
        let mut over = body.chars().count().saturating_sub(MAX_COMMENT_CHARS);
        if over == 0 {
            return Ok((fitted, body));
        }
        // The summary cache only saves the next pass downloads: it goes
        // before any hold or ask does.
        if let Some(list) = needs_you.as_mut().filter(|l| !l.summary_cache.is_empty()) {
            list.summary_cache.clear();
            continue;
        }
        // Context is useful but must never evict an actionable decision.
        if let Some(list) = needs_you.as_mut().filter(|l| !l.context.items.is_empty()) {
            list.context.items.pop();
            list.context.omitted += 1;
            continue;
        }
        // So is approval evidence, which only describes a decision.
        if needs_you.as_mut().is_some_and(NeedsYou::evict_evidence) {
            continue;
        }
        // Each entry is its JSON and a ",\n"; the count may grow a digit,
        // which the next round sees.
        while over > 0 {
            let Some(group) = order.next() else {
                // Detail eviction preserves every identity; past the last
                // card, the cap leaves out what it must and says so. The
                // holds this round dropped are not in `fitted`.
                let Some(_) = needs_you.as_mut().and_then(NeedsYou::cut) else {
                    return cap(without(&dropped, &needs_you));
                };
                // A cut removes its descriptor too; re-render before cutting
                // again so that the additional savings do not overcut asks.
                break;
            };
            for n in group {
                let len = serde_json::to_string(&holds[n])?.chars().count() + 2;
                over = over.saturating_sub(len);
                dropped.insert(n);
            }
        }
    }
}

/// The last resort of [`fit`], once every evictable hold and detail card is
/// gone, in this order: the source carry records no card is left for, then
/// the lowest ranked identities, counted in `omitted` (`count` stays the
/// number of decisions), after each batch of them the epic activity no kept
/// identity names, then the protected holds in [`left_out_first`] order, counted in
/// `truncated`, until it fits.
fn cap(mut snapshot: Snapshot) -> Result<(Snapshot, String)> {
    loop {
        let body = render(&snapshot)?;
        let mut over = body.chars().count().saturating_sub(MAX_COMMENT_CHARS);
        if over == 0 {
            return Ok((snapshot, body));
        }
        if let Some(list) = snapshot.needs_you.as_mut() {
            // Aliases hidden by deduplication can outlive the last card.
            if !list.request_sources.is_empty() || !list.requests.is_empty() {
                list.request_sources.clear();
                list.requests.clear();
                list.complete = false;
                continue;
            }
            // Once identities have gone, the activity of epics no kept one
            // names goes before more of them do. Missing activity is
            // unknown, not proof of inactivity.
            if list.omitted > 0 || list.identities.is_empty() {
                let identities = &list.identities;
                let before = list.epics.len();
                list.epics.retain(|epic, _| {
                    identities
                        .iter()
                        .any(|i| &i.epic == epic || &i.top_epic == epic)
                });
                if list.epics.len() < before {
                    list.complete = false;
                    continue;
                }
            }
            if !list.identities.is_empty() {
                // Keep the highest ranked prefix. Batch the cut by encoded
                // character size, then re-render to account for count digits.
                while over > 0 {
                    let Some(identity) = list.identities.pop() else {
                        break;
                    };
                    if !list
                        .identities
                        .iter()
                        .any(|i| i.item.eq_ignore_ascii_case(&identity.item))
                    {
                        list.carry_validation
                            .remove(&identity.item.to_ascii_lowercase());
                    }
                    over =
                        over.saturating_sub(serde_json::to_string(&identity)?.chars().count() + 1);
                    list.omitted += 1;
                    *list.omitted_sources.entry(identity.source).or_default() += 1;
                }
                list.complete = false;
                continue;
            }
        }
        if let Some(first) = snapshot.holds.iter().min_by_key(|h| left_out_first(h)) {
            // Even protected holds may overflow on their own. Keep an alarm
            // and its base together, just as the normal fitting path does.
            let on = (first.item.clone(), first.epic);
            let before = snapshot.holds.len();
            snapshot.holds.retain(|h| (h.item.clone(), h.epic) != on);
            snapshot.truncated += before - snapshot.holds.len();
            if let Some(list) = snapshot.needs_you.as_mut() {
                list.complete = false;
            }
            continue;
        }
        // The remaining variable-size metadata is optional. Absence means
        // unknown, never an empty lease inventory or complete accounting.
        if snapshot.leases.is_some() || snapshot.pass.overhead.is_some() {
            snapshot.leases = None;
            snapshot.pass.overhead = None;
            continue;
        }
        // Only carried timestamps can still have unbounded text here. Preserve
        // their instants in canonical form rather than looping without progress.
        let canonical = |text: &str| {
            parse_time(text)
                .and_then(|t| t.format(&Rfc3339).ok())
                .unwrap_or_else(|| snapshot.at.clone())
        };
        snapshot.pass.progress_at = canonical(&snapshot.pass.progress_at);
        if let Some(list) = snapshot.needs_you.as_mut() {
            list.changed_at = canonical(&list.changed_at);
            list.complete = false;
        }
        let body = render(&snapshot)?;
        return Ok((snapshot, body));
    }
}

/// The JSON of a published hold comment, if it is one of [`HOLDS_SCHEMA`].
pub fn published(body: &str) -> Option<Value> {
    json_block(body).filter(|s| s["schema"] == HOLDS_SCHEMA)
}

#[cfg(test)]
pub use self::published as parse;

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

    #[test]
    fn urls_stay_on_the_items_forge() {
        let forgejo = "https://forge.example:8443/subpath";
        for (host, expected) in [
            (DEFAULT_HOST, "https://github.com/o/r/issues/7"),
            (forgejo, "https://forge.example:8443/subpath/o/r/issues/7"),
        ] {
            let mut on = item("o/r", 7);
            on.host = host.into();
            let held = entry(
                Scope::Item(on),
                HoldKind::NeedsHuman,
                "labelled needs-human",
            );
            assert_eq!(held.published().url.as_deref(), Some(expected));
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
            overhead: None,
            mode: "actions",
            candidates: Some(3),
            started: 1,
            live_epics: None,
            revisited: None,
            progress_at: String::new(),
        };
        (wip, pass)
    }

    #[test]
    fn overhead_carry_and_rollover() {
        let at = OffsetDateTime::parse("2026-10-23T23:59:00Z", &Rfc3339).unwrap();
        let effects = super::super::Effects {
            starts: BTreeMap::from([("triage".into(), 2)]),
            comments_created: 3,
            comments_edited: 1,
        };
        let wrap = |at: OffsetDateTime, overhead: &Overhead| {
            json!({
                "schema": HOLDS_SCHEMA, "at": at.format(&Rfc3339).unwrap(),
                "pass": {"overhead": overhead},
            })
        };
        let first = overhead(None, at, &effects);
        assert!(!first.today.complete);
        let previous = wrap(at, &first);
        let again = overhead(Some(&previous), at, &effects);
        assert_eq!(again.today.passes, 2);
        assert_eq!(again.today.effects.starts["triage"], 4);
        assert_eq!(again.today.effects.comments_created, 6);
        assert_eq!(again.today.effects.comments_edited, 2);
        assert!(!again.today.complete);

        for previous in [
            None,
            Some(json!({})),
            Some(json!({"schema": "older"})),
            Some(json!({"schema": HOLDS_SCHEMA, "at": "unreadable", "pass": {"overhead": first}})),
            Some(wrap(at + time::Duration::days(1), &first)),
        ] {
            let reset = overhead(previous.as_ref(), at, &effects);
            assert_eq!(reset, first);
        }
        let next = at + time::Duration::minutes(2);
        let rolled = overhead(Some(&previous), next, &effects);
        assert!(rolled.today.complete);
        assert_eq!(rolled.today.day, "2026-10-24");
        assert_eq!(rolled.today.passes, 1);
        assert_eq!(rolled.history, vec![first.today]);
        // UTC, not the caller's local date.
        assert_eq!(
            overhead(
                Some(&previous),
                next.to_offset(time::UtcOffset::from_hms(-5, 0, 0).unwrap()),
                &effects
            ),
            rolled
        );
        let gap = overhead(Some(&previous), at + time::Duration::days(3), &effects);
        assert!(!gap.today.complete);

        let mut carried = previous;
        for n in 1..=12 {
            let now = at + time::Duration::days(n);
            let daily = overhead(Some(&carried), now, &effects);
            assert_eq!(daily.history.len(), (n as usize).min(7));
            assert!(daily.today.complete);
            assert!(daily.history.iter().all(|d| d.day < daily.today.day));
            carried = wrap(now, &daily);
        }
    }

    #[test]
    fn overhead_alone_does_not_publish() {
        let at = "2026-10-23T23:59:00Z";
        let now = parse_time(at).unwrap();
        let mut first = snapshot(&[], at.into(), tally());
        first.pass.overhead = Some(overhead(None, now, &super::super::Effects::default()));
        let body = render(&first).unwrap();
        let previous = published(&body).unwrap();
        for days in [0, 1, 10] {
            let mut later = snapshot(&[], at.into(), tally());
            later.pass.overhead = Some(overhead(
                Some(&previous),
                now + time::Duration::days(days),
                &super::super::Effects::default(),
            ));
            assert!(!changed(Some(&body), &later).unwrap());
        }
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
            "pass": {"mode": "actions", "candidates": 3, "started": 1, "live_epics": null,
                     "progress_at": "2026-10-26T00:00:00Z"},
            "holds": [
                {"item": null, "url": null, "epic": false, "kind": "wip",
                 "reason": "2 of 2 runs active", "operator": false, "priority": null, "question": null,
                 "since": "2026-10-26T00:00:00Z"},
                {"item": "o/agent-board#7", "url": "https://github.com/o/agent-board/issues/7",
                 "epic": false, "kind": "dispatched", "reason": "plan P was dispatched already, U",
                 "operator": false, "priority": null, "question": null,
                 "since": "2026-10-26T00:00:00Z"},
                {"item": "o/tracker#596", "url": "https://github.com/o/tracker/issues/596",
                 "epic": true, "kind": "epic-unplanned",
                 "reason": "its planning request R got no plan, nor did R0 before it; a person comments on the epic to plan it again",
                 "operator": true, "priority": null, "question": null,
                 "since": "2026-10-26T00:00:00Z"},
                {"item": "o/tracker#1000", "url": "https://github.com/o/tracker/issues/1000",
                 "epic": false, "kind": "needs-human", "reason": "labelled needs-human",
                 "operator": true, "priority": "P1", "question": "Which target?",
                 "since": "2026-10-26T00:00:00Z"},
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
        // A summary cache that takes a list at the limit past it goes
        // before any hold or ask does.
        let with_list = |pad| {
            let mut cached = bare(&board(40, pad));
            cached.needs_you = Some(listed(&waiting(), None, (true, true)));
            cached
        };
        let pad = MAX_COMMENT_CHARS - chars(&render(&with_list(0)).unwrap());
        let mut cached = with_list(pad);
        let list = cached.needs_you.as_mut().unwrap();
        let asks = list.items.len();
        list.summary_cache.insert(
            "o/tracker#5001".into(),
            Summary {
                question: Some("Ship?".into()),
                ..Summary::default()
            },
        );
        let (uncached, uncached_body) = fit(cached).unwrap();
        assert_eq!(
            (uncached.truncated, chars(&uncached_body)),
            (0, MAX_COMMENT_CHARS)
        );
        let list = uncached.needs_you.unwrap();
        assert_eq!((list.items.len(), list.summary_cache.len()), (asks, 0));
        // Protected holds that leave room for the list's own fields but
        // none of its inventory still publish, with every decision counted
        // in `omitted` and no hold left out.
        let full = listed(&waiting(), None, (true, true));
        let count = full.count;
        let mut omitted_sources = BTreeMap::<String, usize>::new();
        for identity in &full.identities {
            *omitted_sources.entry(identity.source.clone()).or_default() += 1;
        }
        let mut context = full.context.clone();
        context.omitted += std::mem::take(&mut context.items).len();
        let mut asked = bare(&board(0, 0));
        asked.needs_you = Some(NeedsYou {
            complete: false,
            omitted: count,
            omitted_sources,
            context,
            details_omitted: count,
            items: Vec::new(),
            identities: Vec::new(),
            epics: BTreeMap::new(),
            requests: Vec::new(),
            request_sources: Vec::new(),
            summary_cache: BTreeMap::new(),
            ..full.clone()
        });
        let pad = MAX_COMMENT_CHARS - chars(&render(&asked).unwrap());
        asked
            .holds
            .last_mut()
            .unwrap()
            .reason
            .push_str(&"p".repeat(pad));
        asked.needs_you = Some(full);
        let (asked, body) = fit(asked).unwrap();
        assert_eq!((asked.truncated, chars(&body)), (0, MAX_COMMENT_CHARS));
        let list = asked.needs_you.as_ref().unwrap();
        assert_eq!(
            (
                list.count,
                list.omitted,
                list.identities.len(),
                list.complete
            ),
            (count, count, 0, false)
        );
        assert_eq!(parse(&body).unwrap()["needs_you"]["omitted"], list.omitted);
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
        // The operator's questions alone past the limit still publish: the
        // whole inventory and epic activity go, then the holds in
        // [`left_out_first`] order, each counted.
        let mut asked = board(0, 0);
        for hold in &mut asked[1..] {
            hold.why = Some(format!("Q: {}", "q".repeat(4000)));
        }
        let full = bare(&asked);
        let mut crowded = bare(&asked);
        let list = listed(&waiting(), None, (true, true));
        let count = list.count;
        crowded.needs_you = Some(list);
        let (fitted, body) = fit(crowded).unwrap();
        assert!(chars(&body) <= MAX_COMMENT_CHARS);
        assert!(fitted.truncated > 0);
        assert_eq!(fitted.holds.len() + fitted.truncated, full.holds.len());
        let mut order: Vec<&Published> = full.holds.iter().collect();
        order.sort_by_key(|h| left_out_first(h));
        let (out, kept) = order.split_at(fitted.truncated);
        assert!(out.iter().all(|h| !fitted.holds.contains(h)));
        assert!(kept.iter().all(|h| fitted.holds.contains(h)));
        let list = fitted.needs_you.as_ref().unwrap();
        assert_eq!(
            (list.count, list.omitted, list.complete),
            (count, count, false)
        );
        assert!(list.identities.is_empty() && list.epics.is_empty());
        let parsed = parse(&body).unwrap();
        assert_eq!(parsed["truncated"], fitted.truncated);
        assert_eq!(parsed["needs_you"]["omitted"], count);
    }

    /// The full form of `identity`, as published before the compact one.
    fn full_form(identity: &Identity) -> Value {
        json!({
            "key": identity.key, "detail_key": identity.detail_key,
            "item": identity.item, "kind": identity.kind, "source": identity.source,
            "waiting_since": identity.waiting_since, "priority": identity.priority,
            "epic": identity.epic, "topEpic": identity.top_epic,
        })
    }

    /// An inventory of `n` identity-only decisions whose full form is past
    /// the limit: compact, `whole` fits with none left out; past that, the
    /// lowest ranked identities are left out, counted in `omitted`, with
    /// `count` still every decision and `complete` false.
    #[test]
    fn inventory_over_sixty_thousand_characters_still_publishes() {
        for (n, whole) in [(250, true), (800, false)] {
            let mut snapshot = waiting();
            let mut list = listed(&snapshot, None, (true, true));
            let template = Identity {
                priority: Some("P2".into()),
                epic: "cgwalters-forge/tracker#596".into(),
                top_epic: "cgwalters-forge/tracker#766".into(),
                ..list.identities[0].clone()
            };
            list.identities = (10000..10000 + n)
                .map(|n| Identity {
                    key: format!("cgwalters-forge/tracker#{n}"),
                    detail_key: format!("cgwalters-forge/tracker#{n}"),
                    item: format!("cgwalters-forge/tracker#{n}"),
                    epic: if n % 2 == 0 {
                        template.top_epic.clone()
                    } else {
                        template.epic.clone()
                    },
                    ..template.clone()
                })
                .collect();
            list.count = list.identities.len();
            list.items.clear();
            list.requests.clear();
            list.request_sources.clear();
            list.details_omitted = list.count;
            let full: Vec<_> = list.identities.iter().map(full_form).collect();
            assert!(chars(&serde_json::to_string(&full).unwrap()) > MAX_COMMENT_CHARS);
            // The pass reads back either form as the same identity.
            for (identity, full) in list.identities.iter().zip(&full) {
                let compact = serde_json::to_value(identity).unwrap();
                assert!(compact.get("detail_key").is_none() && compact.get("key").is_none());
                for form in [compact, full.clone()] {
                    assert_eq!(&serde_json::from_value::<Identity>(form).unwrap(), identity);
                }
            }
            let identities = list.identities.clone();
            let complete = list.complete;
            snapshot.needs_you = Some(list);
            let (fitted, body) = fit(snapshot).unwrap();
            assert!(chars(&body) <= MAX_COMMENT_CHARS);
            let list = fitted.needs_you.as_ref().unwrap();
            assert_eq!(list.count, n);
            assert_eq!(list.count, list.identities.len() + list.omitted);
            assert_eq!(list.identities, identities[..list.identities.len()]);
            assert_eq!(list.details_omitted, list.count);
            match whole {
                true => assert_eq!((list.omitted, list.complete), (0, complete), "{n}"),
                false => {
                    assert!(list.omitted > 0 && !list.identities.is_empty(), "{n}");
                    assert!(!list.complete);
                    // One fewer left out would not fit.
                    let back = serde_json::to_string(&identities[list.identities.len()]);
                    assert!(chars(&body) + chars(&back.unwrap()) + 1 > MAX_COMMENT_CHARS);
                }
            }
            let parsed = parse(&body).unwrap();
            assert_eq!(parsed["needs_you"]["omitted"], list.omitted);
            assert_eq!(parsed["needs_you"]["complete"], list.complete);
            assert_eq!(parsed["truncated"], fitted.truncated);
            assert!(!changed(Some(&body), &fitted).unwrap());
        }
    }

    #[test]
    fn oversized_optional_metadata_still_publishes() {
        let mut snapshot = waiting();
        snapshot.holds.clear();
        snapshot.leases = Some(vec![Held {
            slot: 1,
            lease: super::super::lease::Lease {
                run: 1,
                item: "o/r#1".into(),
                pool: "p".repeat(MAX_COMMENT_CHARS),
                taken_at: snapshot.at.clone(),
                expires_at: snapshot.at.clone(),
                overridden: false,
            },
        }]);
        let (fitted, body) = fit(snapshot).unwrap();
        assert!(chars(&body) <= MAX_COMMENT_CHARS);
        assert!(
            fitted.leases.is_none(),
            "missing leases are unknown, not empty"
        );
    }

    /// Only a change of the holds, the WIP count or the pass's summary
    /// publishes: not a later clock, nor the order the pass decided them in,
    /// once their times carry over. A comment from before the WIP count and
    /// summary is republished.
    #[test]
    fn unchanged_is_not_republished() {
        let at = "2026-10-26T00:00:00Z";
        let published = render(&snapshot(&entries(), at.into(), tally())).unwrap();
        let mut reordered = entries();
        reordered.reverse();
        let mut later = snapshot(&reordered, "2026-10-26T00:10:00Z".into(), tally());
        assert!(changed(Some(&published), &later).unwrap());
        carry(&mut later, parse(&published).as_ref(), (false, true));
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
            HoldKind::EpicNotLive,
            HoldKind::OutsideEpics,
            HoldKind::Overlap,
            HoldKind::Unreadable,
            HoldKind::Wip,
            HoldKind::Pools,
            HoldKind::Budget,
            HoldKind::Other,
            HoldKind::Stalled,
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
                "epic-unplanned",
                "needs-operator",
                "epic-not-live",
                "stalled"
            ]
        );
    }

    /// Children under a parent that is not a live epic: a line each, one
    /// entry for the parent, which waits on the operator only through a
    /// child that does, named highest Priority first; their order decided
    /// does not change it.
    #[test]
    fn not_live_epics_are_one_entry() {
        let under = |n, epic, priority: Option<&str>| Entry {
            scope: Scope::Under(item("o/r", n), item("o/t", epic)),
            hold: Hold::new(HoldKind::EpicNotLive, "its parent is not a live epic")
                .operator(priority.is_some_and(|p| p < "P2")),
            priority: priority.map(Into::into),
            why: None,
        };
        let mut entries = vec![
            under(1, 9, Some("P3")),
            under(2, 9, Some("P1")),
            under(3, 9, Some("P0")),
            under(4, 5, Some("P2")),
            under(5, 5, None),
        ];
        assert_eq!(
            entries[0].line(),
            "hold o/r#1: its parent is not a live epic"
        );
        let grouped = |entries: &[Entry]| {
            (bare(entries).holds.iter())
                .map(|h| json!([h.item, h.epic, h.operator, h.priority, h.reason]))
                .collect::<Vec<_>>()
        };
        let expected = vec![
            json!([
                "o/t#5",
                true,
                false,
                "P2",
                "not a live epic, so its 2 open child issue(s) wait until the operator or the bot labels it epic"
            ]),
            json!([
                "o/t#9",
                true,
                true,
                "P0",
                "not a live epic, so its 3 open child issue(s) wait until the operator or the bot labels it epic, among them o/r#3 (P0), o/r#2 (P1)"
            ]),
        ];
        assert_eq!(grouped(&entries), expected);
        entries.reverse();
        assert_eq!(grouped(&entries), expected);
        for n in 6..=9 {
            entries.push(under(n, 9, Some("P1")));
        }
        assert_eq!(
            grouped(&entries)[1][4],
            "not a live epic, so its 7 open child issue(s) wait until the operator or the bot labels it epic, among them o/r#3 (P0), o/r#2 (P1), o/r#6 (P1) and 3 more"
        );
    }
    #[test]
    fn times_are_normalised() {
        for (reason, expected) in [
            (
                "plan 6477159835@2026-10-28T18:24:18Z waits; one epic plan is applied a pass",
                "plan 6477159835@<time> waits; one epic plan is applied a pass",
            ),
            ("at 2026-10-28T18:24:18.123Z.", "at <time>."),
            (
                "at 2026-10-28T18:24:18+02:00 and 2026-10-28T18:24:19-05:30",
                "at <time> and <time>",
            ),
            ("at 2026-10-28T18:24:18", "at <time>"),
            ("at 2026-10-28T18:24:18.Z", "at <time>.Z"),
            ("on 2026-10-28 only", "on 2026-10-28 only"),
            ("ünïcode 2026-10-28T18:24:18Z ✓", "ünïcode <time> ✓"),
            ("", ""),
        ] {
            assert_eq!(normalised(reason), expected, "{reason}");
        }
    }

    /// A stall setting of the defaults, on the holds issue o/ops#1, with a
    /// pass log and `free` candidates free to start.
    fn watch(stall: &Stall, free: Option<usize>) -> Watch<'_> {
        static ON: std::sync::LazyLock<ItemRef> = std::sync::LazyLock::new(|| item("o/ops", 1));
        static TOP: std::sync::LazyLock<Vec<String>> =
            std::sync::LazyLock::new(|| vec!["P0".into(), "P1".into()]);
        static NONE: BTreeSet<String> = BTreeSet::new();
        Watch {
            stall,
            top: &TOP,
            on: &ON,
            log: Some("https://github.com/o/caller/actions/runs/9".into()),
            free,
            blocked: &NONE,
        }
    }

    /// The snapshot of `entries` at `at` with `candidates`, carried over from
    /// the published `previous` and alarmed, and its `stall` lines.
    fn alarmed(
        entries: &[Entry],
        at: &str,
        (candidates, progressed): (usize, bool),
        previous: Option<&Value>,
    ) -> (Snapshot, Vec<String>) {
        let (wip, mut pass) = tally();
        pass.candidates = Some(candidates);
        let mut snapshot = snapshot(entries, at.into(), (wip, pass));
        carry(&mut snapshot, previous, (progressed, true));
        let stall = Stall::default();
        let free = free(Some(&ranked(entries, candidates)), entries);
        let lines = alarm(&mut snapshot, previous, &watch(&stall, free));
        (snapshot, lines)
    }

    /// `n` ranked candidates: the items `entries` hold one by one, then
    /// others held by none.
    fn ranked(entries: &[Entry], n: usize) -> Vec<ItemRef> {
        let mut ranked: Vec<ItemRef> = Vec::new();
        let held = (entries.iter()).filter_map(|e| match &e.scope {
            Scope::Item(item) => Some(item.clone()),
            _ => None,
        });
        for item in held.chain((1..).map(|n| item("o/free", n))) {
            if ranked.len() == n {
                break;
            }
            if !ranked.contains(&item) {
                ranked.push(item);
            }
        }
        ranked
    }

    /// [`alarmed`], as the next pass reads it back.
    fn pass_at(
        entries: &[Entry],
        at: &str,
        progress: (usize, bool),
        previous: Option<&Value>,
    ) -> (Value, Vec<String>) {
        let (snapshot, lines) = alarmed(entries, at, progress, previous);
        (parse(&render(&snapshot).unwrap()).unwrap(), lines)
    }

    fn kinds(snapshot: &Value) -> Vec<(String, String)> {
        (snapshot["holds"].as_array().unwrap().iter())
            .map(|h| (text(h, "item"), text(h, "kind")))
            .collect()
    }

    fn text(hold: &Value, key: &str) -> String {
        hold[key].as_str().unwrap_or("-").to_owned()
    }

    /// A hold on an epic, or on a P0 or P1 item, that the pass is stuck on
    /// raises an alarm once the same hold, its times aside, has lasted
    /// `holdHours`; one that waits on a person, or on a lower Priority, or
    /// of a kind that waits by design, does not; and the alarm clears once
    /// the hold changes. Pass after pass, nothing else changes.
    #[test]
    fn a_hold_that_repeats_stalls() {
        let waits = |t: &str| format!("plan 6477159835@{t} waits; one epic plan is applied a pass");
        let at = |hours: u32| format!("2026-10-28T{hours:02}:00:00Z");
        let held = |reason: &str| {
            let mut epic = entry(Scope::Epic(item("o/t", 9)), HoldKind::Other, reason);
            epic.priority = Some("P2".into());
            let mut asked = entry(
                Scope::Item(item("o/t", 10)),
                HoldKind::NeedsHuman,
                "labelled needs-human",
            );
            asked.priority = Some("P0".into());
            let mut low = entry(
                Scope::Item(item("o/t", 11)),
                HoldKind::Unreadable,
                "labels not read whole",
            );
            low.priority = Some("P2".into());
            let mut top = entry(
                Scope::Item(item("o/t", 12)),
                HoldKind::Unreadable,
                "labels not read whole",
            );
            top.priority = Some("P1".into());
            let mut waiting = entry(
                Scope::Item(item("o/t", 13)),
                HoldKind::WaitingOn,
                "o/t#1 is open",
            );
            waiting.priority = Some("P0".into());
            let mut refused = entry(
                Scope::Epic(item("o/t", 14)),
                HoldKind::NeedsOperator,
                "plan refused",
            );
            refused.hold = refused.hold.operator(true);
            vec![epic, asked, low, top, waiting, refused]
        };
        // The first pass: every hold since now.
        let (first, lines) = pass_at(&held(&waits(&at(0))), &at(0), (0, false), None);
        assert!(lines.is_empty(), "{lines:?}");
        assert!(
            first["holds"]
                .as_array()
                .unwrap()
                .iter()
                .all(|h| h["since"] == at(0).as_str())
        );
        // Five hours on, the epic's plan time changed: still the same hold.
        let (fifth, lines) = pass_at(&held(&waits(&at(5))), &at(5), (0, false), Some(&first));
        assert!(lines.is_empty(), "{lines:?}");
        assert_eq!(fifth["holds"][0]["since"], at(0));
        assert_eq!(fifth["holds"][0]["reason"], waits(&at(5)));
        // Six hours on: the epic and the P1 item stall, nothing else.
        let (sixth, lines) = pass_at(&held(&waits(&at(6))), &at(6), (0, false), Some(&fifth));
        let stalled = format!(
            "the same hold for 6 hours or more, since {}: {}",
            at(0),
            waits("<time>")
        );
        assert_eq!(
            lines,
            [
                format!("stall hold epic o/t#9: {stalled}"),
                format!(
                    "stall hold o/t#12: the same hold for 6 hours or more, since {}: labels not read whole",
                    at(0)
                ),
            ]
        );
        let expected: Vec<(String, String)> = [
            ("o/t#9", "other"),
            ("o/t#9", "stalled"),
            ("o/t#10", "needs-human"),
            ("o/t#11", "unreadable"),
            ("o/t#12", "unreadable"),
            ("o/t#12", "stalled"),
            ("o/t#13", "waiting-on"),
            ("o/t#14", "needs-operator"),
        ]
        .map(|(i, k)| (i.to_owned(), k.to_owned()))
        .to_vec();
        assert_eq!(kinds(&sixth), expected);
        assert_eq!(
            sixth["holds"][1],
            json!({"item": "o/t#9", "url": "https://github.com/o/t/issues/9", "epic": true,
                "kind": "stalled", "reason": stalled, "operator": true, "priority": "P2",
                "question": null, "since": at(0)})
        );
        // A pass later that says the same is not published again.
        let (published, _) = alarmed(&held(&waits(&at(6))), &at(6), (0, false), Some(&fifth));
        let (later, _) = alarmed(&held(&waits(&at(6))), &at(7), (0, false), Some(&sixth));
        assert!(!changed(Some(&render(&published).unwrap()), &later).unwrap());
        // A new plan is a new hold: its alarm clears, as does the P1
        // item's once its hold is another.
        let mut moved =
            held("plan 6477159836@2026-10-28T08:00:00Z waits; one epic plan is applied a pass");
        moved[3].hold = Hold::new(HoldKind::Dispatched, "plan P was dispatched already, U");
        let (cleared, lines) = pass_at(&moved, &at(8), (0, false), Some(&sixth));
        assert!(lines.is_empty(), "{lines:?}");
        assert!(kinds(&cleared).iter().all(|(_, k)| k != "stalled"));
        assert_eq!(cleared["holds"][0]["since"], at(8));
        assert_eq!(
            cleared["holds"][1]["since"],
            at(0),
            "needs-human carries on"
        );
    }

    /// With candidates and no progress for `idleHours`, an alarm on the
    /// holds issue, naming the log of the pass that first raised it while it
    /// says the same; progress, or no candidates, clears it. A time from a
    /// snapshot that does not parse, or from after this pass, is no time.
    #[test]
    fn a_pass_that_moves_nothing_stalls() {
        let at = |hours: u32| format!("2026-10-28T{hours:02}:00:00Z");
        let (first, lines) = pass_at(&[], &at(0), (3, false), None);
        assert!(lines.is_empty(), "{lines:?}");
        assert_eq!(first["pass"]["progress_at"], at(0));
        let (third, _) = pass_at(&[], &at(3), (3, false), Some(&first));
        assert_eq!(third["pass"]["progress_at"], at(0));
        let (idle, lines) = pass_at(&[], &at(4), (3, false), Some(&third));
        let reason = format!(
            "no apply, dispatch or merge for 4 hours or more, since {}, with 3 candidate(s) free to start; see the pass log https://github.com/o/caller/actions/runs/9",
            at(0)
        );
        assert_eq!(lines, [format!("stall idle: {reason}")]);
        assert_eq!(
            idle["holds"],
            json!([{"item": "o/ops#1", "url": "https://github.com/o/ops/issues/1", "epic": false,
                "kind": "stalled", "reason": reason, "operator": true, "priority": null,
                "question": null, "since": at(0)}])
        );
        // A later pass, in another run, keeps the first one's log.
        let (wip, mut pass) = tally();
        pass.candidates = Some(3);
        let mut later = snapshot(&[], at(5), (wip, pass));
        carry(&mut later, Some(&idle), (false, true));
        let stall = Stall::default();
        let mut other = watch(&stall, Some(3));
        other.log = Some("https://github.com/o/caller/actions/runs/10".into());
        alarm(&mut later, Some(&idle), &other);
        assert_eq!(later.holds[0].reason, reason);
        // No candidates: no alarm, and the time carries on.
        let (none, lines) = pass_at(&[], &at(5), (0, false), Some(&idle));
        assert!(lines.is_empty() && none["holds"] == json!([]), "{lines:?}");
        assert_eq!(none["pass"]["progress_at"], at(0));
        // Progress clears it.
        let (moved, lines) = pass_at(&[], &at(5), (3, true), Some(&idle));
        assert!(lines.is_empty() && moved["holds"] == json!([]), "{lines:?}");
        assert_eq!(moved["pass"]["progress_at"], at(5));
        // A time that is not one, or is later, starts again.
        for progress in [json!("yesterday"), json!(at(9)), json!(null)] {
            let mut previous = idle.clone();
            previous["pass"]["progress_at"] = progress;
            let (fresh, lines) = pass_at(&[], &at(5), (3, false), Some(&previous));
            assert!(lines.is_empty(), "{lines:?}");
            assert_eq!(fresh["pass"]["progress_at"], at(5));
        }
    }
    /// Candidates in flight, with a pull request open or otherwise waiting
    /// by design are not free to start, so a pass with only those is no
    /// stall however long it makes no progress; one held for a reason the
    /// pass is stuck on is. Past a hold of every new run, only those count.
    #[test]
    fn only_free_candidates_idle() {
        let held = |n, kind| entry(Scope::Item(item("o/t", n)), kind, "r");
        let busy = vec![
            held(1, HoldKind::InFlight),
            held(2, HoldKind::PrOpen),
            held(2, HoldKind::OverlapIgnored),
        ];
        assert_eq!(free(Some(&ranked(&busy, 2)), &busy), Some(0));
        assert_eq!(free(None, &busy), None);
        let (idle, lines) = alarmed(&busy, "2026-10-28T09:00:00Z", (2, false), None);
        let published = parse(&render(&idle).unwrap());
        let (later, lines_later) = alarmed(
            &busy,
            "2026-10-28T20:00:00Z",
            (2, false),
            published.as_ref(),
        );
        assert!(
            lines.is_empty() && lines_later.is_empty(),
            "{lines_later:?}"
        );
        assert!(later.holds.iter().all(|h| h.kind != HoldKind::Stalled));
        let mut stuck = busy.clone();
        stuck.push(held(3, HoldKind::Unreadable));
        assert_eq!(
            free(Some(&ranked(&stuck, 4)), &stuck),
            Some(2),
            "#3 and one not held"
        );
        // Holds of items that are not ranked candidates count for nothing:
        // children under a parent that is not a live epic, and an item held
        // unreadable before ranking.
        let mut unranked = stuck.clone();
        for n in 4..=8 {
            let under = Scope::Under(item("o/t", n), item("o/t", 99));
            unranked.push(entry(under, HoldKind::EpicNotLive, "r"));
        }
        unranked.push(held(9, HoldKind::Unreadable));
        let three = ranked(&stuck, 4);
        assert_eq!(free(Some(&three), &unranked), Some(2));
        unranked.push(entry(Scope::Pass, HoldKind::Pools, "no pool has room"));
        assert_eq!(
            free(Some(&three), &unranked),
            Some(1),
            "past pools, only #3"
        );
        stuck.push(entry(Scope::Pass, HoldKind::Pools, "no pool has room"));
        assert_eq!(
            free(Some(&ranked(&stuck, 4)), &stuck),
            Some(1),
            "past pools, only #3"
        );
        let (_, lines) = alarmed(
            &stuck,
            "2026-10-28T20:00:00Z",
            (4, false),
            published.as_ref(),
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("with 1 candidate(s) free to start"),
            "{lines:?}"
        );
    }

    /// A pass that did not get as far as stage 4's candidates (WIP full,
    /// say) keeps the published item holds as they were, with their
    /// `since`, so that they go on to stall; one that did replaces them.
    #[test]
    fn item_holds_outlast_a_pass_that_did_not_read_them() {
        let mut top = entry(
            Scope::Item(item("o/t", 12)),
            HoldKind::Unreadable,
            "labels not read whole",
        );
        top.priority = Some("P0".into());
        let epic = entry(Scope::Epic(item("o/t", 9)), HoldKind::Other, "plan P waits");
        let wip = entry(Scope::Pass, HoldKind::Wip, "2 of 2 runs active");
        let (first, _) = pass_at(
            &[top.clone(), epic.clone()],
            "2026-10-28T00:00:00Z",
            (1, false),
            None,
        );
        // WIP full: stage 4 publishes no item hold of its own.
        let mut full = snapshot(
            &[wip.clone(), epic.clone()],
            "2026-10-28T03:00:00Z".into(),
            tally(),
        );
        full.pass.candidates = None;
        carry(&mut full, Some(&first), (false, false));
        let kept: Vec<(Option<String>, HoldKind, String)> = (full.holds.iter())
            .map(|h| (h.item.clone(), h.kind, h.since.clone()))
            .collect();
        let at = |t: &str| format!("2026-10-28T{t}:00Z");
        assert_eq!(
            kept,
            [
                (None, HoldKind::Wip, at("03:00")),
                (Some("o/t#9".into()), HoldKind::Other, at("00:00")),
                (Some("o/t#12".into()), HoldKind::Unreadable, at("00:00")),
            ]
        );
        let full_json = parse(&render(&full).unwrap()).unwrap();
        // Still WIP full seven hours on: the epic, which stage 3 read again,
        // stalls; #12, which no pass read since, does not.
        let mut still = snapshot(&[wip.clone(), epic.clone()], at("07:00"), tally());
        still.pass.candidates = None;
        carry(&mut still, Some(&full_json), (true, false));
        let stall = Stall::default();
        let lines = alarm(&mut still, Some(&full_json), &watch(&stall, None));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("stall hold epic o/t#9: "), "{lines:?}");
        let full = full_json;
        // Six hours on, stage 4 reads #12 held the same: it stalls.
        let (sixth, lines) = pass_at(&[top, epic.clone()], &at("06:00"), (1, true), Some(&full));
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[1].starts_with("stall hold o/t#12: "), "{lines:?}");
        // Once stage 4 reads the candidates and #12 is no longer held, it is
        // gone.
        let (gone, _) = pass_at(&[epic], &at("07:00"), (1, false), Some(&sixth));
        assert!(
            gone["holds"]
                .as_array()
                .unwrap()
                .iter()
                .all(|h| h["item"] != "o/t#12")
        );
    }
    /// A board with holds of every sort, read whole by stages 3 and 4: each
    /// item hold with its Priority, and its `since` at the given hour.
    fn waiting() -> Snapshot {
        let held = |scope, kind, priority: Option<&str>, operator: bool| Entry {
            scope,
            hold: Hold::new(kind, format!("{} reason", kind.name())).operator(operator),
            priority: priority.map(Into::into),
            why: None,
        };
        let t = |n| item("o/t", n);
        let mut asked = held(Scope::Item(t(1)), HoldKind::NeedsHuman, Some("P1"), true);
        asked.why = Some("Q: Which target?".into());
        let entries = vec![
            asked,
            held(Scope::Item(t(2)), HoldKind::NeedsOperator, Some("P0"), true),
            held(Scope::Item(t(3)), HoldKind::NeedsOperator, Some("P1"), true),
            // The epic the pass retries by itself.
            held(
                Scope::Epic(t(4)),
                HoldKind::EpicUnplanned,
                Some("P0"),
                false,
            ),
            held(Scope::Item(t(5)), HoldKind::NeedsDesign, Some("P0"), false),
            // A rerun in flight.
            held(Scope::Item(t(6)), HoldKind::InFlight, Some("P0"), false),
            held(Scope::Item(t(7)), HoldKind::Closed, Some("P0"), true),
            held(Scope::Item(t(8)), HoldKind::PrOpen, None, true),
            held(Scope::Epic(t(9)), HoldKind::NeedsOperator, None, true),
            held(Scope::Pass, HoldKind::Wip, None, false),
        ];
        let (wip, mut pass) = tally();
        pass.live_epics = Some(2);
        let mut snapshot = snapshot(&entries, "2026-10-28T09:00:00Z".into(), (wip, pass));
        // Two alarms on the holds issue: one ask.
        let alarm = |since: &str, reason: &str| Published {
            item: Some("o/ops#1".into()),
            url: Some(url(&item("o/ops", 1))),
            epic: false,
            kind: HoldKind::Stalled,
            reason: reason.into(),
            operator: true,
            priority: None,
            question: None,
            since: since.into(),
            carried: false,
            watched: false,
        };
        snapshot
            .holds
            .push(alarm("2026-10-28T06:00:00Z", "3 more hold(s)"));
        snapshot
            .holds
            .push(alarm("2026-10-28T04:00:00Z", "no apply"));
        let hours = [(1, 3), (2, 5), (3, 1), (8, 0), (9, 2)];
        for hold in &mut snapshot.holds {
            let n = hold.item.as_deref().and_then(|i| i.strip_prefix("o/t#"));
            let n: Option<u64> = n.and_then(|n| n.parse().ok());
            if let Some((_, h)) = hours.iter().find(|(m, _)| Some(*m) == n) {
                hold.since = format!("2026-10-28T{h:02}:00:00Z");
            }
        }
        snapshot
    }

    fn options(names: &[&str]) -> Vec<String> {
        names.iter().map(|&n| n.to_owned()).collect()
    }

    /// The needs-you list of `snapshot` on the holds issue o/ops#1, on a
    /// board of P0 to P3, with stage 3 on or not and every candidate
    /// considered or not.
    fn listed(
        snapshot: &Snapshot,
        previous: Option<&Value>,
        (epics, considered): (bool, bool),
    ) -> NeedsYou {
        let (on, options) = (item("o/ops", 1), options(&["P0", "P1", "P2", "P3"]));
        let reading = Reading {
            epics,
            considered,
            ..reading(&on, &options)
        };
        needs_you(snapshot, previous, &reading)
    }

    /// What [`listed`] reads: stage 3 on, every candidate considered, the
    /// list's own sources off and no summaries.
    fn reading<'a>(on: &'a ItemRef, options: &'a [String]) -> Reading<'a> {
        Reading {
            operator: "operator",
            epics: true,
            considered: true,
            options,
            on,
            reviews: Collected::off(),
            proposals: Collected::off(),
            digest: Collected::off(),
            inbox: BTreeMap::new(),
            observed: Observed::off(),
            summaries: (Read::Complete, BTreeMap::new()),
        }
    }

    /// Only the operator's holds on open items, one an item, by Priority,
    /// then the longest waiting; counted from the list itself.
    #[test]
    fn needs_you_ranks_the_operators_holds() {
        let list = listed(&waiting(), None, (true, true));
        let ranked: Vec<Value> = (list.items.iter())
            .map(|a| json!([a.key, a.source, a.kind, a.priority, a.waiting_since]))
            .collect();
        let at = |h: u32| format!("2026-10-28T{h:02}:00:00Z");
        assert_eq!(
            ranked,
            [
                json!(["o/t#2", "items", "needs-operator", "P0", at(5)]),
                json!(["o/t#3", "items", "needs-operator", "P1", at(1)]),
                json!(["o/t#1", "items", "needs-human", "P1", at(3)]),
                json!(["o/t#8", "items", "pr-open", null, at(0)]),
                json!(["o/t#9", "epics", "needs-operator", null, at(2)]),
                json!(["o/ops#1", "pass", "stalled", null, at(4)]),
            ]
        );
        assert_eq!(list.count, list.items.len());
        assert!(list.complete);
        let asks: Vec<&str> = list.items.iter().map(|a| a.ask.as_str()).collect();
        let operator = "needs-operator reason";
        assert_eq!(asks[..3], [operator, operator, "Which target?"]);
        assert_eq!(asks[5], "no apply; 3 more hold(s)", "the two alarms' asks");
        let first = &list.items[0];
        assert_eq!(first.item, "o/t#2");
        assert_eq!(first.target, "https://github.com/o/t/issues/2");
        assert_eq!(first.head, None);
        // Its order decided changes nothing.
        let mut reordered = waiting();
        reordered.holds.reverse();
        assert_eq!(listed(&reordered, None, (true, true)), list);
        // Priority goes in the board's order, an unknown one after: not by
        // name. The first hold is of every new run, #4 not the operator's.
        let mut board = waiting();
        let priorities = ["-", "Low", "Urgent", "Odd", "High"];
        for (hold, priority) in board.holds.iter_mut().zip(priorities) {
            hold.priority = Some(priority.into());
        }
        let (on, options) = (item("o/ops", 1), options(&["Urgent", "High", "Low"]));
        let read = needs_you(&board, None, &reading(&on, &options));
        let ranked: Vec<Option<String>> = (read.items.iter())
            .map(|a| a.priority.clone())
            .take(3)
            .collect();
        assert_eq!(
            ranked,
            [Some("Urgent"), Some("Low"), Some("Odd")].map(|p| p.map(Into::into))
        );
        // A pass that does not read the board ranks as the published list
        // did, not by name, so the list and its `changed_at` stay.
        let later = Snapshot {
            at: "2026-10-28T10:00:00Z".into(),
            ..board
        };
        let previous = json!({"schema": HOLDS_SCHEMA, "needs_you": read});
        let again = needs_you(&later, Some(&previous), &reading(&on, &[]));
        assert_eq!(
            (again.items, again.changed_at),
            (read.items, read.changed_at)
        );
    }

    /// Complete and empty is not the same as unread or partly read: each
    /// source says which, and `complete` only where all were read whole.
    #[test]
    fn needs_you_says_what_it_read() {
        let unreadable = |scope| entry(scope, HoldKind::Unreadable, "labels not read whole");
        type Case = (&'static str, Vec<Entry>, bool, bool, (Read, Read));
        let cases: Vec<Case> = vec![
            (
                "empty",
                vec![],
                true,
                true,
                (Read::Complete, Read::Complete),
            ),
            (
                "epics off",
                vec![],
                false,
                true,
                (Read::Off, Read::Complete),
            ),
            (
                "stage 3 unread",
                vec![],
                true,
                false,
                (Read::Unread, Read::Complete),
            ),
            (
                "an item unread",
                vec![unreadable(Scope::Item(item("o/t", 1)))],
                true,
                true,
                (Read::Complete, Read::Partial),
            ),
            (
                "an epic unread",
                vec![unreadable(Scope::Epic(item("o/t", 1)))],
                true,
                true,
                (Read::Partial, Read::Complete),
            ),
        ];
        for (name, entries, epics, listed_epics, (on_epics, on_items)) in cases {
            let (wip, mut pass) = tally();
            pass.live_epics = listed_epics.then_some(1);
            let snapshot = snapshot(&entries, "2026-10-28T09:00:00Z".into(), (wip, pass));
            let list = listed(&snapshot, None, (epics, true));
            let expected = Sources {
                epics: on_epics,
                items: on_items,
                reviews: Read::Off,
                proposals: Read::Off,
                digest: Read::Off,
                inbox: BTreeMap::new(),
            };
            assert_eq!(list.sources, expected, "{name}");
            let whole = [on_epics, on_items]
                .iter()
                .all(|r| matches!(r, Read::Complete | Read::Off));
            assert_eq!(list.complete, whole, "{name}");
            assert_eq!((list.count, list.items.len()), (0, 0), "{name}");
        }
        // Stage 4 stopped partway through its candidates, or a candidate's
        // consideration failed: the rest may wait on the operator.
        let list = listed(&waiting(), None, (true, false));
        assert_eq!(list.sources.items, Read::Partial);
        assert!(!list.complete);
        // Stage 4 did not get to its candidates: what it lists is carried.
        let mut full = waiting();
        full.pass.candidates = None;
        let list = listed(&full, None, (true, true));
        assert_eq!(list.sources.items, Read::Unread);
        assert!(!list.complete);
        assert_eq!(list.count, 6);
    }

    /// A pass that reads the same list again validates it without changing
    /// it, and writes it again only once the last write is an hour old; a
    /// change is a change at once.
    #[test]
    fn needs_you_validated_apart_from_changed() {
        let at = |m: u32| format!("2026-10-28T{:02}:{:02}:00Z", 9 + m / 60, m % 60);
        let pass_at = |snapshot: Snapshot, minutes, previous: Option<&Value>| {
            let mut snapshot = Snapshot {
                at: at(minutes),
                ..snapshot
            };
            carry(&mut snapshot, previous, (false, true));
            snapshot.needs_you = Some(listed(&snapshot, previous, (true, true)));
            snapshot
        };
        let first = pass_at(waiting(), 0, None);
        let body = render(&first).unwrap();
        assert_eq!(parse(&body), Some(serde_json::to_value(&first).unwrap()));
        let published = parse(&body);
        let again = pass_at(waiting(), 30, published.as_ref());
        let list = again.needs_you.as_ref().unwrap();
        assert_eq!(
            (list.validated_at.as_str(), list.changed_at.as_str()),
            (at(30).as_str(), at(0).as_str())
        );
        assert!(!changed(Some(&body), &again).unwrap());
        let hour = pass_at(waiting(), 60, published.as_ref());
        assert_eq!(hour.needs_you.as_ref().unwrap().changed_at, at(0));
        assert!(changed(Some(&body), &hour).unwrap(), "due again");
        let mut fewer = waiting();
        fewer.holds.retain(|h| h.item.as_deref() != Some("o/t#2"));
        let fewer = pass_at(fewer, 30, published.as_ref());
        assert_eq!(fewer.needs_you.as_ref().unwrap().changed_at, at(30));
        assert!(changed(Some(&body), &fewer).unwrap());
        // A comment without the list, or with a validation time that is
        // none, is written.
        let mut old = first;
        old.needs_you = None;
        assert!(changed(Some(&render(&old).unwrap()), &again).unwrap());
        let unparsed = body.replace(
            &format!("\"validated_at\":\"{}\"", at(0)),
            "\"validated_at\":\"soon\"",
        );
        assert_ne!(unparsed, body);
        assert!(changed(Some(&unparsed), &again).unwrap());
        // `fit` keeps it; one that cannot fit beside the operator's holds
        // keeps its highest ranked asks and counts the rest as omitted, so
        // it is not complete, and the holds are published all the same.
        let (fitted, _) = fit(again).unwrap();
        assert_eq!(fitted.needs_you.unwrap().count, 6);
        let mut over = pass_at(waiting(), 30, published.as_ref());
        // The operator's holds; its stall alarms go before any ask.
        let operators = |s: &Snapshot| {
            (s.holds.iter())
                .filter(|h| h.operator && h.kind != HoldKind::Stalled)
                .count()
        };
        let operator = operators(&over);
        let list = over.needs_you.as_mut().unwrap();
        let long = |n: usize| Ask {
            key: format!("o/work#{n}"),
            item: format!("o/work#{n}"),
            ask: format!("{n} {}", "a".repeat(MAX_ASK - 4)),
            ..list.items[0].clone()
        };
        list.items = (0..200).map(long).collect();
        list.identities = (0..200)
            .map(|n| Identity {
                key: format!("o/work#{n}"),
                detail_key: format!("o/work#{n}"),
                item: format!("o/work#{n}"),
                ..list.identities[0].clone()
            })
            .collect();
        list.count = 200;
        let items = list.items.clone();
        let (fitted, body) = fit(over).unwrap();
        assert!(chars(&body) <= MAX_COMMENT_CHARS);
        assert_eq!(operators(&fitted), operator);
        let list = fitted.needs_you.as_ref().unwrap();
        let kept = list.items.len();
        assert!(kept > 0 && kept < 200);
        assert_eq!(list.items, items[..kept]);
        assert_eq!((list.count, list.omitted, list.complete), (200, 0, true));
        assert_eq!(list.details_omitted, 200 - kept);
        assert_eq!(
            parse(&body).unwrap()["needs_you"]["identities"]
                .as_array()
                .unwrap()
                .len(),
            200
        );
        // One fewer omitted would not fit.
        let back = serde_json::to_string(&items[kept]).unwrap();
        assert!(chars(&body) + chars(&back) + 1 > MAX_COMMENT_CHARS);
    }

    /// A board whose list [`fit`] cuts, and that does not change, publishes
    /// the same list again: its `changed_at` stays, and nothing is written
    /// within the hour. One more ask past the cut, or a shown ask changed,
    /// is a change.
    #[test]
    fn fit_cut_list_is_the_same_list_next_pass() {
        let at = |m: u32| format!("2026-10-28T09:{m:02}:00Z");
        let pass = |holds, first: &str, minutes, previous: Option<&Value>| {
            let entries: Vec<Entry> = (1..=holds)
                .map(|n| {
                    let mut asked = entry(
                        Scope::Item(item("o/tracker", n)),
                        HoldKind::NeedsHuman,
                        "labelled needs-human",
                    );
                    let q = if n == 4 { first } else { "q" };
                    // Leave room for the descriptors as well as the holds;
                    // the full list still exceeds the publication size cap.
                    asked.why = Some(format!("Q: {n} {}?", q.repeat(30)));
                    asked.priority = Some(format!("P{}", n % 4));
                    asked
                })
                .collect();
            let mut snapshot = snapshot(&entries, at(minutes), tally());
            carry(&mut snapshot, previous, (false, true));
            snapshot.needs_you = Some(listed(&snapshot, previous, (true, true)));
            fit(snapshot).unwrap()
        };
        let pass_at = |minutes, previous: Option<&Value>| pass(80, "q", minutes, previous);
        let (first, body) = pass_at(0, None);
        assert_eq!((first.truncated, first.holds.len()), (0, 80));
        let list = first.needs_you.as_ref().unwrap();
        assert!(list.details_omitted > 0 && !list.items.is_empty());
        let published = parse(&body);
        let (again, _) = pass_at(30, published.as_ref());
        let list = again.needs_you.as_ref().unwrap();
        assert_eq!(list.changed_at, at(0));
        assert!(!changed(Some(&body), &again).unwrap());
        // #4 is P0, so its ask is shown; #83 is P3 and new, so past the cut.
        let shown = |s: &Snapshot, key: &str| {
            (s.needs_you.as_ref().unwrap().items.iter()).any(|a| a.key == key)
        };
        assert!(shown(&first, "o/tracker#4"));
        assert!(!shown(
            &pass(83, "q", 30, published.as_ref()).0,
            "o/tracker#83"
        ));
        for (holds, first) in [(83, "q"), (80, "r")] {
            let (other, _) = pass(holds, first, 30, published.as_ref());
            let list = other.needs_you.as_ref().unwrap();
            assert_eq!(list.changed_at, at(30), "{holds} {first}");
            assert!(changed(Some(&body), &other).unwrap(), "{holds} {first}");
        }
    }

    const HEAD_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HEAD_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// An ask of one of [`Reading`]'s own sources on `repository#number`,
    /// keyed as that source keys it.
    fn own(
        source: &str,
        (repository, number): (&str, u64),
        action: RefKind,
        head: Option<&str>,
    ) -> Ask {
        let item = format!("{repository}#{number}");
        let key = match source {
            REVIEWS => item.clone(),
            DIGEST => format!("{item}/digest"),
            _ => format!("{item}/comment-1"),
        };
        let action = Reference {
            head: head.map(Into::into),
            ..Reference::new(action, repository, number)
        };
        Ask {
            key,
            item,
            source: source.into(),
            kind: format!("{source}-kind"),
            priority: None,
            waiting_since: "2026-10-28T08:00:00Z".into(),
            ask: "Act?".into(),
            recommendation: None,
            target: action.url(),
            head: head.map(Into::into),
            source_ref: Reference::new(RefKind::Comment, repository, number),
            action,
            carried: false,
            group: None,
            chain: EpicChain::default(),
        }
    }

    fn collected(read: Read, asks: Vec<Ask>, unread: &[&str]) -> Collected {
        let unread = unread.iter().map(|&u| u.to_owned()).collect();
        Collected {
            read,
            asks,
            unread,
            review_buckets: BTreeMap::new(),
            approval_evidence: None,
        }
    }

    #[test]
    fn semantic_requests_replay_sources_revisions_and_answers() {
        let board = snapshot(&[], "2026-10-28T09:00:00Z".into(), tally());
        let on = item("o/ops", 1);
        let mut reading = reading(&on, &[]);
        let mut live = own(
            REVIEWS,
            ("o/r", 9),
            RefKind::ApprovePullRequest,
            Some(HEAD_A),
        );
        live.kind = "review-approve".into();
        live.ask = "Approve the reviewed head to release the approval wait?".into();
        let mut digest = own(
            DIGEST,
            ("o/r", 9),
            RefKind::ApprovePullRequest,
            Some(HEAD_A),
        );
        digest.kind = "digest-approve".into();
        digest.source_ref.comment = Some(17);
        digest.waiting_since = "2026-10-27T08:00:00Z".into();
        reading.reviews = collected(Read::Complete, vec![live.clone()], &[]);
        reading.digest = collected(Read::Complete, vec![digest], &[]);
        let list = needs_you(&board, None, &reading);
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].source, REVIEWS);
        let request = &list.requests[0];
        assert_eq!(request.sources.len(), 2);
        assert_eq!(request.revision.as_deref(), Some(HEAD_A));
        assert_eq!(request.requested_operator, "operator");
        assert_eq!(request.quality, "decision");
        assert!(
            request
                .actions
                .iter()
                .any(|a| a.kind == "review-at-head" && a.supported)
        );
        assert!(
            request
                .actions
                .iter()
                .filter(|a| !a.supported)
                .all(|a| !a.reason.is_empty())
        );
        // Same semantic question, new target revision. The old digest is not
        // equivalent to authorization for the new head.
        let published = as_published(&list);
        live.head = Some(HEAD_B.into());
        live.action.head = live.head.clone();
        reading.reviews.asks = vec![live];
        reading.digest.asks.clear();
        let changed = needs_you(&board, Some(&published), &reading);
        assert_eq!(changed.requests[0].identity, request.identity);
        assert_ne!(changed.requests[0].revision, request.revision);
        assert_eq!(changed.items[0].waiting_since, "2026-10-28T08:00:00Z");
        // Already answered waits are absent from the poll's complete result.
        reading.reviews.asks.clear();
        assert!(
            needs_you(&board, Some(&published), &reading)
                .items
                .is_empty()
        );
        reading.reviews.read = Read::Unread;
        let carried = needs_you(&board, Some(&published), &reading);
        assert_eq!(carried.requests[0].quality, "unverified-carried");
        assert!(carried.requests[0].actions.iter().all(|a| !a.supported));
    }

    #[test]
    fn request_quality_and_distinct_proposal_comments() {
        let list = listed(&waiting(), None, (true, true));
        assert!(
            list.requests
                .iter()
                .any(|r| r.quality == "missing-concrete-decision")
        );
        assert!(
            list.requests
                .iter()
                .all(|r| r.resolution.starts_with("Pending"))
        );
        // A bot's done verdict cannot make a still-held needs-human item
        // complete; machine reruns and design parking stay out of the list.
        assert!(
            !list
                .items
                .iter()
                .any(|a| matches!(a.kind.as_str(), "in-flight" | "needs-design" | "closed"))
        );
        let proposals = [21, 22].map(|id| {
            let mut ask = own(PROPOSALS, ("o/ops", 5), RefKind::OperatorAction, None);
            ask.action.comment = Some(id);
            ask.source_ref.comment = Some(id);
            ask.key = format!("o/ops#5/comment-{id}");
            ask
        });
        let list = own_listed(
            &[],
            "2026-10-28T09:00:00Z",
            (
                Collected::off(),
                collected(Read::Complete, proposals.into(), &[]),
            ),
            None,
        );
        assert_eq!(list.requests.len(), 2);
        assert_ne!(list.requests[0].identity, list.requests[1].identity);
        // The extension stays outside Ask: strict old record readers still
        // deserialize every legacy item without learning any new fields.
        for ask in serde_json::to_value(&list).unwrap()["items"]
            .as_array()
            .unwrap()
        {
            serde_json::from_value::<Ask>(ask.clone()).unwrap();
        }
    }

    #[test]
    fn request_replays_keep_source_clocks_and_unread_aliases() {
        let on = item("o/ops", 1);
        let board = snapshot(&[], "2026-10-28T09:00:00Z".into(), tally());
        let mut reading = reading(&on, &[]);
        let mut proposal = own(PROPOSALS, ("o/ops", 5), RefKind::OperatorAction, None);
        proposal.action.comment = Some(21);
        proposal.source_ref.comment = Some(21);
        proposal.key = "o/ops#5/comment-21".into();
        proposal.ask = "Apply the scoped label proposal?".into();
        reading.proposals = collected(Read::Complete, vec![proposal.clone()], &[]);
        let first = needs_you(&board, None, &reading);
        proposal.waiting_since = "2026-10-28T08:30:00Z".into();
        proposal.ask = "Apply the amended scoped label proposal?".into();
        reading.proposals.asks = vec![proposal];
        let edited = needs_you(&board, Some(&as_published(&first)), &reading);
        assert_eq!(edited.items[0].waiting_since, first.items[0].waiting_since);
        assert_eq!(edited.requests[0].identity, first.requests[0].identity);
        assert_ne!(edited.requests[0].revision, first.requests[0].revision);
        assert_eq!(
            edited.requests[0].source_time.as_deref(),
            Some("2026-10-28T08:30:00Z")
        );

        let mut live = own(
            REVIEWS,
            ("o/r", 9),
            RefKind::ApprovePullRequest,
            Some(HEAD_A),
        );
        live.ask = "Approve this reviewed head?".into();
        live.priority = Some("P0".into());
        let mut digest = own(
            DIGEST,
            ("o/r", 9),
            RefKind::ApprovePullRequest,
            Some(HEAD_A),
        );
        digest.ask = "Approve at the assessed head?".into();
        digest.key = "o/r#9/digest".into();
        digest.source_ref.comment = Some(17);
        digest.waiting_since = "2026-10-27T08:00:00Z".into();
        reading.proposals = Collected::off();
        reading.reviews = collected(Read::Complete, vec![live], &[]);
        reading.digest = collected(Read::Complete, vec![digest], &[]);
        let mut first = needs_you(&board, None, &reading);
        super::super::needs::inventory::revalidate(&mut first, &BTreeMap::new(), None);
        assert_eq!(first.request_sources.len(), 2);
        assert_eq!(first.items[0].waiting_since, "2026-10-27T08:00:00Z");
        reading.digest = Collected::of(Read::Unread);
        let mut later = board;
        later.at = "2026-10-28T10:00:00Z".into();
        let mut middle = needs_you(&later, Some(&as_published(&first)), &reading);
        super::super::needs::inventory::revalidate(&mut middle, &BTreeMap::new(), None);
        assert_ne!(
            middle.requests[0].observed_at,
            first.requests[0].observed_at
        );
        reading.reviews.asks.clear();
        later.at = "2026-10-28T10:30:00Z".into();
        let unread = needs_you(&later, Some(&as_published(&middle)), &reading);
        assert_eq!(unread.items.len(), 1);
        assert_eq!(unread.items[0].source, DIGEST);
        assert_eq!(unread.requests[0].quality, "unverified-carried");
        assert_eq!(
            unread.requests[0].source_time.as_deref(),
            Some("2026-10-27T08:00:00Z")
        );
        assert_eq!(
            unread.requests[0].observed_at,
            first.requests[0].observed_at
        );
        later.at = "2026-10-28T11:00:00Z".into();
        let still_unread = needs_you(&later, Some(&as_published(&unread)), &reading);
        assert_eq!(
            still_unread.requests[0].observed_at,
            first.requests[0].observed_at
        );
    }

    #[test]
    fn carried_hold_and_shared_legacy_card_remain_explicit() {
        let on = item("o/ops", 1);
        let mut reading = reading(&on, &[]);
        let mut board = waiting();
        // Modeled #621 pattern: a bot reports done while a human acceptance
        // decision is still held. The summary never releases that hold.
        reading.summaries.1.insert(
            "o/t#1".into(),
            Some(Summary {
                question: Some("Accept the offline result pending live parity evidence?".into()),
                recommendation: Some(
                    "Bot verdict: done; live acceptance has not been recorded.".into(),
                ),
                revision: None,
            }),
        );
        let first = needs_you(&board, None, &reading);
        let done = first.requests.iter().find(|r| r.key == "o/t#1").unwrap();
        assert_eq!(done.quality, "decision");
        assert!(done.resolution.starts_with("Pending"));
        board.at = "2026-10-28T10:00:00Z".into();
        board
            .holds
            .iter_mut()
            .find(|h| h.item.as_deref() == Some("o/t#1"))
            .unwrap()
            .carried = true;
        reading.considered = false;
        let partial = needs_you(&board, Some(&as_published(&first)), &reading);
        let held = partial.requests.iter().find(|r| r.key == "o/t#1").unwrap();
        assert_eq!(held.quality, "unverified-carried");
        assert_eq!(
            held.observed_at,
            first
                .requests
                .iter()
                .find(|r| r.key == "o/t#1")
                .unwrap()
                .observed_at
        );

        let blocked = entry(
            Scope::Item(item("o/r", 9)),
            HoldKind::Blocking,
            "Inspect this stopped pull request",
        );
        let mut board = snapshot(&[blocked], "2026-10-28T09:00:00Z".into(), tally());
        reading.reviews = collected(
            Read::Complete,
            vec![own(
                REVIEWS,
                ("o/r", 9),
                RefKind::ApprovePullRequest,
                Some(HEAD_A),
            )],
            &[],
        );
        let list = needs_you(&board, None, &reading);
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.requests.len(), 2);
        assert_ne!(list.requests[0].identity, list.requests[1].identity);
        assert!(list.requests.iter().all(|r| r.key == list.items[0].key));
        let mut stale = board.clone();
        stale.holds[0].kind = HoldKind::NeedsHuman;
        stale.holds[0].carried = true;
        stale.holds[0].priority = Some("P0".into());
        let mut shared = needs_you(&stale, None, &reading);
        assert_eq!(shared.identities[0].kind, "needs-human");
        super::super::needs::inventory::revalidate(
            &mut shared,
            &BTreeMap::from([(
                "o/r#9".into(),
                json!({"state":"OPEN","labels":{"nodes":[],"totalCount":0}}),
            )]),
            None,
        );
        assert_eq!(shared.items.len(), 1);
        assert_eq!(shared.requests.len(), 1);
        assert_eq!(shared.requests[0].target.kind, RefKind::ApprovePullRequest);
        assert_eq!(shared.identities[0].source, REVIEWS);
        assert_eq!(shared.counts.decisions, 1);
        assert_eq!(shared.counts.members, 1);
        reading.reviews = Collected::of(Read::Unread);
        let carried_review = needs_you(&stale, Some(&as_published(&shared)), &reading);
        assert!(
            carried_review
                .requests
                .iter()
                .any(|r| r.target.kind == RefKind::ApprovePullRequest)
        );
        reading.reviews = Collected::of(Read::Unread);
        let unread = needs_you(&board, Some(&as_published(&list)), &reading);
        assert_eq!(unread.items.len(), 1);
        assert_eq!(unread.requests.len(), 2);
        assert!(
            unread
                .requests
                .iter()
                .any(|r| r.target.kind == RefKind::ApprovePullRequest
                    && r.quality == "unverified-carried")
        );
        // Source aliases, not the combined display question, survive when
        // the blocking hold disappears without a fresh review-source read.
        let empty = snapshot(&[], "2026-10-28T10:00:00Z".into(), tally());
        let only_review = needs_you(&empty, Some(&as_published(&unread)), &reading);
        assert_eq!(only_review.requests.len(), 1);
        assert_eq!(only_review.requests[0].question, "Act?");
        board.needs_you = Some(list);
        let pad = MAX_COMMENT_CHARS - chars(&render(&board).unwrap()) + 1;
        board.holds[0].reason.push_str(&"p".repeat(pad));
        let (fitted, body) = fit(board).unwrap();
        assert!(chars(&body) <= MAX_COMMENT_CHARS);
        let list = fitted.needs_you.unwrap();
        assert!(
            list.items.is_empty() && list.requests.is_empty() && list.request_sources.is_empty()
        );
        assert_eq!(list.details_omitted, 1);
        assert_eq!(list.identities.len(), 1);
        assert_eq!(list.omitted, 0);
    }

    #[test]
    fn semantic_identity_precedes_display_clipping_and_names_the_forge() {
        let on = item("o/ops", 1);
        let reading = reading(&on, &[]);
        let entries: Vec<_> = ["first?", "second?"]
            .map(|suffix| {
                let mut held = entry(
                    Scope::Item(item("o/r", 9)),
                    HoldKind::NeedsHuman,
                    "labelled needs-human",
                );
                held.why = Some(format!("Q: {} {suffix}", "long ".repeat(MAX_ASK)));
                held
            })
            .into();
        let board = snapshot(&entries, "2026-10-28T09:00:00Z".into(), tally());
        let list = needs_you(&board, None, &reading);
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.requests.len(), 2);
        assert_ne!(list.requests[0].identity, list.requests[1].identity);
        assert!(
            list.requests
                .iter()
                .all(|r| !r.sources.is_empty() && r.question.chars().count() <= MAX_ASK + 1)
        );
        let mut forgejo = on.clone();
        forgejo.host = "https://forge.example:8443/subpath".into();
        let other = needs_you(
            &board,
            None,
            &Reading {
                on: &forgejo,
                ..reading
            },
        );
        assert_ne!(list.requests[0].identity, other.requests[0].identity);
        assert!(other.requests[0].identity.starts_with(&forgejo.host));
    }

    #[test]
    fn stranded_reviews_group_by_repository_and_reason() {
        let stopped = |repo: &str, number, reason: &str| {
            let mut ask = own(REVIEWS, (repo, number), RefKind::PullRequest, Some(HEAD_A));
            ask.kind = "review-needs-human".into();
            ask.ask = reason.into();
            ask
        };
        let draft = "a draft with no checkpoint; mark it ready if the work is complete";
        let mut asks: Vec<_> = (1..=44)
            .rev()
            .map(|n| stopped("o/bootc", n, draft))
            .collect();
        asks.push(stopped("o/other", 45, draft));
        asks.push(stopped("o/bootc", 46, "fix round 1 is blocked: no pool"));
        asks.push(stopped(
            "o/bootc",
            47,
            "fix round 2 is blocked: no capacity",
        ));
        asks.push(own(
            REVIEWS,
            ("o/bootc", 48),
            RefKind::ApprovePullRequest,
            Some(HEAD_A),
        ));
        // Midstream drafts waiting for promotion are a group of their own.
        for number in [50, 49] {
            let mut ask = stopped(
                "o/bootc",
                number,
                "a midstream draft, waiting for promotion",
            );
            ask.kind = "review-promote".into();
            asks.push(ask);
        }
        asks[0].waiting_since = "2026-10-20T09:00:00Z".into();
        asks[0].priority = Some("P1".into());
        let listed = own_listed(
            &[],
            "2026-10-28T09:00:00Z",
            (collected(Read::Complete, asks, &[]), Collected::off()),
            None,
        );
        assert_eq!(listed.count, 50);
        assert_eq!(listed.items.len(), 5);
        let promote = listed.items.iter().find(|a| a.key == "o/bootc#49").unwrap();
        assert_eq!(promote.ask, "bootc: 2 drafts waiting for promotion");
        assert_eq!(promote.kind, "review-promote");
        assert_eq!(promote.group.as_ref().unwrap().numbers, [49, 50]);
        assert!(promote.target.ends_with("draft%3Atrue"));
        let group = listed.items.iter().find(|a| a.key == "o/bootc#1").unwrap();
        assert_eq!(group.ask, "bootc: 44 drafts to inspect and promote");
        assert_eq!(
            group.group.as_ref().unwrap().numbers,
            (1..=44).collect::<Vec<_>>()
        );
        assert_eq!(group.action.kind, RefKind::PullRequest);
        assert_eq!(
            group.target,
            "https://github.com/o/bootc/pulls?q=is%3Apr+is%3Aopen+draft%3Atrue"
        );
        let blocked = listed.items.iter().find(|a| a.key == "o/bootc#46").unwrap();
        assert_eq!(blocked.ask, "bootc: 2 fix round blocked");
        // Unread repositories retain the grouped membership; a complete
        // empty read drops it. No new v2 action or key grammar is needed.
        let previous = json!({"schema": HOLDS_SCHEMA, "needs_you": listed});
        let carried = own_listed(
            &[],
            "2026-10-28T10:00:00Z",
            (collected(Read::Unread, vec![], &[]), Collected::off()),
            Some(&previous),
        );
        assert_eq!(carried.count, 50);
        assert_eq!(carried.items.len(), 5);
        for identity in &listed.identities {
            let kept = carried
                .identities
                .iter()
                .find(|i| i.key == identity.key)
                .unwrap();
            assert_eq!(kept.waiting_since, identity.waiting_since);
            assert_eq!(kept.priority, identity.priority);
        }
        assert!(carried.items.iter().all(|a| a.carried));
        assert_eq!(
            carried
                .items
                .iter()
                .find(|a| a.key == group.key)
                .unwrap()
                .group,
            group.group
        );
        let cleared = own_listed(
            &[],
            "2026-10-28T10:00:00Z",
            (collected(Read::Complete, vec![], &[]), Collected::off()),
            Some(&previous),
        );
        assert_eq!(cleared.count, 0);
        // A partial next pass has a fresh approval for the old group's
        // representative. It must not be overwritten by that carried group.
        let mut approve = own(
            REVIEWS,
            ("o/bootc", 1),
            RefKind::ApprovePullRequest,
            Some(HEAD_B),
        );
        approve.kind = "review-approve".into();
        let partial = own_listed(
            &[],
            "2026-10-28T10:00:00Z",
            (
                collected(Read::Partial, vec![approve], &["o/bootc"]),
                Collected::off(),
            ),
            Some(&previous),
        );
        let approval = partial.items.iter().find(|a| a.key == "o/bootc#1").unwrap();
        assert_eq!(approval.action.kind, RefKind::ApprovePullRequest);
        assert_eq!(approval.head.as_deref(), Some(HEAD_B));
        assert!(approval.group.is_none());
        let drafts = partial.items.iter().find(|a| a.key == "o/bootc#2").unwrap();
        assert_eq!(
            drafts.group.as_ref().unwrap().numbers,
            (2..=44).collect::<Vec<_>>()
        );
        assert_eq!(drafts.action.number, 2);
        assert!(drafts.head.is_none());
        // The normal labelled shortcut preserves a previously published
        // specific stop category instead of collapsing it to a generic one.
        let generic = stopped("o/bootc", 46, "labelled needs-human");
        let refreshed = own_listed(
            &[],
            "2026-10-28T10:00:00Z",
            (
                collected(Read::Complete, vec![generic], &[]),
                Collected::off(),
            ),
            Some(&previous),
        );
        assert_eq!(
            refreshed.items[0].group.as_ref().unwrap().reason,
            "fix round blocked"
        );
        // The shortcut reads a labelled draft as one, so a generic stop is
        // a promoted draft: its old drafts category does not return.
        let promoted = stopped("o/bootc", 3, "labelled needs-human");
        let refreshed = own_listed(
            &[],
            "2026-10-28T10:00:00Z",
            (
                collected(Read::Complete, vec![promoted], &[]),
                Collected::off(),
            ),
            Some(&previous),
        );
        let ready = refreshed.items[0].group.as_ref().unwrap();
        assert_eq!(ready.reason, "labelled needs-human");
        assert_eq!(
            refreshed.items[0].target,
            "https://github.com/o/bootc/pulls?q=is%3Apr+is%3Aopen"
        );
    }

    /// The list of `entries` at `at` with `reviews` and `proposals` read,
    /// stage 3 off, after `previous`, a published snapshot's JSON.
    fn own_listed(
        entries: &[Entry],
        at: &str,
        (reviews, proposals): (Collected, Collected),
        previous: Option<&Value>,
    ) -> NeedsYou {
        let snapshot = snapshot(entries, at.into(), tally());
        let (on, options) = (item("o/ops", 1), options(&["P0", "P1"]));
        let reading = Reading {
            epics: false,
            reviews,
            proposals,
            ..reading(&on, &options)
        };
        needs_you(&snapshot, previous, &reading)
    }

    /// `list` as a published snapshot's JSON, as the next pass reads it.
    fn as_published(list: &NeedsYou) -> Value {
        json!({"schema": HOLDS_SCHEMA, "needs_you": list})
    }

    fn green(head: &str) -> approval::Evidence {
        approval::Evidence {
            assessed_head: head.into(),
            diff: approval::Diff::Lines {
                additions: 1,
                deletions: 1,
            },
            ci: approval::Ci {
                state: approval::CiState::Green,
                complete: true,
            },
            review: None,
        }
    }

    /// Stage 2's approval rows `numbers` of `o/a` at [`HEAD_A`], each with
    /// green evidence at `head`.
    fn approvals(numbers: std::ops::RangeInclusive<u64>, head: &str) -> Collected {
        let asks = numbers
            .clone()
            .map(|n| {
                own(
                    REVIEWS,
                    ("o/a", n),
                    RefKind::ApprovePullRequest,
                    Some(HEAD_A),
                )
            })
            .collect();
        let mut reviews = collected(Read::Complete, asks, &[]);
        reviews.approval_evidence =
            Some(numbers.map(|n| (format!("o/a#{n}"), green(head))).collect());
        reviews
    }

    /// Evidence describes only a fresh approval row at the head it was read
    /// at: never another head's, another kind's, a missing row's or a row
    /// carried from the published list. Off, the field is absent.
    #[test]
    fn approval_evidence_binds_only_fresh_rows_at_their_head() {
        let at = "2026-11-05T12:00:00Z";
        let mut reviews = approvals(1..=2, HEAD_A);
        reviews
            .asks
            .push(own(REVIEWS, ("o/a", 3), RefKind::PullRequest, Some(HEAD_A)));
        let fresh = reviews.approval_evidence.as_mut().unwrap();
        fresh.insert("o/a#2".into(), green(HEAD_B));
        fresh.insert("o/a#3".into(), green(HEAD_A));
        fresh.insert("o/a#4".into(), green(HEAD_A));
        let list = own_listed(&[], at, (reviews, Collected::off()), None);
        let evidence = list.approval_evidence.as_ref().unwrap();
        assert_eq!(
            (
                evidence.schema,
                evidence.observed_at.as_str(),
                evidence.omitted
            ),
            (approval::EVIDENCE_SCHEMA, at, 0)
        );
        assert_eq!(evidence.items.keys().collect::<Vec<_>>(), ["o/a#1"]);
        // Off: no field at all, not an empty one.
        let mut off = approvals(1..=1, HEAD_A);
        off.approval_evidence = None;
        let off = own_listed(&[], at, (off, Collected::off()), None);
        assert!(
            serde_json::to_value(&off)
                .unwrap()
                .get("approval_evidence")
                .is_none()
        );
        // The next pass did not read o/a: its row is carried, and evidence
        // that claims to be fresh for it is not published as current.
        let previous = as_published(&list);
        let mut partial = collected(Read::Partial, Vec::new(), &["o/a"]);
        partial.approval_evidence = approvals(1..=1, HEAD_A).approval_evidence;
        let later = "2026-11-05T13:00:00Z";
        let carried = own_listed(&[], later, (partial, Collected::off()), Some(&previous));
        assert!(carried.items.iter().any(|a| a.key == "o/a#1" && a.carried));
        assert!(carried.approval_evidence.unwrap().items.is_empty());
        let unread = collected(Read::Unread, Vec::new(), &[]);
        let unread = own_listed(&[], later, (unread, Collected::off()), Some(&previous));
        assert!(unread.approval_evidence.is_none());
    }

    /// Past [`MAX_APPROVAL_EVIDENCE`] rows, the lowest ranked are counted,
    /// not described; every row and identity is still listed.
    #[test]
    fn approval_evidence_is_capped_and_counted() {
        let over = MAX_APPROVAL_EVIDENCE as u64 + 2;
        let mut reviews = approvals(1..=over, HEAD_A);
        reviews.asks[0].waiting_since = "2026-10-01T00:00:00Z".into();
        let list = own_listed(
            &[],
            "2026-11-05T12:00:00Z",
            (reviews, Collected::off()),
            None,
        );
        let evidence = list.approval_evidence.as_ref().unwrap();
        assert_eq!(
            (evidence.items.len(), evidence.omitted),
            (MAX_APPROVAL_EVIDENCE, 2)
        );
        assert!(evidence.items.contains_key("o/a#1"), "the longest waiting");
        assert_eq!(
            (list.count, list.items.len()),
            (over as usize, over as usize)
        );
    }

    #[test]
    fn approval_evidence_cap_is_applied_after_retention() {
        let at = "2026-11-05T12:00:00Z";
        let reviews = approvals(1..=52, HEAD_A);
        let fresh = reviews.approval_evidence.clone().unwrap();
        for removed in [1, 2, 3, 52] {
            let mut list = own_listed(&[], at, (reviews.clone(), Collected::off()), None);
            assert_eq!(list.approval_evidence.as_ref().unwrap().omitted, 2);
            // Final validation can remove rows on either side of the cap.
            list.items.drain(..removed);
            let decisions = (list.count, list.counts.clone(), list.identities.clone());
            list.retain_evidence(Some(&fresh));
            let evidence = list.approval_evidence.as_ref().unwrap();
            let qualifying = 52 - removed;
            assert_eq!(evidence.items.len(), qualifying.min(MAX_APPROVAL_EVIDENCE));
            assert_eq!(
                evidence.omitted,
                qualifying.saturating_sub(MAX_APPROVAL_EVIDENCE)
            );
            assert_eq!(
                decisions,
                (list.count, list.counts.clone(), list.identities.clone())
            );
            assert_eq!(evidence.observed_at, at);
        }
        let mut list = own_listed(&[], at, (reviews, Collected::off()), None);
        list.items[0].carried = true;
        list.items[1].head = Some(HEAD_B.into());
        list.items[2].group = Some(AskGroup {
            reason: "grouped".into(),
            numbers: vec![3, 4],
        });
        list.retain_evidence(Some(&fresh));
        let evidence = list.approval_evidence.unwrap();
        assert_eq!((evidence.items.len(), evidence.omitted), (49, 0));
        for ask in &list.items[..3] {
            assert!(!evidence.items.contains_key(&ask.key));
        }
    }

    /// Over the comment limit, evidence goes before any hold or card,
    /// lowest ranked first and counted; the next pass's same list keeps
    /// its `changed_at`, whose evidence clock alone is no change either.
    #[test]
    fn approval_evidence_goes_first_for_size_and_its_clock_is_no_change() {
        // Within NEEDS_YOU_REFRESH_HOURS, after which a write is due anyway.
        let (first, later) = ("2026-11-05T12:00:00Z", "2026-11-05T12:30:00Z");
        let with_list = |pad, at: &str| {
            let mut snapshot = bare(&board(40, pad));
            snapshot.at = at.into();
            let mut reviews = approvals(1..=3, HEAD_A);
            reviews.asks[0].waiting_since = "2026-10-01T00:00:00Z".into();
            snapshot.needs_you = Some(own_listed(&[], at, (reviews, Collected::off()), None));
            snapshot
        };
        let pad = MAX_COMMENT_CHARS - chars(&render(&with_list(0, first)).unwrap()) + 1;
        let (fitted, body) = fit(with_list(pad, first)).unwrap();
        assert!(chars(&body) <= MAX_COMMENT_CHARS);
        assert_eq!(fitted.truncated, 0);
        let list = fitted.needs_you.as_ref().unwrap();
        let evidence = list.approval_evidence.as_ref().unwrap();
        assert_eq!((list.items.len(), list.count, list.omitted), (3, 3, 0));
        assert_eq!(evidence.omitted, 1);
        assert_eq!(evidence.items.len(), 2);
        assert!(evidence.items.contains_key("o/a#1"));
        let published = parse(&body).unwrap();
        let mut next = with_list(pad, later);
        let list = next.needs_you.as_mut().unwrap();
        preserve_changed_at(list, Some(&published));
        assert_eq!(list.changed_at, first);
        assert_eq!(list.approval_evidence.as_ref().unwrap().observed_at, later);
        let (next, _) = fit(next).unwrap();
        assert!(!changed(Some(&body), &next).unwrap());
        // A changed reading is a change.
        let mut moved = with_list(pad, later);
        let list = moved.needs_you.as_mut().unwrap();
        let evidence = list.approval_evidence.as_mut().unwrap();
        evidence.items.get_mut("o/a#1").unwrap().ci.state = approval::CiState::Pending;
        preserve_changed_at(list, Some(&published));
        assert_eq!(list.changed_at, later);
    }

    #[test]
    fn inbox_sources_preserve_clocks_counts_and_partial_waits() {
        let (on, options) = (item("o/ops", 1), options(&["P0", "P1"]));
        let mut document = own(DECISIONS, ("o/ops", 2), RefKind::Issue, None);
        document.key.push_str("/decision-1");
        document.priority = Some("P0".into());
        document.ask = "Ship the bounded inbox publication?".into();
        let mut draft = own(DRAFTS, ("o/ops", 3), RefKind::Issue, None);
        draft.key.push_str("/draft");
        draft.ask = "Inspect the newest review draft?".into();
        let fresh = || {
            BTreeMap::from([
                (
                    DECISIONS.into(),
                    collected(Read::Complete, vec![document.clone()], &[]),
                ),
                (
                    DRAFTS.into(),
                    collected(Read::Complete, vec![draft.clone()], &[]),
                ),
            ])
        };
        let mut board = snapshot(
            &[entry(
                Scope::Item(item("o/r", 4)),
                HoldKind::InFlight,
                "working",
            )],
            "2026-10-28T09:00:00Z".into(),
            tally(),
        );
        let mut input = reading(&on, &options);
        input.inbox = fresh();
        let first = needs_you(&board, None, &input);
        assert_eq!(
            (first.count, first.counts.decisions, first.counts.members),
            (2, 2, 2)
        );
        assert_eq!(first.context.items.len(), 1);
        assert_eq!(first.requests[0].quality, "decision");
        let previous = as_published(&first);
        board.at = "2026-10-28T09:30:00Z".into();
        let again = needs_you(&board, Some(&previous), &input);
        assert_eq!(again.changed_at, first.changed_at);
        assert_eq!(again.validated_at, board.at);
        for read in [Read::Unread, Read::Partial] {
            input
                .inbox
                .insert(DRAFTS.into(), collected(read, vec![], &["o/ops#3"]));
            let partial = needs_you(&board, Some(&previous), &input);
            assert!(!partial.complete);
            assert_eq!(partial.counts, first.counts);
            let carried = partial
                .requests
                .iter()
                .find(|r| r.key == draft.key)
                .unwrap();
            assert_eq!(
                carried.observed_at.as_deref(),
                Some(first.validated_at.as_str())
            );
            assert_eq!(carried.quality, "unverified-carried");
            assert!(
                partial
                    .items
                    .iter()
                    .find(|a| a.key == draft.key)
                    .unwrap()
                    .carried
            );
        }
        input.inbox = fresh();
        input.inbox.get_mut(DRAFTS).unwrap().asks.clear();
        let resolved = needs_you(&board, Some(&previous), &input);
        assert_eq!(resolved.counts.decisions, 1);
        // Detail eviction never rewrites the canonical known totals.
        let mut cut = first.clone();
        cut.cut().unwrap();
        assert_eq!((cut.count, cut.details_omitted), (2, 1));
        assert_eq!(cut.counts, first.counts);
        assert_eq!(cut.omitted, 0);
        assert!(cut.omitted_sources.is_empty());
    }

    /// Steps, failed runs and held limits fold into `context` in source
    /// order, each source capped, none of it counted as an ask; their
    /// facts, readings and clocks alone write nothing, and an unread usage
    /// snapshot lists no limit.
    #[test]
    fn context_folds_steps_runs_and_held_limits() {
        let (on, options) = (item("o/ops", 1), options(&["P0"]));
        let fact = |source, n: usize| ContextFact {
            source,
            item: Some(format!("o/r#{n}")),
            kind: "waiting".into(),
            reason: format!("wait: checks pending {n:02}"),
            url: None,
            since: None,
            observed_at: Some("2026-10-28T09:00:00Z".into()),
        };
        let usage = |used: f64, at: &str| {
            json!({"pools": {
                "claude": {"hold": true, "reason": "pace", "used_percent": used, "limit_percent": 40.0,
                    "resets_at": "2026-10-30T00:00:00Z", "observed_at": at, "burst": 5},
                "openai": {"hold": false, "used_percent": 3.0},
            }, "budget": {"weekly_usd": 100.0, "spent_usd": 101.0 + used, "prices": "list"}})
        };
        let mut board = snapshot(
            &[
                entry(Scope::Item(item("o/r", 4)), HoldKind::InFlight, "working"),
                entry(Scope::Pass, HoldKind::Budget, "spent; new runs wait"),
            ],
            "2026-10-28T09:00:00Z".into(),
            tally(),
        );
        let mut input = reading(&on, &options);
        let off = needs_you(&board, None, &input);
        input.observed = Observed {
            steps: (Read::Complete, vec![fact(CONTEXT_STEP, 1)]),
            runs: (
                Read::Complete,
                (0..12).map(|n| fact(CONTEXT_RUN, n)).collect(),
            ),
            usage: (Read::Complete, Some(usage(80.0, "2026-10-28T08:55:00Z"))),
        };
        let first = needs_you(&board, None, &input);
        assert_eq!((first.counts.clone(), first.count), (off.counts, off.count));
        let sources: Vec<_> = first.context.items.iter().map(|f| f.source).collect();
        let mut expected = vec![CONTEXT_HOLD; 2];
        expected.push(CONTEXT_STEP);
        expected.extend([CONTEXT_RUN; 10]);
        assert_eq!(sources, expected);
        assert_eq!(first.context.omitted, 2);
        assert_eq!(
            serde_json::to_value(&first.context.limits).unwrap(),
            json!({
                "budget": {"weekly_usd": 100.0, "spent_usd": 181.0},
                "claude": {"reason": "pace", "used_percent": 80.0, "limit_percent": 40.0,
                    "resets_at": "2026-10-30T00:00:00Z", "observed_at": "2026-10-28T08:55:00Z"},
            })
        );
        board.needs_you = Some(first.clone());
        let (_, body) = fit(board.clone()).unwrap();
        board.at = "2026-10-28T09:30:00Z".into();
        input.observed.usage.1 = Some(usage(85.0, "2026-10-28T09:25:00Z"));
        input.observed.steps.1[0].reason = "wait: checks pending 99".into();
        input.observed.runs.1.pop();
        let again = needs_you(&board, Some(&published(&body).unwrap()), &input);
        assert_eq!(again.changed_at, first.changed_at);
        board.needs_you = Some(again);
        assert!(!changed(Some(&body), &board).unwrap());
        input.observed.usage = (Read::Unread, None);
        let unread = needs_you(&board, Some(&published(&body).unwrap()), &input);
        assert_eq!(unread.context.sources[CONTEXT_USAGE], Read::Unread);
        assert!(unread.context.limits.is_empty());
        assert_ne!(unread.changed_at, first.changed_at);
    }

    /// The real 3 November publication and holds, spend removed, copied from
    /// review's test/fixtures. Its own sources were partial, so the replay
    /// reads none of them: every ask it listed is carried or a hold's.
    const REAL_SNAPSHOT: &str = include_str!("../../../../fixtures/needs-you-real-2026-11-03.json");

    #[test]
    fn real_publication_replay() {
        let raw: Value = serde_json::from_str(REAL_SNAPSHOT).unwrap();
        assert_eq!(raw["schema"], HOLDS_SCHEMA);
        let mut board = snapshot(&[], raw["at"].as_str().unwrap().into(), tally());
        board.holds = serde_json::from_value(raw["holds"].clone()).unwrap();
        board.wip.limit = raw["wip"]["limit"].as_u64().unwrap();
        board.wip.in_flight = raw["wip"]["in_flight"].as_u64().map(|n| n as usize);
        let (on, options) = (item("redacted/ops", 1), options(&["P0", "P1", "P2", "P3"]));
        let mut input = reading(&on, &options);
        input.considered = false;
        input.reviews = Collected::of(Read::Unread);
        input.proposals = Collected::of(Read::Unread);
        input.digest = Collected::of(Read::Unread);
        board.needs_you = Some(needs_you(&board, Some(&raw), &input));
        let list = board.needs_you.as_ref().unwrap();
        assert!(
            !list.complete,
            "the real snapshot is partial, not an empty/full census"
        );
        assert!(list.counts.decisions > raw["needs_you"]["count"].as_u64().unwrap() as usize);
        let keys: BTreeSet<_> = list.requests.iter().map(|r| &r.identity).collect();
        assert_eq!(keys.len(), list.counts.decisions);
        assert!(list.counts.groups <= list.counts.decisions);
        assert!(list.counts.members >= list.counts.decisions);
        assert_eq!(list.count + list.omitted, list.counts.decisions);
        // Global order: Priority, then age.
        let rank = |a: &Ask| {
            let priority = a.priority.as_deref();
            let p = ["P0", "P1", "P2", "P3"]
                .iter()
                .position(|p| Some(*p) == priority);
            (p.unwrap_or(usize::MAX), a.waiting_since.clone())
        };
        assert!(list.items.windows(2).all(|w| rank(&w[0]) <= rank(&w[1])));
        let totals = list.counts.clone();
        let (fitted, body) = fit(board.clone()).unwrap();
        assert!(body.chars().count() <= MAX_COMMENT_CHARS);
        let fitted = fitted.needs_you.unwrap();
        assert_eq!(fitted.counts, totals);
        assert_eq!(
            fitted.omitted_sources.values().sum::<usize>(),
            fitted.omitted
        );
        let previous = published(&body).unwrap();
        board.at = (parse_time(&board.at).unwrap() + time::Duration::minutes(30))
            .format(&Rfc3339)
            .unwrap();
        board.needs_you = Some(needs_you(&board, Some(&previous), &input));
        let (_, again) = fit(board.clone()).unwrap();
        let again = published(&again).unwrap();
        assert_eq!(
            again["needs_you"]["changed_at"],
            previous["needs_you"]["changed_at"]
        );
        assert_eq!(again["needs_you"]["validated_at"], board.at.as_str());
        // One more source, read and then failing: its ask is carried, the
        // known totals stay, and the list does not claim to be complete.
        let mut document = own(DECISIONS, ("redacted/ops", 2), RefKind::Issue, None);
        document.key.push_str("/decision-1");
        // Only a published ask can be carried: this one ranks first.
        (document.priority, document.waiting_since) =
            (Some("P0".into()), "2026-10-01T00:00:00Z".into());
        input.inbox = BTreeMap::from([(
            DECISIONS.into(),
            collected(Read::Complete, vec![document.clone()], &[]),
        )]);
        let read = needs_you(&board, Some(&previous), &input);
        assert_eq!(read.sources.inbox[DECISIONS], Read::Complete);
        assert_eq!(read.counts.decisions, totals.decisions + 1);
        board.needs_you = Some(read.clone());
        let (_, read_body) = fit(board.clone()).unwrap();
        input.inbox = BTreeMap::from([(DECISIONS.into(), Collected::of(Read::Unread))]);
        let failed = needs_you(&board, Some(&published(&read_body).unwrap()), &input);
        assert_eq!(failed.sources.inbox[DECISIONS], Read::Unread);
        assert!(!failed.complete);
        assert_eq!(failed.counts, read.counts);
        let carried = failed.requests.iter().find(|r| r.key == document.key);
        assert_eq!(
            carried.map(|r| r.quality.as_str()),
            Some("unverified-carried")
        );
    }

    /// An issue held on the operator's approval of its pull request and
    /// that pull request's review wait are one ask, the review's, with its
    /// head and the hold's Priority and question; without the review, the
    /// hold is listed. A review wait keeps its `waiting_since` while its head
    /// does, and starts again at a new head.
    #[test]
    fn needs_you_joins_a_hold_and_its_review() {
        let mut held = entry(
            Scope::Item(item("o/t", 7)),
            HoldKind::PrOpen,
            "approve pull request o/t#50 to merge",
        );
        held.hold.operator = true;
        held.priority = Some("P1".into());
        held.why = Some("Q: Merge the fix?".into());
        let approve = own(
            REVIEWS,
            ("o/t", 50),
            RefKind::ApprovePullRequest,
            Some(HEAD_A),
        );
        let reviews = collected(Read::Complete, vec![approve.clone()], &[]);
        let none = || collected(Read::Complete, vec![], &[]);
        let at = "2026-10-28T09:00:00Z";
        let list = own_listed(&[held.clone()], at, (reviews, Collected::off()), None);
        let asks: Vec<Value> = (list.items.iter())
            .map(|a| json!([a.key, a.source, a.priority, a.ask, a.head, a.waiting_since]))
            .collect();
        let since = "2026-10-28T08:00:00Z";
        assert_eq!(
            asks,
            [json!([
                "o/t#50",
                REVIEWS,
                "P1",
                "Merge the fix?",
                HEAD_A,
                since
            ])]
        );
        assert_eq!(list.items[0].action.kind, RefKind::ApprovePullRequest);
        assert!(list.complete);
        let list = own_listed(&[held.clone()], at, (none(), Collected::off()), None);
        let keys: Vec<&str> = list.items.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, ["o/t#7"]);
        // Two holds on one approval are one ask, with the most urgent
        // Priority, whichever comes first, with the review or without.
        let mut urgent = entry(
            Scope::Item(item("o/t", 8)),
            HoldKind::PrOpen,
            "approve pull request o/t#50 to merge",
        );
        urgent.hold.operator = true;
        urgent.priority = Some("P0".into());
        for entries in [
            [held.clone(), urgent.clone()],
            [urgent.clone(), held.clone()],
        ] {
            for (reviews, key) in [(vec![approve.clone()], "o/t#50"), (vec![], "o/t#8")] {
                let reviews = collected(Read::Complete, reviews, &[]);
                let list = own_listed(&entries, at, (reviews, Collected::off()), None);
                let asks: Vec<(&str, Option<&str>)> = (list.items.iter())
                    .map(|a| (a.key.as_str(), a.priority.as_deref()))
                    .collect();
                assert_eq!(asks, [(key, Some("P0"))], "{key}");
                assert_eq!(list.count, 1, "{key}");
            }
        }
        // A review wait carried over is joined with its hold as a live one
        // is: one ask, not the review's and the hold's.
        let reviews = collected(Read::Complete, vec![approve.clone()], &[]);
        let first = own_listed(&[held.clone()], at, (reviews, Collected::off()), None);
        let previous = as_published(&first);
        for reviews in [
            collected(Read::Unread, vec![], &[]),
            collected(Read::Partial, vec![], &["o/t"]),
        ] {
            let read = reviews.read;
            let list = own_listed(&[held.clone()], at, (reviews, none()), Some(&previous));
            let asks: Vec<Value> = (list.items.iter())
                .map(|a| json!([a.key, a.carried, a.priority, a.ask]))
                .collect();
            let expected = json!(["o/t#50", true, "P1", "Merge the fix?"]);
            assert_eq!(asks, [expected], "{read:?}");
            assert_eq!(list.count, 1, "{read:?}");
        }
        // The same head an hour on waits since it was first seen; a new head
        // is a new ask.
        let wait = |head, since: &str| Ask {
            waiting_since: since.into(),
            ..own(REVIEWS, ("o/t", 60), RefKind::PullRequest, Some(head))
        };
        let first = own_listed(
            &[],
            at,
            (
                collected(Read::Complete, vec![wait(HEAD_A, at)], &[]),
                Collected::off(),
            ),
            None,
        );
        let previous = as_published(&first);
        for (head, expected) in [(HEAD_A, at), (HEAD_B, "2026-10-28T10:00:00Z")] {
            let now = "2026-10-28T10:00:00Z";
            let reviews = collected(Read::Complete, vec![wait(head, now)], &[]);
            let list = own_listed(&[], now, (reviews, Collected::off()), Some(&previous));
            assert_eq!(list.items[0].waiting_since, expected, "{head}");
        }
    }

    /// A source read partly keeps the asks published from what it did not
    /// read, marked carried, and one unread keeps them all; what it read
    /// again is current, so an ask gone from it is gone. Either is not
    /// complete. A published ask that is not well formed is not kept.
    #[test]
    fn needs_you_keeps_what_it_did_not_read() {
        let at = "2026-10-28T09:00:00Z";
        let asks = vec![
            own(REVIEWS, ("o/a", 1), RefKind::PullRequest, Some(HEAD_A)),
            own(
                REVIEWS,
                ("o/b", 2),
                RefKind::ApprovePullRequest,
                Some(HEAD_A),
            ),
        ];
        let proposals = vec![own(PROPOSALS, ("o/ops", 5), RefKind::OperatorAction, None)];
        let first = own_listed(
            &[],
            at,
            (
                collected(Read::Complete, asks, &[]),
                collected(Read::Complete, proposals, &[]),
            ),
            None,
        );
        assert!(first.complete);
        let mut previous = as_published(&first);
        let mut tampered = previous["needs_you"]["items"][0].clone();
        tampered["key"] = "evil/x#1".into();
        tampered["action"]["repository"] = "evil".into();
        let mut moved = previous["needs_you"]["items"][0].clone();
        moved["key"] = "o/b#3".into();
        moved["head"] = "HEAD".into();
        moved["action"]["head"] = "HEAD".into();
        let items = previous["needs_you"]["items"].as_array_mut().unwrap();
        items.extend([tampered, moved]);
        type Case = (
            &'static str,
            Collected,
            Collected,
            Vec<(&'static str, bool)>,
        );
        let cases: Vec<Case> = vec![
            (
                "o/b not read, the proposals unread",
                collected(Read::Partial, vec![], &["o/b"]),
                collected(Read::Unread, vec![], &[]),
                vec![("o/b#2", true), ("o/ops#5/comment-1", true)],
            ),
            (
                "o/b#2 read again",
                collected(
                    Read::Partial,
                    vec![own(
                        REVIEWS,
                        ("o/b", 2),
                        RefKind::ApprovePullRequest,
                        Some(HEAD_A),
                    )],
                    &["o/b"],
                ),
                collected(Read::Complete, vec![], &[]),
                vec![("o/b#2", false)],
            ),
            (
                "both read whole: none waits",
                collected(Read::Complete, vec![], &[]),
                collected(Read::Complete, vec![], &[]),
                vec![],
            ),
        ];
        for (name, reviews, proposals, expected) in cases {
            let whole = reviews.read == Read::Complete && proposals.read == Read::Complete;
            let list = own_listed(&[], at, (reviews, proposals), Some(&previous));
            let listed: Vec<(&str, bool)> = (list.items.iter())
                .map(|a| (a.key.as_str(), a.carried))
                .collect();
            assert_eq!(listed, expected, "{name}");
            assert_eq!(list.complete, whole, "{name}");
            assert_eq!(list.count, list.items.len(), "{name}");
        }
    }

    #[test]
    fn evicted_details_do_not_erase_unread_decisions() {
        let on = item("o/ops", 1);
        let mut reading = reading(&on, &[]);
        reading.proposals = collected(
            Read::Complete,
            (1..=150)
                .map(|n| own(PROPOSALS, ("o/ops", n), RefKind::OperatorAction, None))
                .collect(),
            &[],
        );
        let mut board = snapshot(&[], "2026-10-28T09:00:00Z".into(), tally());
        let first = needs_you(&board, None, &reading);
        board.needs_you = Some(first);
        let (fitted, body) = fit(board).unwrap();
        let fitted = fitted.needs_you.unwrap();
        assert_eq!(fitted.identities.len(), 150);
        assert!(fitted.items.len() < 150);
        let previous = parse(&body).unwrap();
        let board = snapshot(&[], "2026-10-28T09:00:00Z".into(), tally());
        reading.proposals = Collected::of(Read::Unread);
        let unread = needs_you(&board, Some(&previous), &reading);
        assert_eq!(unread.count, 150);
        assert_eq!((unread.counts.decisions, unread.counts.members), (150, 150));
        assert_eq!(unread.identities.len(), 150);
        assert!(unread.identities.iter().all(|i| i.carried));
        assert_eq!(unread.details_omitted, 150 - unread.items.len());
        // A failure in a different repository must not retain decisions
        // whose own repository was successfully refreshed and retired them.
        reading.proposals = collected(Read::Partial, vec![], &["o/other"]);
        assert_eq!(needs_you(&board, Some(&previous), &reading).count, 0);
        reading.proposals = collected(Read::Partial, vec![], &["o/ops"]);
        assert_eq!(needs_you(&board, Some(&previous), &reading).count, 150);
        reading.proposals = Collected::of(Read::Complete);
        assert_eq!(needs_you(&board, Some(&previous), &reading).count, 0);
        // So do an inbox source's, until it is read whole or not configured.
        let drafts = (1..=150)
            .map(|n| {
                let mut ask = own(DRAFTS, ("o/ops", n), RefKind::Issue, None);
                ask.key.push_str("/draft");
                ask
            })
            .collect();
        reading.inbox = BTreeMap::from([(DRAFTS.into(), collected(Read::Complete, drafts, &[]))]);
        let mut board = snapshot(&[], "2026-10-28T09:00:00Z".into(), tally());
        board.needs_you = Some(needs_you(&board, None, &reading));
        let (_, body) = fit(board.clone()).unwrap();
        let previous = parse(&body).unwrap();
        reading.inbox = BTreeMap::from([(DRAFTS.into(), Collected::of(Read::Unread))]);
        let unread = needs_you(&board, Some(&previous), &reading);
        assert_eq!((unread.count, unread.counts.decisions), (150, 150));
        reading.inbox = BTreeMap::from([(DRAFTS.into(), Collected::of(Read::Complete))]);
        assert_eq!(needs_you(&board, Some(&previous), &reading).count, 0);
        reading.inbox.clear();
        assert_eq!(needs_you(&board, Some(&previous), &reading).count, 0);
    }

    #[test]
    fn unreadable_item_keeps_its_evicted_decision() {
        let on = item("o/ops", 1);
        let reading = reading(&on, &[]);
        let held = entry(Scope::Item(item("o/r", 9)), HoldKind::NeedsHuman, "help");
        let board = snapshot(&[held], "2026-10-28T09:00:00Z".into(), tally());
        let mut list = needs_you(&board, None, &reading);
        assert_eq!(list.identities.len(), 1);
        list.items.clear();
        list.requests.clear();
        let previous = json!({"schema": HOLDS_SCHEMA, "needs_you": list});
        let unread = entry(
            Scope::Item(item("o/r", 9)),
            HoldKind::Unreadable,
            "read failed",
        );
        let board = snapshot(&[unread], "2026-10-28T10:00:00Z".into(), tally());
        let list = needs_you(&board, Some(&previous), &reading);
        assert_eq!(list.identities.len(), 1);
        assert!(list.identities[0].carried);
    }

    /// Past the old cap every identity and ask is still available; each ask and recommendation is one line
    /// of at most [`MAX_ASK`] characters.
    #[test]
    fn needs_you_is_bounded() {
        let long = format!("{}\nsecond line", "a".repeat(MAX_ASK + 10));
        let asks: Vec<Ask> = (1..=MAX_NEEDS_YOU as u64 + 7)
            .map(|n| Ask {
                ask: long.clone(),
                recommendation: Some(format!("\n  {long}")),
                ..own(PROPOSALS, ("o/ops", n), RefKind::OperatorAction, None)
            })
            .collect();
        let proposals = collected(Read::Complete, asks, &[]);
        let list = own_listed(
            &[],
            "2026-10-28T09:00:00Z",
            (Collected::off(), proposals),
            None,
        );
        assert_eq!(
            (list.count, list.items.len(), list.omitted),
            (MAX_NEEDS_YOU + 7, MAX_NEEDS_YOU + 7, 0)
        );
        assert!(list.complete);
        assert_eq!(list.identities.len(), MAX_NEEDS_YOU + 7);
        let cut = format!("{}…", "a".repeat(MAX_ASK));
        assert!(
            list.items
                .iter()
                .all(|a| a.ask == cut && a.recommendation.as_ref() == Some(&cut))
        );
    }

    /// A bot summary is read as data: the rest of its first bold `Decision
    /// needed` and `Recommendation` lines, one line each; an empty one is
    /// absent, so that the Why's question or the hold's reason stands.
    #[test]
    fn summaries_ask_or_fall_back() {
        let cases = [
            (
                "**Decision needed:** Ship?\n**Recommendation:** Yes.",
                Some("Ship?"),
                Some("Yes."),
            ),
            (
                "- **Decision needed**: Ship?\n  - **Recommendation**:  Yes.  ",
                Some("Ship?"),
                Some("Yes."),
            ),
            ("**Decision needed:**\n**Recommendation:**   ", None, None),
            (
                "Decision needed: Ship?\nRecommendation: Yes.",
                Some("Ship?"),
                Some("Yes."),
            ),
            (
                "**QUESTION FOR THE OPERATOR:** Ship?\n**RECOMMENDED:** Yes.",
                Some("Ship?"),
                Some("Yes."),
            ),
            (
                "Question for the operator: Ship?\nRecommended: Yes.",
                Some("Ship?"),
                Some("Yes."),
            ),
            (
                "- **Question for the operator**: Ship?\n- **Recommended**: Yes.",
                Some("Ship?"),
                Some("Yes."),
            ),
            ("A bold opening is not a question.\n**Ship?**", None, None),
            (
                "**Decision needed:** First\n**Decision needed:** Second",
                Some("First"),
                None,
            ),
        ];
        for (body, question, recommendation) in cases {
            let said = summary(body);
            let expected = Summary {
                question: question.map(Into::into),
                recommendation: recommendation.map(Into::into),
                revision: None,
            };
            assert_eq!(said, expected, "{body}");
        }
        let mut asked = entry(
            Scope::Item(item("o/t", 1)),
            HoldKind::NeedsHuman,
            "labelled needs-human",
        );
        asked.why = Some("Q: Which target?".into());
        let plain = entry(
            Scope::Item(item("o/t", 2)),
            HoldKind::NeedsHuman,
            "labelled needs-human",
        );
        let entries = [asked, plain];
        let snapshot = super::snapshot(&entries, "2026-10-28T09:00:00Z".into(), tally());
        let (on, options) = (item("o/ops", 1), options(&["P0"]));
        let said = |question: Option<&str>| Summary {
            question: question.map(Into::into),
            recommendation: Some("Do it.".into()),
            revision: None,
        };
        for (question, expected) in [
            (Some("Ship 1.0?"), ["Ship 1.0?", "Ship 1.0?"]),
            (None, ["Which target?", "labelled needs-human"]),
        ] {
            let summaries = [("o/t#1", said(question)), ("o/t#2", said(question))];
            let reading = Reading {
                summaries: (
                    Read::Partial,
                    summaries
                        .into_iter()
                        .map(|(k, s)| (k.into(), Some(s)))
                        .collect(),
                ),
                ..reading(&on, &options)
            };
            let list = needs_you(&snapshot, None, &reading);
            let asks: Vec<&str> = list.items.iter().map(|a| a.ask.as_str()).collect();
            assert_eq!(asks, expected);
            assert!(
                list.items
                    .iter()
                    .all(|a| a.recommendation.as_deref() == Some("Do it."))
            );
            assert_eq!(list.summaries, Read::Partial);
        }
        // A summary not read again keeps what the same hold published, and
        // the list its `changed_at`, however much of the summaries was read;
        // one read without a question, or a hold whose Why changed, falls
        // back.
        let read = |whole, summaries: &[(&str, Option<Summary>)]| Reading {
            summaries: (
                whole,
                (summaries.iter())
                    .map(|(k, s)| ((*k).to_owned(), s.clone()))
                    .collect(),
            ),
            ..reading(&on, &options)
        };
        let shipped = Some(said(Some("Ship 1.0?")));
        let summaries = [("o/t#1", shipped.clone()), ("o/t#2", shipped)];
        let first = needs_you(&snapshot, None, &read(Read::Complete, &summaries));
        let mut published = serde_json::to_value(&snapshot).unwrap();
        published["needs_you"] = serde_json::to_value(&first).unwrap();
        let later = |held: &[Entry]| {
            let mut later = super::snapshot(held, "2026-10-28T10:00:00Z".into(), tally());
            carry(&mut later, Some(&published), (false, true));
            later
        };
        let mut why = entries.clone();
        why[0].why = Some("Q: Which host?".into());
        let (later, rewhy) = (later(&entries), later(&why));
        let shipped = ["Ship 1.0?", "Ship 1.0?"];
        let fallback = ["Which target?", "labelled needs-human"];
        let cases = [
            ("unread", &later, vec![], shipped, Some("Do it."), true),
            (
                "read, none",
                &later,
                vec![("o/t#1", None), ("o/t#2", None)],
                fallback,
                None,
                false,
            ),
            (
                "Why changed",
                &rewhy,
                vec![],
                ["Which host?", "Ship 1.0?"],
                None,
                false,
            ),
        ];
        for (name, snapshot, summaries, asks, recommendation, same) in cases {
            let list = needs_you(snapshot, Some(&published), &read(Read::Partial, &summaries));
            let said: Vec<&str> = list.items.iter().map(|a| a.ask.as_str()).collect();
            assert_eq!(said, asks, "{name}");
            let recommended = list.items[0].recommendation.as_deref();
            assert_eq!(recommended, recommendation, "{name}");
            let changed_at = if same { &first.changed_at } else { &later.at };
            assert_eq!(&list.changed_at, changed_at, "{name}");
        }
    }

    /// Many stuck holds at once, as in a label outage, raise at most
    /// [`MAX_STALLS`] alarms of their own, each repeating at most
    /// [`MAX_STALL_REASON`] characters of its hold's reason, and one that
    /// counts the rest; a line each all the same. Even past that, alarms
    /// never keep the snapshot from fitting: `fit` leaves them out after the
    /// other holds that do not wait on the operator.
    #[test]
    fn many_stalls_never_stop_the_holds() {
        let long = "labels not read whole ".repeat(100);
        let held: Vec<Entry> = (1..=200)
            .map(|n| {
                let mut e = entry(Scope::Item(item("o/t", n)), HoldKind::Unreadable, &long);
                e.priority = Some("P0".into());
                e
            })
            .collect();
        let (first, _) = pass_at(&held, "2026-10-28T00:00:00Z", (0, false), None);
        let (stalled, lines) = alarmed(&held, "2026-10-28T07:00:00Z", (0, false), Some(&first));
        assert_eq!(lines.len(), 200);
        let alarms: Vec<&Published> = (stalled.holds.iter())
            .filter(|h| h.kind == HoldKind::Stalled)
            .collect();
        assert_eq!(alarms.len(), MAX_STALLS + 1);
        assert!(alarms[..MAX_STALLS].iter().all(|a| a.reason.ends_with('…')));
        assert!(alarms.iter().all(|a| a.reason.chars().count() < 400));
        let more = alarms[MAX_STALLS];
        assert_eq!(more.item.as_deref(), Some("o/ops#1"));
        assert_eq!(
            more.reason,
            "190 more hold(s) the same for 6 hours or more, among them o/t#11, o/t#12, o/t#13 and 187 more; the pass's stall lines name them all"
        );
        let (fitted, body) = fit(stalled).unwrap();
        assert!(chars(&body) <= MAX_COMMENT_CHARS);
        let kept = (fitted.holds.iter()).filter(|h| h.kind == HoldKind::Stalled);
        assert_eq!(kept.count(), MAX_STALLS + 1, "the holds go first");
        // Each kept alarm keeps its hold, and so its `since`.
        let of = |kind| (fitted.holds.iter()).filter(move |h| h.kind == kind);
        for alarm in of(HoldKind::Stalled).take(MAX_STALLS) {
            assert!(of(HoldKind::Unreadable).any(|h| h.item == alarm.item));
        }
        // Alarms alone past the limit are left out rather than fail.
        let alarm = Published {
            kind: HoldKind::Stalled,
            operator: true,
            reason: "r".repeat(1000),
            ..bare(&held[..1]).holds[0].clone()
        };
        let mut flood = bare(&held[..1]);
        flood.holds = vec![alarm; 100];
        let (fitted, body) = fit(flood).unwrap();
        assert!(chars(&body) <= MAX_COMMENT_CHARS);
        assert!(fitted.truncated > 0);
        // Past the item alarms too, the one on the holds issue, which
        // speaks for many, is kept longest.
        let longer = "labels not read whole ".repeat(400);
        let held: Vec<Entry> = (1..=MAX_STALLS as u64 + 2)
            .map(|n| {
                let mut e = entry(Scope::Item(item("o/t", n)), HoldKind::Unreadable, &longer);
                e.priority = Some("P0".into());
                e
            })
            .collect();
        let (first, _) = pass_at(&held, "2026-10-28T00:00:00Z", (0, false), None);
        let (stalled, _) = alarmed(&held, "2026-10-28T07:00:00Z", (0, false), Some(&first));
        let (fitted, body) = fit(stalled).unwrap();
        assert!(chars(&body) <= MAX_COMMENT_CHARS);
        let alarms: Vec<&str> = (fitted.holds.iter())
            .filter(|h| h.kind == HoldKind::Stalled)
            .filter_map(|h| h.item.as_deref())
            .collect();
        assert!(alarms.len() < MAX_STALLS, "{alarms:?}");
        assert_eq!(alarms.last(), Some(&"o/ops#1"), "{alarms:?}");
    }

    /// Past the comment's cap, the holds a stall alarm watches outlast the
    /// rest, and each alarm its hold, so that the stall clock goes on
    /// across truncated snapshots: P1 holds the pass is stuck on are left
    /// out after P0 ones that wait by design, their alarm is raised on time
    /// and stays, `since` the first pass, pass after pass.
    #[test]
    fn truncation_keeps_the_stall_clock() {
        let mut held: Vec<Entry> = (1..=40)
            .map(|n| {
                let reason = format!("plan {n} was dispatched already, {}", "r".repeat(2000));
                let mut e = entry(Scope::Item(item("o/w", n)), HoldKind::Dispatched, &reason);
                e.priority = Some("P0".into());
                e
            })
            .collect();
        for n in 1..=3 {
            let mut e = entry(Scope::Item(item("o/t", n)), HoldKind::Unreadable, "labels");
            e.priority = Some("P1".into());
            held.push(e);
        }
        let first = "2026-10-28T00:00:00Z";
        let mut previous: Option<Value> = None;
        for at in [
            first,
            "2026-10-28T03:00:00Z",
            "2026-10-28T07:00:00Z",
            "2026-10-28T08:00:00Z",
        ] {
            let (snapshot, _) = alarmed(&held, at, (0, false), previous.as_ref());
            let (fitted, body) = fit(snapshot).unwrap();
            assert!(fitted.truncated > 0, "{at}");
            let published = parse(&body).unwrap();
            let on = |kind: HoldKind| -> Vec<(String, String)> {
                (published["holds"].as_array().unwrap().iter())
                    .filter(|h| h["kind"] == kind.name())
                    .map(|h| (text(h, "item"), text(h, "since")))
                    .collect()
            };
            let stuck: Vec<(String, String)> = (1..=3)
                .map(|n| (format!("o/t#{n}"), first.into()))
                .collect();
            assert_eq!(on(HoldKind::Unreadable), stuck, "{at}");
            let alarms = if at < "2026-10-28T06:00:00Z" {
                vec![]
            } else {
                stuck
            };
            assert_eq!(on(HoldKind::Stalled), alarms, "{at}");
            previous = Some(published);
        }
    }
}
