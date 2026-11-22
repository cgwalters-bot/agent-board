//! `stackHeld` (agent-board#1144, docs/job-plans.md#stacking-held-work): an
//! implement start held only by the issue's own open pull request
//! (`pr-open`), the bot's, builds on it as a stacked run ([`crate::stack`])
//! rather than waiting for its merge. The base is what [`stack::resolve`]
//! would accept for a plan's `stacks_on: N`, N the issue itself: a
//! `dispatch/` branch of the target repository itself that the bot's open
//! pull request comes from, so a visitor's or the operator's pull request,
//! or one from a fork, never becomes the code a run starts from. A run in
//! flight (on the issue or the base), several pull requests, any overlap but
//! the base's or an overlap read cut short by its bounds, a promotion or
//! midstream fork, a read that fails and any other plan keep holding.
use super::*;

/// A hold of an issue's own facts that `stackHeld` may stack past.
#[derive(Clone, Debug)]
pub(super) struct Under {
    pub hold: Hold,
    /// The issue's own open pull request, which its run stacks on.
    pub pull: u64,
}

impl Under {
    /// Whether `held`, read again, is still this hold.
    pub(super) fn is(&self, held: &Hold) -> bool {
        held.kind == self.hold.kind && held.reason == self.hold.reason
    }
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    /// `held`, [`hold`]'s of `facts`, where `stackHeld` may stack past it:
    /// the issue's complete set of trusted open pull requests is one, the
    /// bot's own from the issue's repository; never with a run in flight on
    /// the issue. Reads nothing.
    pub(super) fn stackable(
        &self,
        facts: &Facts,
        held: &Hold,
        busy: &BTreeMap<String, String>,
    ) -> Option<Under> {
        if !self.config.stack_held
            || held.kind != HoldKind::PrOpen
            || held.operator
            || busy.contains_key(&key(&facts.issue))
            || !facts.pulls_complete
            || facts.promotion.is_some()
            || self.promotes(&facts.issue.repository)
        {
            return None;
        }
        let writers = self.writers();
        let trusted: Vec<&ClosingPull> = (facts.pulls.iter())
            .filter(|p| is(p.author.as_deref(), &writers))
            .collect();
        let [pull] = trusted[..] else {
            return None;
        };
        (is(pull.author.as_deref(), &[&self.config.bot])
            && pull
                .repository
                .eq_ignore_ascii_case(&facts.issue.repository))
        .then(|| Under {
            hold: held.clone(),
            pull: pull.number,
        })
    }

    /// Whether work in `repository` may be promoted upstream, or the
    /// midstream forks that would say were not read. Promotion replays a
    /// pull request by its stored plan, in which a stack the pass chose
    /// leaves no `stacks_on`, so it would publish the upper slice without
    /// waiting for the one below it.
    fn promotes(&self, repository: &str) -> bool {
        self.upstream_fork(repository) || self.midstreams.is_none()
    }

    /// Whether the newest plan in `comments` is one `stackHeld` stacks for
    /// `issue`: an accepted implement plan of the issue itself, in its own
    /// repository, that stacks on nothing and is not dispatched yet. A read
    /// that fails says no, so the issue keeps its hold.
    pub(super) fn stackable_plan(
        &self,
        issue: &ItemRef,
        comments: &[plan::Comment],
        writers: &[&str],
    ) -> bool {
        let Ok((plan, from)) = plan::newest_plan(comments, writers) else {
            return false;
        };
        if plan.kind != "implement"
            || plan.stacks_on.is_some()
            || plan.target_repo.is_some()
            || !plan.repo.eq_ignore_ascii_case(&issue.repository)
            || plan.item != issue.number
        {
            return false;
        }
        plan::plan_id(from)
            .and_then(|id| batch::held(&self.client, comments, writers, &id, issue))
            .is_ok_and(|held| held.is_empty())
    }

    /// `plan`, which changes `dest`, stacked on `dest`'s own open pull
    /// request `pull`, and the lower issue and base pull request the
    /// dispatch checks again. `None` keeps the hold: a run in flight on the
    /// issue or on `pull` (a fix round), a read that fails or a base
    /// [`stack::resolve`] does not accept or that is not `pull`.
    pub(super) fn stack_on(
        &self,
        (dest, plan): (&ItemRef, &plan::JobPlan),
        pull: u64,
        busy: &BTreeMap<String, String>,
    ) -> Option<(plan::Stacked, (u64, u64))> {
        let lower = dest.number;
        if stack_hold(dest, Some(lower), busy).is_some() {
            return None;
        }
        let plan = plan::JobPlan {
            stacks_on: Some(lower),
            ..plan.clone()
        };
        let stacked = plan::stack(&self.client, dest, plan, Some(&self.config.bot)).ok()?;
        let base = stacked.as_ref().ok()?.1.as_ref()?.pull;
        if base != pull || base_in_flight(dest, base, busy).is_some() {
            return None;
        }
        Some((stacked.ok()?, (lower, base)))
    }
}

/// A run in flight on pull request `base` of `dest`'s repository, such as
/// stage 2's fix round: a run stacked on it would start mid-push.
pub(super) fn base_in_flight(
    dest: &ItemRef,
    base: u64,
    busy: &BTreeMap<String, String>,
) -> Option<Hold> {
    let pull = ItemRef {
        number: base,
        ..dest.clone()
    };
    fix_in_flight(&pull, busy)
}
