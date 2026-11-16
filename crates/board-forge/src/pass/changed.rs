//! `changeSweepHours` (docs/pass.md#change-driven-pull-request-walk): stage
//! 0 reads the changes diff ([`crate::changes::diff`]) of the followed and
//! midstream repositories since the watermarks the store keeps, and stage 2
//! visits only the pull requests it lists, rather than every open one
//! again (agent-board#1050). Every pull request is still visited at least
//! `changeSweepHours` apart, by a full walk whose time the trusted `state`
//! keeps.
//!
//! The concrete failure it stops is the walk spending the pass's requests
//! on pull requests nothing has moved. What does not move `updated_at`
//! waits for the next full walk: a timer of its own (a stale draft, a run
//! that ended without a push), or a base that moved under it. Check
//! results the diff follows itself, as pending checks.
//!
//! The store comes from an Actions cache that other default-branch jobs
//! can write. A forged watermark can only leave a pull request unvisited
//! until the next full walk; one ahead of the clock is read as none, and
//! what a forged state could make the read spend is bounded as a real
//! state's is ([`sane`]). A repository without a committed watermark is
//! walked whole, and a watermark moves only where stage 2 walked the
//! repository to its end and held no step there.
use super::*;
use crate::changes::diff::{self, Diff, Followed, PENDING_LIMIT, Scope};
use crate::changes::parse_time;

/// The longest `changeSweepHours`: a week.
pub const CHANGE_SWEEP_HOURS_LIMIT: u64 = 7 * 24;

/// The most numbers a list keeps for a 304: it keeps an ETag only for a
/// list one page short of a full page of 100.
const VALIDATED_NUMBERS: usize = 99;

/// What stage 0 read, for stage 2 to walk and then commit.
pub(super) struct Read {
    /// A diff for each repository read; one that could not be read has none.
    diffs: Vec<Diff>,
    /// The state after the reads, its watermarks not yet moved.
    state: diff::State,
    /// The pull request numbers of each repository stage 2 may narrow its
    /// walk to; empty on a full walk.
    changed: BTreeMap<String, BTreeSet<u64>>,
    /// Whether this pass walks every pull request.
    sweep: bool,
    /// The last full walk's time, kept where this one is not one.
    swept: Option<String>,
}

pub(super) fn check(config: &Config) -> Result<()> {
    let hours = config.change_sweep_hours;
    ensure!(
        hours <= CHANGE_SWEEP_HOURS_LIMIT,
        "changeSweepHours is not 0 to {CHANGE_SWEEP_HOURS_LIMIT}"
    );
    if hours == 0 {
        return Ok(());
    }
    ensure!(
        config.forgejo_board().is_none(),
        "changeSweepHours supports only GitHub"
    );
    ensure!(
        config.store.is_some() && config.state.is_some(),
        "changeSweepHours needs store and state"
    );
    Ok(())
}

/// `state` without what no pass could have written for `scope`, the
/// lower-case repositories read, `hours` the `changeSweepHours`: a
/// watermark or first window ahead of `now`, or one that is no time, which
/// the read would refuse; one older than a first window and a full walk's
/// interval, which a pass walking the repository to its end would have
/// moved, and which would list everything since; numbers kept for a 304
/// past what a list with an ETag holds; and pending checks of other
/// repositories, malformed, or past [`PENDING_LIMIT`]. So a forged state
/// makes the read spend no more than a fresh one does, and the repository
/// is walked whole.
fn sane(
    mut state: diff::State,
    scope: &BTreeSet<String>,
    (now, hours): (OffsetDateTime, u64),
) -> diff::State {
    let oldest = now - diff::FIRST_WINDOW - time::Duration::hours(hours as i64);
    let past = |at: &Option<String>| {
        at.as_deref()
            .is_some_and(|at| parse_time(at).is_ok_and(|at| (oldest..=now).contains(&at)))
    };
    state.repositories.retain(|repo, _| scope.contains(repo));
    for mark in state.repositories.values_mut() {
        if !past(&mark.since) {
            mark.since = None;
        }
        if !past(&mark.start) {
            mark.start = None;
        }
        if mark.numbers.len() > VALIDATED_NUMBERS {
            (mark.url, mark.etag, mark.numbers) = (None, None, Vec::new());
        }
    }
    let pending = std::mem::take(&mut state.pending);
    state.pending = (pending.into_iter())
        .filter(|reference| {
            reference.rsplit_once('#').is_some_and(|(repo, number)| {
                scope.contains(repo) && number.parse::<u64>().is_ok_and(|n| n > 0)
            })
        })
        .take(PENDING_LIMIT)
        .collect();
    state
}

/// Whether a full walk is due at `now`, the last one at `swept`, `hours`
/// apart: one never made, ahead of the clock or unreadable is due.
fn sweep_due(swept: Option<&str>, hours: u64, now: OffsetDateTime) -> bool {
    let hours = time::Duration::hours(hours as i64);
    swept
        .and_then(|at| parse_time(at).ok())
        .is_none_or(|at| at > now || now - at >= hours)
}

/// The item numbers `diffs` list for each repository whose watermark
/// `state` had committed before the read; a repository read from its first
/// window is walked whole, as nothing says what changed before it.
fn changed(diffs: &[Diff], state: &diff::State) -> BTreeMap<String, BTreeSet<u64>> {
    let mut changed = BTreeMap::new();
    for repository in diffs.iter().flat_map(|d| &d.repositories) {
        let key = repository.repository.to_ascii_lowercase();
        if state
            .repositories
            .get(&key)
            .is_none_or(|mark| mark.since.is_none())
        {
            continue;
        }
        let numbers = (repository.items.iter())
            .filter_map(|item| item.reference.rsplit_once('#')?.1.parse().ok())
            .collect();
        changed.insert(key, numbers);
    }
    changed
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    /// Stage 0: the changes diff since the store's watermarks, `state`'s
    /// (the caller's read of the store), where `changeSweepHours` is set. A
    /// repository whose diff cannot be read is walked whole.
    pub(super) fn changes(&mut self, state: &mut State) {
        let hours = self.config.change_sweep_hours;
        let stored = state.changes.take();
        if hours == 0 {
            return;
        }
        let swept = state.changes_swept.clone();
        let sweep = sweep_due(swept.as_deref(), hours, self.now);
        let (diffs, watermarks) = self.read_diffs(stored.unwrap_or_default());
        let changed = match sweep {
            true => BTreeMap::new(),
            false => self::changed(&diffs, &watermarks),
        };
        let read = diffs.iter().map(|d| d.repositories.len()).sum::<usize>();
        let walk = match (&swept, sweep) {
            (Some(at), false) => format!(
                "{} of {read} repositories narrowed to what changed; the last full walk at {at}",
                changed.len()
            ),
            (Some(at), true) => format!("a full walk, the last at {at}"),
            (None, _) => "a full walk, none recorded before".into(),
        };
        let items: usize = (diffs.iter().flat_map(|d| &d.repositories))
            .map(|r| r.items.len())
            .sum();
        let requests: usize = diffs.iter().map(|d| d.requests).sum();
        let revalidated: usize = diffs.iter().map(|d| d.revalidated).sum();
        self.line(format!(
            "changes: {items} item(s) changed in {read} repositories since their watermarks, {requests} request(s), {revalidated} revalidated; {walk}"
        ));
        self.report.records.borrow_mut().changes = Some(watermarks.clone());
        self.changes = Some(Read {
            diffs,
            state: watermarks,
            changed,
            sweep,
            swept,
        });
    }

    /// The diff of each repository stage 2 may narrow since `stored`, and
    /// the state the reads leave: `follow` and the midstream forks, and
    /// under `pr.actionFirst` only those in our own space, which its walk
    /// narrows. A repository that fails its read loses its watermark, so
    /// that the next pass reads it from a first window.
    fn read_diffs(&mut self, stored: diff::State) -> (Vec<Diff>, diff::State) {
        let config = self.config;
        let midstreams = match &config.midstream {
            None => Vec::new(),
            Some(midstream) => {
                let read = match config.resolved_midstreams.clone() {
                    Some(read) => Ok(read),
                    None => midstream.read(&self.client),
                };
                match read {
                    Ok(read) => {
                        // Stage 2 follows the same, read once.
                        self.midstreams = Some(read.clone());
                        read
                    }
                    Err(error) => {
                        self.line(format!(
                            "changes: midstream topic {} unread, so its forks are walked whole: {error:#}",
                            midstream.topic
                        ));
                        Vec::new()
                    }
                }
            }
        };
        let mut seen = BTreeSet::new();
        let repositories: Vec<Followed> = (config.follow.iter().map(|r| (r, false)))
            .chain(midstreams.iter().map(|r| (r, true)))
            .filter(|(r, _)| {
                !config.pr.poll.action_first
                    || poll::our_space(&config.dispatch_repo, &config.bot, r)
            })
            .filter(|(r, _)| seen.insert(r.to_ascii_lowercase()))
            .map(|(name, midstream)| Followed {
                name: name.clone(),
                midstream,
            })
            .collect();
        let hours = config.change_sweep_hours;
        let mut state = sane(stored, &seen, (self.now, hours));
        let mut diffs = Vec::new();
        for followed in repositories {
            let name = followed.name.clone();
            let scope = Scope {
                repositories: vec![followed],
                operator: config.operator.clone(),
            };
            match diff::read(&self.client, &scope, &mut state, self.now) {
                Ok(diff) => diffs.push(diff),
                Err(error) if budget_spent(&error) => {
                    self.line(format!(
                        "changes: the stage's request share is spent at {name}; it and the repositories after it are walked whole: {error:#}"
                    ));
                    break;
                }
                Err(error) => {
                    self.line(format!(
                        "changes: {name} unread, so it is walked whole: {error:#}"
                    ));
                    state.repositories.remove(&name.to_ascii_lowercase());
                }
            }
        }
        // Each read caps only its own repository's.
        state.pending = std::mem::take(&mut state.pending)
            .into_iter()
            .take(PENDING_LIMIT)
            .collect();
        (diffs, state)
    }

    /// The pull requests stage 2's walk may narrow to, by repository.
    pub(super) fn changed(&self) -> BTreeMap<String, BTreeSet<u64>> {
        self.changes
            .as_ref()
            .map(|read| read.changed.clone())
            .unwrap_or_default()
    }

    /// After stage 2 walked to `outcome`: the watermarks of the repositories
    /// it walked to their end move, and a full walk of every one is the
    /// last; the repositories it narrowed count as read in part.
    pub(super) fn changes_walked(&mut self, outcome: &mut poll::Outcome) {
        let Some(changes) = &mut self.changes else {
            return;
        };
        let applied = self.apply.then_some(self.now);
        let watermarks = walked(changes, outcome, applied);
        self.report.records.borrow_mut().changes = Some(watermarks);
    }
}

/// [`Scheduler::changes_walked`] of `changes`, at `applied` where the pass
/// applies: the watermarks after it. A repository is consumed where stage 2
/// walked it to its end and held no step there, which a later pass would
/// otherwise not visit again until the next full walk.
fn walked(
    changes: &mut Read,
    outcome: &mut poll::Outcome,
    applied: Option<OffsetDateTime>,
) -> diff::State {
    // A pull request already labelled for the supervisor waits on it as a
    // skip does; one whose label write was refused is held.
    let labelled = format!("labelled {}", crate::stop::SUPERVISOR_LABEL);
    let waits = |s: &poll::Stepped| {
        matches!(s.action, "wait" | "skip")
            || (s.action == crate::stop::SUPERVISOR_LABEL && s.reason == labelled)
    };
    let held: BTreeSet<String> = (outcome.steps.iter())
        .filter(|s| !s.ran && !waits(s))
        .map(|s| s.repository.to_ascii_lowercase())
        .collect();
    let undecided = &outcome.undecided;
    let consumed = |repo: &str| !undecided.contains(repo) && !held.contains(repo);
    for diff in &changes.diffs {
        diff.commit_where(&mut changes.state, consumed);
    }
    // A full walk ends where every repository a later pass may narrow, or
    // the diff read, was walked to its end; a held step there is listed
    // again by the next diff, its watermark kept.
    let complete = changes.sweep
        && !changes.diffs.is_empty()
        && (changes.diffs.iter().flat_map(|d| &d.repositories))
            .map(|r| r.repository.to_ascii_lowercase())
            .chain(changes.state.repositories.keys().cloned())
            .all(|repo| !undecided.contains(&repo));
    outcome.state.changes_swept = match applied.filter(|_| complete) {
        Some(now) => now.format(&Rfc3339).ok(),
        None => changes.swept.clone(),
    };
    // What it left unvisited may wait on the operator as it did.
    (outcome.undecided).extend(outcome.unchanged.keys().cloned());
    changes.state.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> OffsetDateTime {
        parse_time(text).unwrap()
    }

    #[test]
    fn sweeps() {
        let now = at("2027-01-15T12:00:00Z");
        for (swept, due) in [
            (None, true),
            (Some("not a time"), true),
            (Some("2027-01-15T13:00:00Z"), true),
            (Some("2027-01-15T06:00:00Z"), true),
            (Some("2027-01-15T06:00:01Z"), false),
            (Some("2027-01-15T12:00:00Z"), false),
        ] {
            assert_eq!(sweep_due(swept, 6, now), due, "{swept:?}");
        }
    }

    /// What no pass could have written is dropped: a watermark or first
    /// window ahead of the clock or no time, so that the read starts from
    /// the first window and walks it whole; numbers past a validated list's;
    /// repositories and pending checks out of scope, malformed or too many.
    #[test]
    fn sane_state() {
        let now = at("2027-01-15T12:00:00Z");
        let mut state = diff::State::default();
        for (repo, since, start, numbers) in [
            ("o/past", Some("2027-01-15T11:00:00Z"), None, 99),
            ("o/future", Some("2027-01-16T00:00:00Z"), None, 100),
            (
                "o/garbage",
                Some("yesterday"),
                Some("2027-01-14T12:00:00Z"),
                0,
            ),
            ("o/start", None, Some("2027-01-20T00:00:00Z"), 0),
            ("o/other", Some("2027-01-15T11:00:00Z"), None, 0),
            // Older than a first window and the six hours between walks.
            (
                "o/ancient",
                Some("2027-01-14T05:59:59Z"),
                Some("2000-01-01T00:00:00Z"),
                0,
            ),
            ("o/oldest", Some("2027-01-14T06:00:00Z"), None, 0),
        ] {
            let mark = diff::Watermark {
                since: since.map(Into::into),
                start: start.map(Into::into),
                etag: Some("\"e\"".into()),
                numbers: (1..=numbers).collect(),
                ..Default::default()
            };
            state.repositories.insert(repo.into(), mark);
        }
        state.pending = (1..=PENDING_LIMIT + 5)
            .map(|n| format!("o/past#{n}"))
            .chain(["o/other#1".into(), "o/past#x".into(), "o/past#0".into()])
            .collect();
        let scope = [
            "o/past",
            "o/future",
            "o/garbage",
            "o/start",
            "o/ancient",
            "o/oldest",
        ]
        .map(String::from)
        .into();
        let sane = sane(state, &scope, (now, 6));
        let marks: Vec<_> = (sane.repositories.into_iter())
            .map(|(repo, mark)| (repo, mark.since, mark.start, mark.numbers.len(), mark.etag))
            .collect();
        let some = |at: &str| Some(at.to_owned());
        let etag = some("\"e\"");
        assert_eq!(
            marks,
            [
                ("o/ancient".into(), None, None, 0, etag.clone()),
                ("o/future".into(), None, None, 0, None),
                (
                    "o/garbage".into(),
                    None,
                    some("2027-01-14T12:00:00Z"),
                    0,
                    etag.clone()
                ),
                (
                    "o/oldest".into(),
                    some("2027-01-14T06:00:00Z"),
                    None,
                    0,
                    etag.clone()
                ),
                (
                    "o/past".into(),
                    some("2027-01-15T11:00:00Z"),
                    None,
                    99,
                    etag.clone()
                ),
                ("o/start".into(), None, None, 0, etag),
            ]
        );
        assert_eq!(sane.pending.len(), PENDING_LIMIT);
        assert!(sane.pending.iter().all(|r| r.starts_with("o/past#")));
    }

    /// A diff of `o/a`, which lists #5, and `o/b`, which lists nothing,
    /// read at `NOW` past `o/a`'s committed watermark and from `o/b`'s first
    /// window.
    fn read(sweep: bool) -> Read {
        let repository = |name: &str, items: Vec<diff::Item>| Diff {
            schema: diff::SCHEMA,
            now: NOW.into(),
            repositories: vec![diff::RepositoryDiff {
                repository: name.into(),
                midstream: false,
                since: "2027-01-15T07:55:00Z".into(),
                items,
            }],
            requests: 2,
            revalidated: 1,
        };
        let five = diff::Item {
            reference: "o/a#5".into(),
            ..Default::default()
        };
        let diffs = vec![repository("o/a", vec![five]), repository("o/b", vec![])];
        let mut state = diff::State::default();
        let mark = diff::Watermark {
            since: Some("2027-01-15T08:00:00Z".into()),
            ..Default::default()
        };
        state.repositories.insert("o/a".into(), mark);
        let first = diff::Watermark {
            start: Some("2027-01-14T12:00:00Z".into()),
            ..Default::default()
        };
        state.repositories.insert("o/b".into(), first);
        Read {
            changed: if sweep {
                BTreeMap::new()
            } else {
                changed(&diffs, &state)
            },
            diffs,
            state,
            sweep,
            swept: Some("2027-01-15T06:00:00Z".into()),
        }
    }

    const NOW: &str = "2027-01-15T12:00:00Z";

    /// Only a repository with a committed watermark is narrowed, to the
    /// numbers the diff lists.
    #[test]
    fn narrows_committed_repositories() {
        let expected: BTreeMap<String, BTreeSet<u64>> = [("o/a".into(), [5].into())].into();
        assert_eq!(read(false).changed, expected);
    }

    /// The watermarks move where the walk ended and held no step, and a
    /// full walk of the diff's repositories to their end in an applying
    /// pass is the last, whatever stage 2 left of others; a narrowed
    /// repository reads as in part for the hold snapshot.
    #[test]
    fn walks_commit() {
        let since = |state: &diff::State, repo: &str| state.repositories[repo].since.clone();
        let now = Some(NOW.to_owned());
        let last = Some("2027-01-15T06:00:00Z".to_owned());
        let step = |action: &'static str| poll::Stepped {
            repository: "O/A".into(),
            number: 5,
            head: "abc".into(),
            action,
            reason: "why".into(),
            ran: false,
            moved_at: None,
            waiting: poll::WaitReason::Person,
        };
        type Case<'a> = (
            &'a str,
            bool,
            Vec<&'a str>,
            Vec<&'a str>,
            Vec<poll::Stepped>,
            bool,
            &'a Option<String>,
            &'a Option<String>,
            &'a Option<String>,
        );
        let cases: Vec<Case> = vec![
            (
                "narrowed",
                false,
                vec![],
                vec!["o/a"],
                vec![],
                true,
                &now,
                &now,
                &last,
            ),
            (
                "cut at o/b",
                false,
                vec!["o/b"],
                vec![],
                vec![],
                true,
                &now,
                &None,
                &last,
            ),
            (
                "held fix",
                false,
                vec![],
                vec![],
                vec![step("fix")],
                true,
                &None,
                &now,
                &last,
            ),
            (
                "a wait",
                false,
                vec![],
                vec![],
                vec![step("wait")],
                true,
                &now,
                &now,
                &last,
            ),
            (
                "a supervisor hold",
                false,
                vec![],
                vec![],
                vec![poll::Stepped {
                    reason: format!("labelled {}", crate::stop::SUPERVISOR_LABEL),
                    ..step(crate::stop::SUPERVISOR_LABEL)
                }],
                true,
                &now,
                &now,
                &last,
            ),
            (
                "a supervisor label refused",
                false,
                vec![],
                vec![],
                vec![step(crate::stop::SUPERVISOR_LABEL)],
                true,
                &None,
                &now,
                &last,
            ),
            (
                "swept",
                true,
                vec![],
                vec![],
                vec![],
                true,
                &now,
                &now,
                &now,
            ),
            (
                "swept, a fix held",
                true,
                vec![],
                vec![],
                vec![step("fix")],
                true,
                &None,
                &now,
                &now,
            ),
            (
                "swept, other cut",
                true,
                vec!["o/x"],
                vec![],
                vec![],
                true,
                &now,
                &now,
                &now,
            ),
            (
                "sweep cut",
                true,
                vec!["o/a"],
                vec![],
                vec![],
                true,
                &None,
                &now,
                &last,
            ),
            (
                "swept in preview",
                true,
                vec![],
                vec![],
                vec![],
                false,
                &now,
                &now,
                &last,
            ),
        ];
        for (name, sweep, undecided, unchanged, steps, applied, a, b, swept) in cases {
            let mut changes = read(sweep);
            let mut outcome = poll::Outcome::default();
            outcome.undecided = undecided.iter().map(|r| r.to_string()).collect();
            outcome.unchanged = unchanged.iter().map(|r| (r.to_string(), 2)).collect();
            outcome.steps = steps;
            let applied = applied.then(|| at(NOW));
            let state = walked(&mut changes, &mut outcome, applied);
            let a_since = since(&state, "o/a");
            let a_expected = a.clone().or(Some("2027-01-15T08:00:00Z".into()));
            assert_eq!(a_since, a_expected, "{name}");
            assert_eq!(&since(&state, "o/b"), b, "{name}");
            assert_eq!(&outcome.state.changes_swept, swept, "{name}");
            let partial: BTreeSet<String> = (undecided.iter().chain(&unchanged))
                .map(|r| r.to_string())
                .collect();
            assert_eq!(outcome.undecided, partial, "{name}");
        }
    }
}
