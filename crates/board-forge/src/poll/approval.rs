//! Head-bound evidence for an `approve` wait, which the needs-you list may
//! publish beside its row (`holds.approvalEvidence`, docs/pass.md). It is
//! made only from what the poll already read of the pull request at the
//! head it waits at: never from labels or prose, and never carried to
//! another head or pass. It is display data and grants no authority.
use super::*;
use serde::{Deserialize, Serialize};

/// The needs-you extension's schema: a reader that does not know it treats
/// every row's evidence as unknown.
pub const EVIDENCE_SCHEMA: &str = "agent-board/approval-evidence/v1";

/// What the poll read of one pull request at `assessed_head`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub assessed_head: String,
    pub diff: Diff,
    pub ci: Ci,
    /// The review job's newest verdict on the head, where its run was
    /// verified as the configured review workflow's run for the head
    /// ([`review_run_matches`]); null where there is none or it was not.
    pub review: Option<AgentReview>,
}

/// The pull request's line counts, or `"unavailable"` where the read did
/// not give them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Diff {
    Lines { additions: u64, deletions: u64 },
    Unavailable(Unavailable),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unavailable {
    Unavailable,
}

/// Every check on the head, required or not, as a direct merge judges it
/// ([`classify_all_checks`]). `complete` says the head's checks were read
/// whole; only then may `state` be `green`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ci {
    pub state: CiState,
    pub complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiState {
    Green,
    Pending,
    Failing,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReview {
    pub verdict: Verdict,
    /// When the review was submitted.
    pub at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approved,
    ChangesRequested,
    Withheld,
}

/// The evidence of `pull` as [`PULL_QUERY`] and its check pages read it,
/// with `run` the review job's run read for it, if any.
pub(super) fn evidence(
    pull: &Value,
    required: &[Value],
    run: Option<&Value>,
    config: &Config,
) -> Evidence {
    let head = text(pull, "/headRefOid");
    let diff = match (pull["additions"].as_u64(), pull["deletions"].as_u64()) {
        (Some(additions), Some(deletions)) => Diff::Lines {
            additions,
            deletions,
        },
        _ => Diff::Unavailable(Unavailable::Unavailable),
    };
    // Another commit's checks say nothing of the head. Checks past the
    // pages read are not known: a failure seen is, but nothing is green.
    let at_head = !head.is_empty() && text(pull, "/commits/nodes/0/commit/oid") == head;
    let complete = at_head && !truncated(pull).contains(&"checks");
    let checks = classify_all_checks(pull, required);
    let state = match () {
        _ if at_head && !checks.failed.is_empty() => CiState::Failing,
        _ if !complete => CiState::Unknown,
        _ if !checks.pending.is_empty() => CiState::Pending,
        _ => CiState::Green,
    };
    let review = job_verdict(pull)
        .filter(|(_, review)| review_run_matches(pull, review, run, config))
        .and_then(|(verdict, review)| {
            let at = timestamp(review["submittedAt"].as_str())?
                .format(&Rfc3339)
                .ok()?;
            let verdict = match verdict {
                Review::Approved => Verdict::Approved,
                Review::ChangesRequested => Verdict::ChangesRequested,
                Review::Withheld => Verdict::Withheld,
            };
            Some(AgentReview { verdict, at })
        });
    Evidence {
        assessed_head: head.to_owned(),
        diff,
        ci: Ci { state, complete },
        review,
    }
}
