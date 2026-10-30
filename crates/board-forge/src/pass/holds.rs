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
use crate::poll::validate_repository;
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
/// The most asks the list publishes; the rest are counted.
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
    /// Its parent is not a live epic; the operator's only where its
    /// Priority is among the board's first ([`Hold::operator`]).
    EpicNotLive,
    /// Outside every live epic, with no plan, under the triage scope
    /// `epics`: no triage run plans it.
    OutsideEpics,
    Overlap,
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
            Self::NeedsDesign => "needs-design",
            Self::Blocked => "blocked",
            Self::WaitingOn => "waiting-on",
            Self::PrOpen => "pr-open",
            Self::InFlight => "in-flight",
            Self::Dispatched => "dispatched",
            Self::TriageCooldown => "triage-cooldown",
            Self::EpicUnplanned => "epic-unplanned",
            Self::NeedsOperator => "needs-operator",
            Self::EpicNotLive => "epic-not-live",
            Self::OutsideEpics => "outside-epics",
            Self::Overlap => "overlap",
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
    /// (`epic-not-live`, `outside-epics`), or are no hold at all
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
        if let (HoldKind::OverlapIgnored, Some(item)) = (self.hold.kind, self.item()) {
            return format!("overlap ignored: {}: {reason}", item.display());
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
    /// When an applying pass last started a run, applied an epic plan or
    /// merged a pull request, as far as the published snapshots say: this
    /// snapshot's `at` where this pass did, or none before it said
    /// ([`carry`]).
    pub progress_at: String,
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
    format!(
        "https://{}/{}/issues/{}",
        item.host, item.repository, item.number
    )
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
    };
    let mut lines = Vec::new();
    let mut holds = Vec::with_capacity(snapshot.holds.len());
    let (mut raised, mut more) = (0, Vec::new());
    for hold in std::mem::take(&mut snapshot.holds) {
        let priority = (hold.priority.as_ref()).is_some_and(|p| watch.top.contains(p));
        let stalled = (hold.item.clone())
            .filter(|_| hold.kind.stalls() && !hold.operator && !hold.carried)
            .filter(|_| (hold.epic || priority) && over(&hold.since, &at, hold_hours));
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
}

/// The `source` of the asks each of [`Reading`]'s own sources lists.
pub const REVIEWS: &str = "reviews";
pub const PROPOSALS: &str = "proposals";
pub const DIGEST: &str = "digest";

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
}

/// The needs-you list ([`NEEDS_YOU_SCHEMA`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NeedsYou {
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
    /// The asks past [`MAX_NEEDS_YOU`] left out, the lowest ranked.
    pub omitted: usize,
    /// How many `items`: always their number.
    pub count: usize,
    pub items: Vec<Ask>,
}

/// What one of the needs-you list's own sources read this pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collected {
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
            asks: Vec::new(),
            unread: BTreeSet::new(),
        }
    }
}

/// A trusted bot summary's one-line question and recommendation
/// ([`summary`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub question: Option<String>,
    pub recommendation: Option<String>,
}

/// What [`needs_you`] knows of the pass besides its snapshot.
pub struct Reading<'a> {
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
    /// How much of the summaries was read, and each by its issue's key.
    pub summaries: (Read, BTreeMap<String, Summary>),
}

/// The bold lines of a bot summary [`summary`] reads.
const DECISION_NEEDED: &str = "Decision needed";
const RECOMMENDATION: &str = "Recommendation";

/// What a bot summary `body` asks and recommends, read as data: the rest
/// of its first `**Decision needed:**` and `**Recommendation:**` line (the
/// colon inside or outside the bold, the line perhaps a list item), one
/// line each. One that is empty is absent, so that the hold's own question
/// or reason stands.
pub fn summary(body: &str) -> Summary {
    let field = |label: &str| {
        let (inside, outside) = (format!("**{label}:**"), format!("**{label}**:"));
        body.lines()
            .map(|l| l.trim().strip_prefix("- ").unwrap_or(l.trim()))
            .find_map(|l| l.strip_prefix(&inside).or_else(|| l.strip_prefix(&outside)))
            .and_then(|rest| one_line(rest, MAX_ASK))
    };
    Summary {
        question: field(DECISION_NEEDED),
        recommendation: field(RECOMMENDATION),
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

/// The asks of the published list `previous` from the needs-you list's
/// own sources, where each is still well formed: its references valid,
/// its `target` made again from its action and its texts cut again.
fn published_asks(previous: Option<&Value>) -> Vec<Ask> {
    let list = (previous.filter(|s| s["schema"] == HOLDS_SCHEMA))
        .map(|s| &s["needs_you"])
        .filter(|n| n["schema"] == NEEDS_YOU_SCHEMA);
    let items = list
        .and_then(|n| n["items"].as_array())
        .into_iter()
        .flatten();
    items
        .filter_map(|a| serde_json::from_value::<Ask>(a.clone()).ok())
        .filter(|a| [REVIEWS, PROPOSALS, DIGEST].contains(&a.source.as_str()))
        .filter(|a| a.action.valid() && a.source_ref.valid())
        .filter(|a| parse_time(&a.waiting_since).is_some())
        .filter_map(|a| {
            Some(Ask {
                target: a.action.url(),
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

/// What `snapshot`, once [`alarm`] has added its alarms, has wait on the
/// operator, with `reading`'s own sources: each hold with `operator` true
/// on an item, but not on a closed one; so not the holds the pass releases
/// itself (a run in flight or rerun, a cooldown, a retry), nor
/// `needs-design` or board Blocked. A hold's ask is its issue's bot
/// summary's question where it has one, else its board Why's, else its
/// reason. A source's ask keeps the `waiting_since` published for the same
/// key, kind and head; where a source was not read whole, the asks
/// published from what it did not read are kept, marked `carried`. An
/// issue's hold on the operator's approval of its pull request and that
/// pull request's review wait are one ask, the review's with the hold's
/// Priority and question. Ranked by Priority in the board's order (an
/// unknown one after, none last), then the longest waiting, then key; a key
/// listed twice (the holds issue's two alarms) is listed once, where it
/// ranks first, its asks joined; past [`MAX_NEEDS_YOU`], the rest are
/// counted in `omitted`. `changed_at` carries over from `previous`, the
/// published snapshot's JSON, where the list was the same.
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
    };
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
        let said = reading.summaries.1.get(&key).cloned().unwrap_or_default();
        let asked = said.question.clone().or_else(|| h.question.clone());
        let action = Reference::new(RefKind::Issue, &repository, number);
        let ask = Ask {
            key,
            item,
            source: source.into(),
            kind: h.kind.name().into(),
            priority: h.priority.clone(),
            waiting_since: h.since.clone(),
            ask: asked.clone().unwrap_or_else(|| h.reason.clone()),
            recommendation: said.recommendation,
            target: h.url.clone().unwrap_or_else(|| action.url()),
            head: None,
            source_ref: Reference::new(RefKind::Hold, &repository, number),
            action,
            carried: false,
        };
        match approval_of(&h.reason) {
            Some(pull) => drop(approvals.insert(pull.to_ascii_lowercase(), (ask, asked.is_some()))),
            None => items.push(ask),
        }
    }
    let published = published_asks(previous);
    let own = [
        (REVIEWS, &reading.reviews),
        (PROPOSALS, &reading.proposals),
        (DIGEST, &reading.digest),
    ];
    for (_, collected) in own {
        for ask in &collected.asks {
            let mut ask = ask.clone();
            let seen = (published.iter())
                .find(|p| p.key == ask.key && p.kind == ask.kind && p.head == ask.head)
                .and_then(|p| not_after(&Value::from(p.waiting_since.as_str()), at));
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
    }
    items.extend(approvals.into_values().map(|(ask, _)| ask));
    for (source, collected) in own {
        if matches!(collected.read, Read::Complete | Read::Off) {
            continue;
        }
        for ask in published.iter().filter(|p| p.source == source) {
            let item = ask.item.to_ascii_lowercase();
            let repository = item.rsplit_once('#').map_or(item.as_str(), |(r, _)| r);
            let unread = collected.read == Read::Unread
                || collected.unread.contains(&item)
                || collected.unread.contains(repository);
            if unread && !items.iter().any(|a| a.key == ask.key) {
                items.push(Ask {
                    carried: true,
                    ..ask.clone()
                });
            }
        }
    }
    items.sort_by_key(|a| {
        let since = parse_time(&a.waiting_since);
        let known = (a.priority.as_ref()).and_then(|p| reading.options.iter().position(|o| o == p));
        let priority = (
            a.priority.is_none(),
            known.is_none(),
            known,
            a.priority.clone(),
        );
        (priority, since.is_none(), since, a.key.clone())
    });
    let mut first: BTreeMap<String, usize> = BTreeMap::new();
    let mut kept: Vec<Ask> = Vec::with_capacity(items.len());
    for ask in items {
        match first.get(&ask.key) {
            Some(&n) => kept[n].ask = format!("{}; {}", kept[n].ask, ask.ask),
            None => {
                first.insert(ask.key.clone(), kept.len());
                kept.push(ask);
            }
        }
    }
    for ask in &mut kept {
        ask.ask = one_line(&ask.ask, MAX_ASK).unwrap_or_default();
        ask.recommendation = (ask.recommendation.as_deref()).and_then(|r| one_line(r, MAX_ASK));
    }
    let omitted = kept.len().saturating_sub(MAX_NEEDS_YOU);
    kept.truncate(MAX_NEEDS_YOU);
    let items = kept;
    let read = [
        sources.epics,
        sources.items,
        sources.reviews,
        sources.proposals,
        sources.digest,
    ];
    let mut list = NeedsYou {
        schema: NEEDS_YOU_SCHEMA,
        complete: omitted == 0 && read.iter().all(|r| matches!(r, Read::Complete | Read::Off)),
        validated_at: at.clone(),
        changed_at: at.clone(),
        sources,
        summaries: reading.summaries.0,
        omitted,
        count: items.len(),
        items,
    };
    let previous = (previous.filter(|s| s["schema"] == HOLDS_SCHEMA))
        .map(|s| &s["needs_you"])
        .filter(|n| n["schema"] == NEEDS_YOU_SCHEMA);
    let content = |list: &Value| {
        let mut list = list.clone();
        if let Some(fields) = list.as_object_mut() {
            fields.remove("validated_at");
            fields.remove("changed_at");
        }
        list
    };
    let same = previous
        .filter(|p| serde_json::to_value(&list).is_ok_and(|now| content(p) == content(&now)));
    if let Some(since) = same.and_then(|p| not_after(&p["changed_at"], at)) {
        list.changed_at = since;
    }
    list
}

/// Whether `snapshot` says something other than the published comment
/// `body`, if any: its time alone is no change, nor the needs-you list's
/// `validated_at` until [`NEEDS_YOU_REFRESH_HOURS`] after the published one,
/// so that a list nothing changes is written again that often.
pub fn changed(body: Option<&str>, snapshot: &Snapshot) -> Result<bool> {
    let mut now = serde_json::to_value(snapshot)?;
    let validated = |s: &mut Value| {
        let list = s.get_mut("needs_you").and_then(Value::as_object_mut)?;
        list.remove("validated_at")?.as_str().map(str::to_owned)
    };
    let refresh = validated(&mut now);
    Ok(body.and_then(published).is_none_or(|mut s| {
        let last = validated(&mut s);
        let due = refresh.as_ref().is_some_and(|at| {
            let last = last.unwrap_or_default();
            parse_time(&last).is_none() || over(&last, at, NEEDS_YOU_REFRESH_HOURS)
        });
        s["at"] = Value::Null;
        now["at"] = Value::Null;
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
    Ok(format!(
        "{HOLDS_MARKER}\nHolds as of {}. Written by `agent-board pass`; the review app reads the JSON below.\n\n```json\n{{\"schema\":{},\"at\":{},\"truncated\":{},\"wip\":{},\"pass\":{}{leases}{needs_you},\"holds\":[{lines}\n]}}\n```\n",
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
/// in [`left_out_first`] order, then the stall alarms likewise, until it
/// fits, and counted in `truncated`, so that alarms never keep the holds
/// from being published. The operator's own holds and the holds of every
/// new run are always kept; past them, the needs-you list is left out
/// whole, so that its `validated_at` ages rather than the holds go
/// unpublished. If they alone do not fit, that is an error.
pub fn fit(snapshot: Snapshot) -> Result<(Snapshot, String)> {
    let holds = &snapshot.holds;
    let stalled = |n: usize| holds[n].kind == HoldKind::Stalled;
    let mut order: Vec<usize> = (0..holds.len())
        .filter(|&n| holds[n].item.is_some() && (!holds[n].operator || stalled(n)))
        .collect();
    order.sort_by_key(|&n| (stalled(n), left_out_first(&holds[n])));
    let mut order = order.into_iter();
    let mut dropped = BTreeSet::new();
    let mut needs_you = snapshot.needs_you.clone();
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
            needs_you: needs_you.clone(),
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
            let Some(n) = order.next() else {
                ensure!(
                    needs_you.take().is_some(),
                    "the hold snapshot is over {MAX_COMMENT_CHARS} characters with only the operator's holds and those of every new run left"
                );
                break;
            };
            let len = serde_json::to_string(&holds[n])?.chars().count() + 2;
            over = over.saturating_sub(len);
            dropped.insert(n);
        }
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
            progress_at: String::new(),
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
        Watch {
            stall,
            top: &TOP,
            on: &ON,
            log: Some("https://github.com/o/caller/actions/runs/9".into()),
            free,
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
            epics: true,
            considered: true,
            options,
            on,
            reviews: Collected::off(),
            proposals: Collected::off(),
            digest: Collected::off(),
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
        let reading = reading(&on, &options);
        let ranked: Vec<Option<String>> = (needs_you(&board, None, &reading).items.iter())
            .map(|a| a.priority.clone())
            .take(3)
            .collect();
        assert_eq!(
            ranked,
            [Some("Urgent"), Some("Low"), Some("Odd")].map(|p| p.map(Into::into))
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
        // `fit` keeps it; one that cannot fit beside the operator's holds is
        // left out whole, and the holds are published all the same.
        let (fitted, _) = fit(again).unwrap();
        assert_eq!(fitted.needs_you.unwrap().count, 6);
        let mut over = pass_at(waiting(), 30, published.as_ref());
        let list = over.needs_you.as_mut().unwrap();
        let mut long = list.items[0].clone();
        long.ask = "a".repeat(MAX_ASK);
        list.items = vec![long; 200];
        let (fitted, body) = fit(over).unwrap();
        assert!(chars(&body) <= MAX_COMMENT_CHARS);
        assert_eq!(fitted.needs_you, None);
        assert!(fitted.holds.iter().any(|h| h.operator));
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
        }
    }

    fn collected(read: Read, asks: Vec<Ask>, unread: &[&str]) -> Collected {
        let unread = unread.iter().map(|&u| u.to_owned()).collect();
        Collected { read, asks, unread }
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
        let list = own_listed(&[held], at, (none(), Collected::off()), None);
        let keys: Vec<&str> = list.items.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, ["o/t#7"]);
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

    /// Past [`MAX_NEEDS_YOU`] asks the rest are counted, not listed, and
    /// the list is not complete; each ask and recommendation is one line
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
            (MAX_NEEDS_YOU, MAX_NEEDS_YOU, 7)
        );
        assert!(!list.complete);
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
            ("Decision needed: Ship?\n*Recommendation:* Yes.", None, None),
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
        let snapshot = snapshot(&[asked, plain], "2026-10-28T09:00:00Z".into(), tally());
        let (on, options) = (item("o/ops", 1), options(&["P0"]));
        let said = |question: Option<&str>| Summary {
            question: question.map(Into::into),
            recommendation: Some("Do it.".into()),
        };
        for (question, expected) in [
            (Some("Ship 1.0?"), ["Ship 1.0?", "Ship 1.0?"]),
            (None, ["Which target?", "labelled needs-human"]),
        ] {
            let summaries = [("o/t#1", said(question)), ("o/t#2", said(question))];
            let reading = Reading {
                summaries: (
                    Read::Partial,
                    summaries.into_iter().map(|(k, s)| (k.into(), s)).collect(),
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
    }
}
