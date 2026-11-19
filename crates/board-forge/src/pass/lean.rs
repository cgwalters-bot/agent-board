//! `leanReads` (agent-board#1110): a pass spends fewer requests on what it
//! read already, so that scheduling reaches work it can start.
//!
//! - Stage 4 holds a candidate again without reading its comments and plan
//!   where the last pass held it on them (a triage cooldown) or on its plan's
//!   repository being at `openPrCap`, and the board's facts of it (its
//!   `updatedAt`, labels, Status, Priority, parent and closing pull requests)
//!   are unchanged since ([`Carried`]). A comment, edit or label moves
//!   `updatedAt`. The cap is asked again of the repository's count; what
//!   the board does not show, such as a pull request merging without closing
//!   the issue, waits at most [`CARRY_SECONDS`] for the next whole read.
//!   Such a hold says what it reused ([`Reused`]). Not in an upstream fork,
//!   whose upstream completing is not on the board.
//! - Stage 4 names the repositories at `openPrCap`.
//!
//! The carried holds come from the trusted state file, as its cached
//! responses do. One forged can only hold a candidate, never start one, and
//! for at most [`CARRY_SECONDS`]; an open-pr-cap one only while its
//! repository's count still reads at the cap.
use super::*;

/// How long a carried hold stands without its candidate being read whole.
pub const CARRY_SECONDS: i64 = poll::quiet::QUIET_REREAD_SECONDS;

/// The most holds the state carries: a board's whole read ([`BOARD_PAGES`]).
pub const CARRY_LIMIT: usize = BOARD_PAGES * 100;

/// The longest reason a carried hold keeps; a longer one is read again.
const REASON_LIMIT: usize = 1000;

/// What a hold carried by [`Scheduler::lean_carried`] took from an earlier
/// pass's read rather than this one's ([`Hold::reused`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reused {
    /// The whole decision: a triage cooldown as it was read.
    Hold,
    /// The candidate's accepted implement plan; its repository's open pull
    /// request count is this pass's.
    Plan,
}

/// The hold kinds carried: decided from the candidate's comments and plan,
/// which move its `updatedAt`, and the time.
const CARRIED: [HoldKind; 2] = [HoldKind::TriageCooldown, HoldKind::OpenPrCap];

/// A hold of a candidate stage 4 read whole, held again while the board's
/// facts of it are these.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Carried {
    /// The digest of its board facts at that read ([`digest`]).
    pub facts: String,
    /// When it is read whole again: its cooldown's end, at most
    /// [`CARRY_SECONDS`] after the read.
    pub until: String,
    pub kind: HoldKind,
    /// A cooldown's reason, as the read gave it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// An open-pr-cap hold's plan's work repository, whose count is asked
    /// again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
}

impl Carried {
    fn check(&self) -> Result<()> {
        ensure!(
            self.facts.len() == 64 && self.facts.bytes().all(|b| b.is_ascii_hexdigit()),
            "facts digest {:?}",
            self.facts
        );
        ensure!(
            OffsetDateTime::parse(&self.until, &Rfc3339).is_ok(),
            "until {:?}",
            self.until
        );
        match (self.kind, &self.reason, &self.repository) {
            (HoldKind::TriageCooldown, Some(reason), None) => ensure!(
                !reason.is_empty() && reason.len() <= REASON_LIMIT,
                "reason of {} bytes",
                reason.len()
            ),
            (HoldKind::OpenPrCap, None, Some(repo)) => validate_repository(repo)?,
            (kind, ..) => bail!(
                "{} hold with a reason or repository it does not have",
                kind.name()
            ),
        }
        Ok(())
    }

    fn until(&self) -> Option<OffsetDateTime> {
        OffsetDateTime::parse(&self.until, &Rfc3339).ok()
    }
}

/// The state's `carried_holds`, refused whole where an entry is malformed
/// or there are more than [`CARRY_LIMIT`].
pub(crate) fn from_json(value: Option<&Value>) -> Result<BTreeMap<String, Carried>> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let carried: BTreeMap<String, Carried> = serde_json::from_value(value.clone())
        .context("poll state carried_holds is not an issue-to-hold map")?;
    ensure!(
        carried.len() <= CARRY_LIMIT,
        "poll state carried_holds has more than {CARRY_LIMIT} entries"
    );
    for (at, entry) in &carried {
        ensure!(item(at), "poll state carried_holds item {at:?}");
        entry
            .check()
            .with_context(|| format!("poll state carried_holds {at}"))?;
    }
    Ok(carried)
}

/// Whether `at` is a carried hold's key: a github.com issue's [`key`].
fn item(at: &str) -> bool {
    at.split_once('#').is_some_and(|(repo, number)| {
        validate_repository(repo).is_ok()
            && *at == at.to_ascii_lowercase()
            && number.parse::<u64>().is_ok_and(|n| n > 0)
    })
}

/// The digest of a candidate's board facts, which a carried hold is held
/// again under.
fn digest(facts: &Facts) -> String {
    format!("{:x}", Sha256::digest(format!("{facts:?}")))
}

/// What a decision site said of the hold it returns, which the candidate's
/// step must be for it to be carried.
struct Pending {
    entry: Carried,
    reason: String,
}

/// Stage 4's carried holds: the last pass's, and what the next pass gets.
#[derive(Default)]
pub(super) struct Lean {
    previous: BTreeMap<String, Carried>,
    next: BTreeMap<String, Carried>,
    /// The candidate considered, its key and [`digest`].
    candidate: Option<(String, String)>,
    pending: Option<Pending>,
    carried: usize,
}

impl Lean {
    /// The last pass's carried holds still standing at `now`, none past
    /// [`CARRY_SECONDS`] from it, whatever the state says.
    pub(super) fn new(previous: BTreeMap<String, Carried>, now: OffsetDateTime) -> Self {
        let bound = now + time::Duration::seconds(CARRY_SECONDS);
        let previous: BTreeMap<_, _> = (previous.into_iter())
            .filter(|(_, c)| c.until().is_some_and(|until| now < until && until <= bound))
            .collect();
        Self {
            next: previous.clone(),
            previous,
            ..Self::default()
        }
    }
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    /// Before `facts` is considered: it may be held on a carried hold, and
    /// its hold carried. Not where what decides its hold is read beyond its
    /// comments: failed runs ([`Config::reruns`], [`Config::starts`]),
    /// active work ([`Config::skip_active_work`]) or, in an upstream fork,
    /// its upstream's completion ([`Self::upstream_replan`]).
    pub(super) fn lean_begin(&self, facts: &Facts) {
        let config = self.config;
        let upstream = self.upstream_fork(&facts.issue.repository);
        let mut lean = self.lean.borrow_mut();
        lean.pending = None;
        lean.candidate = None;
        if !config.lean_reads
            || facts.updated.is_none()
            || config.reruns
            || config.starts.is_some()
            || config.skip_active_work
            || upstream
        {
            return;
        }
        let at = key(&facts.issue);
        lean.next.remove(&at);
        // Only what the state reads back: an issue on another host is not.
        if item(&at) {
            lean.candidate = Some((at, digest(facts)));
        }
    }

    /// After the candidate's `step`: its hold carried to the next pass where
    /// a decision site said so, or the carried one it was held on; a stop
    /// that decided nothing leaves the last pass's.
    pub(super) fn lean_end(&self, step: &Result<Step>) {
        let mut lean = self.lean.borrow_mut();
        let pending = lean.pending.take();
        let Some((at, digest)) = lean.candidate.take() else {
            return;
        };
        let entry = match (step, pending) {
            (Ok(Step::Stop(..)), _) => lean.previous.get(&at).cloned(),
            (Ok(Step::Hold(hold)), Some(Pending { mut entry, reason })) => {
                let read = Hold {
                    reused: None,
                    ..hold.clone()
                };
                (hold.kind == entry.kind && read == Hold::new(entry.kind, reason)).then(|| {
                    entry.facts = digest;
                    entry
                })
            }
            _ => None,
        };
        // Past the state's bound the candidate is read whole next pass.
        if let Some(entry) = entry.filter(|e| e.check().is_ok())
            && lean.next.len() < CARRY_LIMIT
        {
            lean.next.insert(at, entry);
        }
    }

    /// Where a decision site returns `reason`, a hold of `kind` that stands
    /// until `until` (or [`CARRY_SECONDS`]) unless the board's facts change.
    pub(super) fn lean_carry(
        &self,
        kind: HoldKind,
        reason: &str,
        (until, repository): (Option<OffsetDateTime>, Option<&str>),
    ) {
        let mut lean = self.lean.borrow_mut();
        if lean.candidate.is_none() || !CARRIED.contains(&kind) {
            return;
        }
        let bound = self.now + time::Duration::seconds(CARRY_SECONDS);
        let until = until.map_or(bound, |until| until.min(bound));
        let Ok(until) = until.format(&Rfc3339) else {
            return;
        };
        let entry = Carried {
            facts: String::new(),
            until,
            kind,
            reason: (kind == HoldKind::TriageCooldown).then(|| reason.to_owned()),
            repository: repository.map(str::to_owned),
        };
        let reason = reason.to_owned();
        lean.pending = Some(Pending { entry, reason });
    }

    /// The carried hold of the candidate considered, where the board's facts
    /// of it are those it was read with: a cooldown as it was, an open-pr-cap
    /// one where its repository's count still holds it.
    pub(super) fn lean_carried(&self, facts: &Facts, board: &BoardRead) -> Option<Step> {
        let entry = {
            let lean = self.lean.borrow();
            let (at, digest) = lean.candidate.as_ref()?;
            lean.previous
                .get(at)
                .filter(|c| c.facts == *digest)?
                .clone()
        };
        let hold = match (entry.kind, &entry.reason, &entry.repository) {
            (HoldKind::TriageCooldown, Some(reason), _) => {
                Hold::new(entry.kind, reason.clone()).reused(Reused::Hold)
            }
            // Its plan's gates as when it was read: the state names only
            // where the plan said, and the configuration may since differ.
            (HoldKind::OpenPrCap, _, Some(repository))
                if self.config.works_in(repository) && self.config.writes_in(repository) =>
            {
                let past = self.past_cap_of(facts, board, None);
                match self.pr_pressure((repository, "implement"), past.as_deref()) {
                    pressure::Pressure::Held(held) if held.kind == HoldKind::OpenPrCap => {
                        // As its plan's acceptance said, unread this time.
                        (self.report.accepted_plans.borrow_mut())
                            .insert(facts.issue.display(), "implement".into());
                        held.reused(Reused::Plan)
                    }
                    _ => return None,
                }
            }
            _ => return None,
        };
        let mut lean = self.lean.borrow_mut();
        lean.carried += 1;
        lean.pending = Some(Pending {
            reason: hold.reason.clone(),
            entry,
        });
        Some(Step::Hold(hold))
    }

    /// After stage 4's candidates: the holds the next pass carries, and the
    /// lines saying how many were carried and which repositories are at
    /// `openPrCap`.
    pub(super) fn lean_report(&mut self) {
        if !self.config.lean_reads {
            return;
        }
        let (next, carried) = {
            let lean = self.lean.borrow();
            (lean.next.clone(), lean.carried)
        };
        self.report.carried_holds = Some(next);
        if carried > 0 {
            self.line(format!(
                "schedule: {carried} hold(s) carried from an earlier read, their board facts unchanged since"
            ));
        }
        let Some(cap) = self.config.open_pr_cap else {
            return;
        };
        let at: Vec<String> = (self.report.open_pr_counts.borrow().iter())
            .filter_map(|(repo, count)| {
                count
                    .filter(|c| *c >= cap)
                    .map(|c| format!("{repo} {c}/{cap}"))
            })
            .collect();
        if !at.is_empty() {
            self.line(format!(
                "open-pr-cap: at the cap: {}; their implement plans wait for the bot's pull requests to land or close",
                at.join(", ")
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A carried hold stands until its `until`, and never past
    /// [`CARRY_SECONDS`] from the pass's clock, whatever the state says.
    #[test]
    fn carried_holds_stand_at_most_their_bound() {
        let now = OffsetDateTime::parse("2026-11-19T12:00:00Z", &Rfc3339).unwrap();
        let entry = |seconds: i64| Carried {
            facts: "a".repeat(64),
            until: (now + time::Duration::seconds(seconds))
                .format(&Rfc3339)
                .unwrap(),
            kind: HoldKind::TriageCooldown,
            reason: Some("triaged again from then".into()),
            repository: None,
        };
        for (seconds, stands) in [
            (-1, false),
            (0, false),
            (1, true),
            (CARRY_SECONDS, true),
            (CARRY_SECONDS + 1, false),
            (365 * 24 * 3600, false),
        ] {
            let previous = BTreeMap::from([("o/r#1".to_owned(), entry(seconds))]);
            let lean = Lean::new(previous, now);
            assert_eq!(lean.previous.len(), usize::from(stands), "{seconds}");
            assert_eq!(lean.next, lean.previous, "{seconds}");
        }
    }
}
