//! Per-pass caps on what `agent-board pass` starts and writes
//! (docs/pass.md#order-and-effects): `--max-dispatch` and `--max-writes`.
//!
//! The unit is a step, not a request: a fix round's claim, marker and
//! dispatch are one dispatch, and a stop's label and marker are one write.
//! A step is checked against its cap before its first write and counted
//! once it has written, so it is either taken whole or held whole.
use std::cell::Cell;

/// What a step spends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Starts an agent run: a scheduled start, or a pull request's fix,
    /// rebase, continuation or answer round.
    Dispatch,
    /// Any other forge write: a pull request's comments, labels, retitle,
    /// retarget, enqueue or merge.
    Write,
}

impl Kind {
    /// The flag that caps it, as a held step names it.
    pub fn flag(self) -> &'static str {
        match self {
            Self::Dispatch => "--max-dispatch",
            Self::Write => "--max-writes",
        }
    }
}

/// Board Status changes one pass makes at most, unless told otherwise: a
/// first pass over a board that drifted for months should be looked at
/// before it rewrites it.
pub const STATUS_CHANGES: usize = 50;

/// The caps one pass was given; none is uncapped, but for `status_changes`,
/// which is [`STATUS_CHANGES`] by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    pub dispatch: Option<usize>,
    pub writes: Option<usize>,
    /// More Status changes than this refuse the Status stage whole.
    pub status_changes: Option<usize>,
}

impl Limits {
    /// The most Status changes the pass makes before it refuses them all.
    pub fn status_changes(&self) -> usize {
        self.status_changes.unwrap_or(STATUS_CHANGES)
    }
}

/// What one pass spent of its [`Limits`], and how many steps they held.
/// Shared by the pass's stages, so the pull request follow-up's runs count
/// against the dispatches scheduling may start.
#[derive(Debug, Default)]
pub struct Caps {
    limits: Limits,
    dispatched: Cell<usize>,
    written: Cell<usize>,
    held: Cell<usize>,
}

impl Caps {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            ..Self::default()
        }
    }

    fn of(&self, kind: Kind) -> (&Cell<usize>, Option<usize>) {
        match kind {
            Kind::Dispatch => (&self.dispatched, self.limits.dispatch),
            Kind::Write => (&self.written, self.limits.writes),
        }
    }

    /// Whether `--max-dispatch` or `--max-writes` is set: uncapped, a pass
    /// only counts its steps.
    pub fn limited(&self) -> bool {
        self.limits.dispatch.is_some() || self.limits.writes.is_some()
    }

    /// The most Status changes the pass makes ([`Limits::status_changes`]).
    pub fn status_changes(&self) -> usize {
        self.limits.status_changes()
    }

    /// Whether a step of `kind` would be held now.
    pub fn full(&self, kind: Kind) -> bool {
        let (spent, limit) = self.of(kind);
        limit.is_some_and(|limit| spent.get() >= limit)
    }

    /// Whether one more step of `kind` would be exactly one past a cap
    /// above zero: a step that may go past the cap ([`Kind::Write`]'s hold
    /// snapshot of a pass that made progress) goes at most that far, and
    /// never past a cap of zero, which allows nothing.
    pub fn one_past(&self, kind: Kind) -> bool {
        let (spent, limit) = self.of(kind);
        limit.is_some_and(|limit| limit > 0 && spent.get() == limit)
    }

    /// Before a step of `kind`: `None` where it may go ahead, else the
    /// words for its line, `held by --max-…`, and it is counted as held.
    pub fn check(&self, kind: Kind) -> Option<String> {
        self.check_many(kind, 1)
    }

    /// [`Caps::check`] for `n` steps of `kind` that are taken together or
    /// not at all, such as an epic plan's apply: one held step if they do
    /// not all fit.
    pub fn check_many(&self, kind: Kind, n: usize) -> Option<String> {
        let (spent, limit) = self.of(kind);
        let held = limit.is_some_and(|limit| spent.get().saturating_add(n) > limit);
        held.then(|| {
            self.held.set(self.held.get() + 1);
            format!("held by {}", kind.flag())
        })
    }

    /// A step held by the weekly budget rather than a cap, counted in the
    /// summary's `held` as one.
    pub fn hold(&self) {
        self.held.set(self.held.get() + 1);
    }

    /// A step of `kind` that went ahead and wrote, or that a preview plans.
    pub fn count(&self, kind: Kind) {
        self.count_many(kind, 1);
    }

    /// `n` steps of `kind` that went ahead together ([`Caps::check_many`]).
    pub fn count_many(&self, kind: Kind, n: usize) {
        let (spent, _) = self.of(kind);
        spent.set(spent.get() + n);
    }

    /// The pass's last line: `summary: dispatched 1/1, writes 3/5, held 12`,
    /// without a `/N` where that kind is uncapped.
    pub fn summary(&self) -> String {
        let spent = |kind| {
            let (spent, limit) = self.of(kind);
            match limit {
                Some(limit) => format!("{}/{limit}", spent.get()),
                None => spent.get().to_string(),
            }
        };
        format!(
            "summary: dispatched {}, writes {}, held {}",
            spent(Kind::Dispatch),
            spent(Kind::Write),
            self.held.get()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_table() {
        // Limits, the steps tried in order, which were held, and the summary.
        type Case = (Limits, &'static [Kind], &'static [bool], &'static str);
        use Kind::{Dispatch as D, Write as W};
        let cases: &[Case] = &[
            (
                Limits::default(),
                &[D, D, W],
                &[false, false, false],
                "summary: dispatched 2, writes 1, held 0",
            ),
            (
                Limits {
                    dispatch: Some(1),
                    writes: Some(2),
                    ..Limits::default()
                },
                &[D, W, D, W, W, D],
                &[false, false, true, false, true, true],
                "summary: dispatched 1/1, writes 2/2, held 3",
            ),
            (
                Limits {
                    dispatch: Some(0),
                    ..Limits::default()
                },
                &[D, W],
                &[true, false],
                "summary: dispatched 0/0, writes 1, held 1",
            ),
        ];
        for (limits, steps, held, summary) in cases {
            let caps = Caps::new(*limits);
            let got: Vec<bool> = steps
                .iter()
                .map(|&kind| match caps.check(kind) {
                    Some(line) => {
                        assert_eq!(line, format!("held by {}", kind.flag()));
                        true
                    }
                    None => {
                        caps.count(kind);
                        false
                    }
                })
                .collect();
            assert_eq!(got, *held, "{limits:?}");
            assert_eq!(caps.summary(), *summary);
        }
    }
}
