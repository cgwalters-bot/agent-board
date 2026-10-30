//! `staleDays` (agent-board#325): a pull request of the bot's that the poll
//! stopped at, labelled `needs-human`, holds every board item that overlaps
//! it for as long as it stays open. Where it conflicts with its base or has
//! changes requested, and the operator has not acted on it for that many
//! days since the stop, the poll acts instead: fix round 1 where no fix round
//! ran before the stop, started and recorded as a round on the operator's
//! answer is ([`Pass::stop_round`]); else it closes the pull request.
//!
//! What it stops is a stale pull request holding its overlapping items
//! indefinitely, and only that: the operator acting at all keeps it theirs.
//! The operator acted where they commented or reviewed with text since the
//! stop, pushed (the head is not the one stopped at) or put the label on
//! themselves (its newest `labeled` event is not the bot's). The round's
//! marker counts it, so a stop after it is a close: one round per pull
//! request, unless a person resets it. The close posts a stop first, so a
//! pull request someone reopens is not closed again for that many days.
use super::*;
use comments::stamp;

/// The most days `staleDays` may be.
pub const STALE_DAYS_LIMIT: u64 = 30;

/// What a stale stop's round needs: its brief, and the bot's `labeled` event
/// it takes the label off by.
pub(super) struct Stale {
    pub(super) brief: String,
    pub(super) labelled: Value,
}

impl<R: Rest, G: Transport> Pass<'_, R, G> {
    /// The stale step for `pull`, whose newest marker is `stop`, or none: a
    /// decision, `fix` (round 1) or `close`. Nothing is read before the stop
    /// is old enough.
    pub(super) fn stale(
        &mut self,
        repo: &str,
        pull: &Value,
        checks: &Checks,
        (stop, comments): (&Marker, &[Value]),
    ) -> Result<Option<(Decision, Stale)>> {
        let days = self.config.policy.stale_days;
        let head = text(pull, "/headRefOid");
        let Some(at) = stopped_at(stop, comments) else {
            return Ok(None);
        };
        // A partial read is left alone, as `decide` waits on one: a review
        // past those read, such as the operator's approval, could outweigh
        // the change request the read ends on.
        if days == 0
            || !stop.stopped
            || !labelled(pull)
            || !is_harness(pull, self.config)
            || stop.sha.as_deref() != Some(head)
            || self.now - at < time::Duration::days(days as i64)
            || !truncated(pull).is_empty()
        {
            return Ok(None);
        }
        // The review job's verdict counts only by its run, which a labelled
        // pull request's decision does not read.
        let review_run = match review_run_id(pull, self.config) {
            Some(run) => Some(
                self.get_url(
                    &format!("{API_ROOT}repos/{repo}/actions/runs/{run}"),
                    slim_run,
                    false,
                )?
                .0,
            ),
            None => None,
        };
        let review = match head_review(pull, self.config, review_run.as_ref()) {
            Some((Review::ChangesRequested, body)) => Some(body),
            _ => None,
        };
        let base = text(pull, "/baseRefName");
        let why = match (pull["mergeable"] == CONFLICTING, review) {
            (true, Some(_)) => format!("conflicts with {base} and review requested changes"),
            (true, None) => format!("conflicts with {base}"),
            (false, Some(_)) => "review requested changes".into(),
            (false, None) => return Ok(None),
        };
        let number = pull["number"]
            .as_u64()
            .context("pull request has no number")?;
        if !self.said(repo, number, comments, true, at)?.is_empty() {
            return Ok(None);
        }
        let labelled = self.labelled(repo, number, false)?;
        if text(&labelled, "/actor/login") != self.config.bot {
            return Ok(None);
        }
        let since = format!(
            "{why}, and {} has not acted on it in the {days} days since the poll stopped on {}",
            self.config.operator,
            stamp(at)?
        );
        let decision = if stop.rounds > 0 {
            step(
                "close",
                format!("{since}, after a fix round, so it is closed"),
            )
        } else {
            Decision {
                round: Some(1),
                ..step("fix", format!("{since}, so it gets a fix round"))
            }
        };
        Ok(Some((
            decision,
            Stale {
                brief: fix_task(repo, pull, checks, review, 1),
                labelled: labelled["id"].clone(),
            },
        )))
    }

    /// Close `pull`, a stale stop's, after a stop that says why: reopened, it
    /// is `staleDays` from that stop before the poll acts on it again.
    ///
    /// The close is claimed from the stop, as a reset's or an answer's round
    /// is, so of those one goes on: a round claimed first keeps it open.
    /// Before the claim it reads again what the operator acts by: the label,
    /// still on and last put on by the bot (`labelled`), and the comments,
    /// both without the cache; the head, the newest marker and what they said
    /// since the stop. GitHub
    /// has no conditional write, so one who acts between those reads and the
    /// close is not seen.
    pub(super) fn close(
        &mut self,
        repo: &str,
        pull: &Value,
        (stop, reason): (&Marker, &str),
        labelled: &Value,
    ) -> Result<String> {
        let number = pull["number"]
            .as_u64()
            .context("pull request has no number")?;
        let head = stop.sha.clone().context("the stop has no head")?;
        let events = self.paged(
            &format!("repos/{repo}/issues/{number}/events?per_page=100"),
            true,
        )?;
        let label = events.iter().rfind(|e| {
            matches!(e["event"].as_str(), Some("labeled" | "unlabeled"))
                && e["label"]["name"] == LABEL
        });
        if labelled.is_null()
            || label.is_none_or(|e| e["event"] != "labeled" || e["id"] != *labelled)
        {
            return Ok(format!("not closed: {LABEL} was taken off or put on again"));
        }
        let fresh = self.get(&format!("repos/{repo}/pulls/{number}"))?;
        if fresh["state"] != "open" || text(&fresh, "/head/sha") != head {
            return Ok("not closed: it was pushed to or closed meanwhile".into());
        }
        let comments = self.paged(
            &format!("repos/{repo}/issues/{number}/comments?per_page=100"),
            true,
        )?;
        let at = stopped_at(stop, &comments).context("the stop is not among the comments")?;
        if find_marker(&comments, &self.config.bot).id != stop.id
            || !self.said(repo, number, &comments, true, at)?.is_empty()
        {
            return Ok(format!(
                "not closed: {} or another pass acted on it meanwhile",
                self.config.operator
            ));
        }
        // The marker over the claim and the close are kept from the budget.
        let state = json!({"from": claim_from(stop), "close": true, "sha": head});
        let of = format!("Stale close of `{}`", short(&head));
        let claim = match self.claim_reserved(repo, number, stop, &comments, (state, &of, 2))? {
            Ok(claim) => claim,
            Err((id, _)) => {
                return Ok(format!(
                    "not closed: another pass went first, in comment {id}"
                ));
            }
        };
        let mut closed = Marker {
            rounds: stop.rounds,
            sha: stop.sha.clone(),
            stopped: true,
            queue: stop.queue.clone(),
            ..Marker::default()
        };
        self.mark_claim(
            repo,
            claim,
            &mut closed,
            &format!(
                "The poll closed this pull request: {reason}. Its overlapping items are no longer held by it. Reopen it to keep it: the poll leaves it to a person for another {} days.",
                self.config.policy.stale_days
            ),
        )?;
        let rest = self.rest;
        self.send(
            rest,
            "PATCH",
            &format!("repos/{repo}/pulls/{number}"),
            json!({"state": "closed"}),
            0,
        )?;
        Ok("closed".into())
    }
}

/// When `stop` was posted, where it is among `comments`.
fn stopped_at(stop: &Marker, comments: &[Value]) -> Option<OffsetDateTime> {
    comments
        .iter()
        .find(|c| c["id"].as_u64() == stop.id)
        .and_then(|c| timestamp(c["created_at"].as_str()))
}
