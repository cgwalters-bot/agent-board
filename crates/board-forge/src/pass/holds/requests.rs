//! Read-only decision descriptors, beside (not inside) strict v2 Ask records.
//! Neither a descriptor nor bot prose grants mutation authority.
use super::*;
use sha2::{Digest, Sha256};

/// Observation clocks, limit readings and the context's facts alone do not
/// change a decision or force a write: a step's checks or a run aging out of
/// the day would otherwise write every pass. Hourly confirmation refreshes
/// them; which sources were read and which limits hold still count.
pub(super) fn strip_observation_times(list: &mut serde_json::Map<String, Value>) {
    if let Some(checks) = list
        .get_mut("carry_validation")
        .and_then(Value::as_object_mut)
    {
        for check in checks.values_mut().filter_map(Value::as_object_mut) {
            check.remove("checked_at");
        }
    }
    if let Some(evidence) = list
        .get_mut("approval_evidence")
        .and_then(Value::as_object_mut)
    {
        evidence.remove("observed_at");
    }
    if let Some(context) = list.get_mut("context").and_then(Value::as_object_mut) {
        for field in ["observed_at", "wip", "omitted", "items"] {
            context.remove(field);
        }
        if let Some(limits) = context.get_mut("limits").and_then(Value::as_object_mut) {
            for fields in limits.values_mut().filter_map(Value::as_object_mut) {
                for reading in super::LIMIT_READINGS {
                    fields.remove(*reading);
                }
            }
        }
    }
    for name in ["requests", "request_sources"] {
        if let Some(records) = list.get_mut(name).and_then(Value::as_array_mut) {
            for record in records {
                if let Some(fields) = record.as_object_mut() {
                    fields.remove("observed_at");
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Request {
    pub key: String,
    pub identity: String,
    pub forge: String,
    pub question: String,
    pub recommendation: Option<String>,
    pub target: Reference,
    pub revision: Option<String>,
    pub requested_operator: String,
    pub requester: String,
    pub ownership_evidence: String,
    pub sources: Vec<Reference>,
    pub observed_at: Option<String>,
    pub source_time: Option<String>,
    pub facts: String,
    pub quality: String,
    pub actions: Vec<Action>,
    pub resolution: String,
}

/// The [`Request::quality`] of a fresh, known, concrete decision.
pub const DECISION: &str = "decision";

/// The [`Action`] kind of a forge review bound to the target's head.
pub const REVIEW_AT_HEAD: &str = "review-at-head";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Action {
    pub kind: String,
    pub supported: bool,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceAsk {
    /// The legacy card this source's decision is represented by.
    pub key: String,
    pub ask: Ask,
    pub observed_at: Option<String>,
    pub source_time: Option<String>,
}

/// Keep the semantic projection when inventory pruning changes a group's
/// representative. No facts read at the old PR's head apply to the new one.
pub(crate) fn refresh_group(request: &mut Request, ask: &Ask) {
    request.question = ask.ask.clone();
    if request.target.number == ask.action.number {
        return;
    }
    request.identity = identity(ask, &request.forge);
    request.target = ask.action.clone();
    request.revision = None;
    request.recommendation = None;
    request.sources = vec![ask.source_ref.clone()];
    request.observed_at = None;
    request.source_time = None;
    request.facts = ask.ask.clone();
    request.quality = "unverified-carried".into();
    for action in &mut request.actions {
        action.supported = false;
    }
}

pub(super) fn previous_source(ask: &Ask, previous: Option<&Value>) -> Option<SourceAsk> {
    published_list(previous)
        .and_then(|p| p["request_sources"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|source| serde_json::from_value::<SourceAsk>(source.clone()).ok())
        .find(|source| {
            source.ask.key == ask.key
                && source.ask.source == ask.source
                && source.ask.head == ask.head
                && source.ask.source_ref == ask.source_ref
                && source.ask.action.valid()
                && source.ask.source_ref.valid()
        })
}

/// Identity excludes source and revision. A proposal comment is deliberately
/// independent even when its title or target is identical to another's.
pub(crate) fn identity(ask: &Ask, host: &str) -> String {
    let action = &ask.action;
    let semantic = match action.kind {
        RefKind::ApprovePullRequest => "approve".to_owned(),
        RefKind::ReviewPullRequest => "review".to_owned(),
        RefKind::ReplyPullRequest => "reply".to_owned(),
        RefKind::ClosePullRequest => "close".to_owned(),
        RefKind::OperatorAction => format!("proposal/{}", action.comment.unwrap_or_default()),
        _ if ask.source == DECISIONS || ask.source == DRAFTS => ask.key.clone(),
        // Preserve the existing pass-alarm grouping. Other issue questions
        // must not collapse merely because they share an issue number.
        _ if ask.source == "pass" => "pass-alarm".to_owned(),
        _ => {
            let question = ask.ask.split_whitespace().collect::<Vec<_>>().join(" ");
            format!("question/{:x}", Sha256::digest(question.as_bytes()))
        }
    };
    format!(
        "{host}/{}#{}/{}",
        action.repository.to_ascii_lowercase(),
        action.number,
        semantic
    )
}

pub(super) fn describe(
    ask: &Ask,
    evidence: &[Ask],
    snapshot: &Snapshot,
    reading: &Reading,
    previous: Option<&Value>,
) -> Request {
    let identity = identity(ask, &reading.on.host);
    let mut sources = Vec::new();
    for other in evidence.iter().filter(|other| {
        identity == self::identity(other, &reading.on.host) && ask.head == other.head
    }) {
        if !sources.contains(&other.source_ref) {
            sources.push(other.source_ref.clone());
        }
    }
    let hold = snapshot
        .holds
        .iter()
        .find(|h| h.item.as_deref() == Some(ask.item.as_str()) && h.kind.name() == ask.kind);
    let concrete = match ask.action.kind {
        RefKind::OperatorAction
        | RefKind::ApprovePullRequest
        | RefKind::ReviewPullRequest
        | RefKind::ReplyPullRequest
        | RefKind::ClosePullRequest => true,
        _ if ask.kind == "review-promote" || matches!(ask.source.as_str(), DECISIONS | DRAFTS) => {
            true
        }
        _ => hold.is_some_and(|h| h.question.is_some() || h.reason != ask.ask),
    };
    let known = hold.is_some_and(|h| h.kind != HoldKind::Other)
        || matches!(
            ask.source.as_str(),
            REVIEWS | PROPOSALS | DIGEST | DECISIONS | DRAFTS
        );
    let source_read = match ask.source.as_str() {
        "epics" => {
            if reading.epics && snapshot.pass.live_epics.is_none() {
                Read::Unread
            } else {
                Read::Complete
            }
        }
        "items" => {
            if snapshot.pass.candidates.is_none() {
                Read::Unread
            } else {
                Read::Complete
            }
        }
        _ => Read::Complete,
    };
    let fresh = !ask.carried && !hold.is_some_and(|h| h.carried) && source_read != Read::Unread;
    let previous_request = published_list(previous)
        .and_then(|p| p["requests"].as_array())
        .into_iter()
        .flatten()
        .find(|r| r["identity"] == identity && r["target"]["head"].as_str() == ask.head.as_deref());
    let previous_source = previous_source(ask, previous);
    let source_time = if matches!(ask.source.as_str(), PROPOSALS | DIGEST | DECISIONS | DRAFTS) {
        [&reading.proposals, &reading.digest]
            .into_iter()
            .chain(reading.inbox.values())
            .flat_map(|c| &c.asks)
            .find(|raw| raw.key == ask.key && raw.source_ref == ask.source_ref)
            .map(|raw| raw.waiting_since.clone())
            .or_else(|| previous_source.as_ref().and_then(|s| s.source_time.clone()))
    } else {
        None
    };
    let generic = matches!(
        ask.ask.trim().to_ascii_lowercase().as_str(),
        "act?" | "what should i do?" | "what next?" | "labelled needs-human"
    );
    let quality = if !fresh {
        "unverified-carried"
    } else if !known {
        "unknown-request"
    } else if !concrete || generic {
        "missing-concrete-decision"
    } else {
        DECISION
    };
    let headed = ask.head.as_deref().is_some_and(full_head);
    let mut actions = Vec::new();
    for (kind, supported, reason) in [
        (
            "promote",
            false,
            "Upstream promotion evaluation and writes are not implemented.",
        ),
        (
            "merge-or-enqueue",
            false,
            "Only the poll may merge/enqueue after fresh policy, review and check validation; this publication is not a merge endpoint.",
        ),
        (
            "reply",
            false,
            "No checked reply endpoint or answer acknowledgement is supplied by this publication; open the source and reply manually.",
        ),
        (
            "close",
            false,
            "A digest's stale assessment is not a current-state close precondition; no checked close endpoint is supplied here.",
        ),
    ] {
        actions.push(Action {
            kind: kind.into(),
            supported,
            reason: reason.into(),
        });
    }
    if matches!(
        ask.action.kind,
        RefKind::ApprovePullRequest | RefKind::ReviewPullRequest
    ) {
        actions.push(Action {
            kind: REVIEW_AT_HEAD.into(),
            supported: quality == DECISION && headed,
            reason: "Use a forge review bound to target.head, after re-reading the PR. Only fresh authorized approval at that head can release the poll's approval wait; it does not override checks or merge policy.".into(),
        });
    }
    if ask.action.kind == RefKind::OperatorAction {
        actions.push(Action {
            kind: "check-proposal".into(),
            supported: quality == DECISION,
            reason: "Re-read the exact source comment and run action check with caller scope before independently authorized action apply; a listed proposal is not approval.".into(),
        });
    }
    Request {
        key: ask.key.clone(),
        identity,
        forge: reading.on.host.clone(),
        question: one_line(&ask.ask, MAX_ASK).unwrap_or_default(),
        recommendation: ask.recommendation.as_deref().and_then(|r| one_line(r, MAX_ASK)),
        target: ask.action.clone(),
        revision: ask.head.clone().or_else(|| {
            match reading.summaries.1.get(&ask.key) {
                Some(said) => said.as_ref().and_then(|s| s.revision.clone()),
                None => published_list(previous)
                    .and_then(|p| p["summary_cache"][&ask.key]["revision"].as_str())
                    .map(str::to_owned),
            }
        }).or_else(|| {
            (ask.action.kind == RefKind::OperatorAction).then(|| source_time.clone()).flatten()
        }),
        requested_operator: reading.operator.into(),
        requester: match ask.source.as_str() {
            PROPOSALS => "operator-or-bot (authenticated proposal author)",
            DIGEST => "configured bot (authenticated digest)",
            DECISIONS | DRAFTS => "operator-or-bot (authenticated source)",
            _ => "pass policy",
        }.into(),
        ownership_evidence: match ask.source.as_str() {
            REVIEWS => "Poll decision explicitly waits on the configured operator, not a run, rerun, review job or queue.",
            PROPOSALS => "Scoped action-check accepted a proposal comment written and last edited by the configured operator or bot; this is a request, not authorization.",
            DIGEST => "Newest digest written and last edited by the configured bot requests operator attention; freshness must be rechecked.",
            DECISIONS => "Outstanding entry in the configured decisions document, written and last edited by the operator or bot; checked answers are not resolution.",
            DRAFTS => "Newest review draft written and last edited by the operator or bot on an open labelled issue; source inspection is not authorization to send.",
            _ => "Pass hold has operator=true; a done verdict alone does not release the hold.",
        }.into(),
        sources,
        observed_at: if fresh { Some(snapshot.at.clone()) } else if matches!(ask.source.as_str(), REVIEWS | PROPOSALS | DIGEST | DECISIONS | DRAFTS) {
            previous_source.and_then(|s| s.observed_at)
        } else {
            previous_request.and_then(|p| p["observed_at"].as_str()).map(str::to_owned)
        },
        source_time,
        facts: one_line(if ask.source == "pass" { &ask.ask } else {
            hold.map_or(ask.ask.as_str(), |h| h.reason.as_str())
        }, MAX_ASK).unwrap_or_default(),
        quality: quality.into(),
        actions,
        resolution: "Pending, never inferred complete from a bot done verdict or an answer in prose. Re-read the target and source before acting; the next complete source read must remove or change the underlying wait. Holds require label/Why/policy change; already-answered waits the poll can act on are not human requests. Null revision/source_time means unknown, not current.".into(),
    }
}
