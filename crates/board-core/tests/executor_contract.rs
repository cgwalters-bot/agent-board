//! Offline reference model for docs/architecture.md's proposed boundary.
//! Deliberately test-only: no executor, transport, token or production policy API.
use anyhow::{Result, ensure};
use board_core::ItemRef;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const SCHEMA: &str = "agent-board/executor-proposal/v1";
const MAX_BYTES: usize = 64 * 1024;
const MAX_ACTIONS: usize = 20;
const MAX_AGE: i64 = 300;
const FIXTURE: &str = include_str!("../../../fixtures/executor-proposal.json");
// Independently encoded with JSON.stringify and node:crypto SHA-256. Pin v1's
// field order/encoding rather than deriving the expected identity at test time.
const FIXTURE_ID: &str = "f27e75146db499c132c07ae55510025d1540d51a82c6c911925e6bb0851258f1";
const START_KEY: &str = "9587c0e074f2ddc6ff6c5b1c7de51e042dcfa7785035550b502386506e4ec655";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Proposal {
    schema: String,
    config_digest: String,
    producer_revision: String,
    observed_at: i64,
    created_at: i64,
    expires_at: i64,
    actions: Vec<Action>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Action {
    intent: Intent,
    evidence_digest: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Intent {
    Start {
        item: ItemRef,
        plan_comment: u64,
        plan_revision: String,
    },
    Merge {
        pull: ItemRef,
        head: String,
    },
    StatusLabel {
        item: ItemRef,
        status: LabelStatus,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LabelStatus {
    Todo,
    InProgress,
    NeedsHuman,
    Done,
}

fn digest(value: &impl Serialize) -> String {
    format!("{:x}", Sha256::digest(serde_json::to_vec(value).unwrap()))
}

fn hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

// Caller-owned values, not values taken from the submitted proposal.
struct Trusted {
    identity: String,
    config_digest: String,
    revision: String,
    now: i64,
}

fn baseline() -> (Proposal, Trusted) {
    let plan: Proposal = serde_json::from_str(FIXTURE).unwrap();
    let trusted = Trusted {
        identity: FIXTURE_ID.into(),
        config_digest: plan.config_digest.clone(),
        revision: plan.producer_revision.clone(),
        now: plan.created_at,
    };
    (plan, trusted)
}

fn inspect(bytes: &[u8], trusted: &Trusted) -> Result<Proposal> {
    ensure!(bytes.len() <= MAX_BYTES, "proposal byte bound");
    let plan: Proposal = serde_json::from_slice(bytes)?;
    ensure!(plan.schema == SCHEMA, "unsupported schema");
    ensure!(
        digest(&plan) == trusted.identity,
        "immutable identity mismatch"
    );
    ensure!(
        hex(&plan.config_digest, 64) && plan.config_digest == trusted.config_digest,
        "reviewed config mismatch"
    );
    ensure!(
        hex(&plan.producer_revision, 40) && plan.producer_revision == trusted.revision,
        "reviewed revision mismatch"
    );
    ensure!(
        plan.observed_at <= plan.created_at
            && plan.created_at <= trusted.now
            && trusted.now < plan.expires_at
            && plan
                .expires_at
                .checked_sub(plan.observed_at)
                .is_some_and(|age| age <= MAX_AGE),
        "stale or invalid interval"
    );
    ensure!(
        (1..=MAX_ACTIONS).contains(&plan.actions.len()),
        "intent bound"
    );
    let mut seen = BTreeSet::new();
    for action in &plan.actions {
        ensure!(hex(&action.evidence_digest, 64), "invalid evidence digest");
        let target = match &action.intent {
            Intent::Start {
                item,
                plan_comment,
                plan_revision,
            } => {
                ensure!(*plan_comment > 0, "missing source comment");
                time::OffsetDateTime::parse(
                    plan_revision,
                    &time::format_description::well_known::Rfc3339,
                )?;
                item
            }
            Intent::Merge { pull, head } => {
                ensure!(hex(head, 40), "invalid head");
                pull
            }
            Intent::StatusLabel { item, .. } => item,
        };
        target.validate()?;
        // This reference caller has exactly this host/repository scope.
        ensure!(
            target.host == "github.com" && target.repository == "example/work",
            "caller scope"
        );
        ensure!(seen.insert(digest(&action.intent)), "duplicate effect key");
    }
    Ok(plan) // Inspection is NOT effect authority.
}

// The only proposal-to-submission entry in this model. Reinspect bindings/time
// for every action, including when a previously inspected plan was queued.
fn attempt(
    bytes: &[u8],
    trusted: &Trusted,
    forge: &mut Forge,
    epoch: u64,
    index: usize,
    fresh: Option<&Action>,
) -> Result<()> {
    let plan = inspect(bytes, trusted)?;
    let action = plan
        .actions
        .get(index)
        .ok_or_else(|| anyhow::anyhow!("missing intent"))?;
    forge.submit(epoch, &trusted.identity, action, fresh)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Receipt {
    Pending { plan: String, epoch: u64 },
    Confirmed,
}

// Models a caller gate and authenticated forge records, not a new lock/ledger.
// Local service state is intentionally absent. Real forge attribution and drain
// guarantees belong to the later implementation, not this in-memory model.
#[derive(Default)]
struct Forge {
    owner: Option<u64>,
    receipts: BTreeMap<String, Receipt>,
    completed: BTreeSet<(String, String)>,
    submissions: usize,
}

// Injected authenticated forge result: correlation and exact typed target must
// both match the pending claim. Actual API decoding remains unimplemented.
struct ObservedEffect {
    claim_plan: String,
    intent: Intent,
}

impl Forge {
    fn enter(&mut self, epoch: u64) -> Result<()> {
        ensure!(
            self.owner.is_none(),
            "writer already active; no timeout steal"
        );
        self.owner = Some(epoch);
        Ok(())
    }

    fn submit(
        &mut self,
        epoch: u64,
        plan: &str,
        action: &Action,
        fresh: Option<&Action>,
    ) -> Result<()> {
        ensure!(self.owner == Some(epoch), "writer fenced");
        let key = digest(&action.intent);
        match self.receipts.get(&key) {
            Some(Receipt::Confirmed) => {
                if !matches!(action.intent, Intent::StatusLabel { .. })
                    || self.completed.contains(&(plan.into(), key.clone()))
                    || fresh.is_none()
                {
                    return Ok(());
                }
                // Reconciliation may revisit a previously confirmed status,
                // but only under a new proposal/full-pass invocation.
            }
            Some(Receipt::Pending { .. }) => anyhow::bail!("ambiguous; reconcile, never retry"),
            None => (),
        }
        // Fresh actions stand for existing pass rechecks, never proposal booleans.
        ensure!(fresh == Some(action), "fresh rederivation required");
        self.receipts.insert(
            key,
            Receipt::Pending {
                plan: plan.into(),
                epoch,
            },
        );
        self.submissions += 1; // A lost response leaves Pending durably on forge.
        Ok(())
    }

    fn recover(
        &mut self,
        epoch: u64,
        action: &Action,
        observed: Option<&ObservedEffect>,
    ) -> Result<()> {
        ensure!(self.owner == Some(epoch), "recovery writer fenced");
        let receipt = self.receipts.get_mut(&digest(&action.intent)).unwrap();
        let Receipt::Pending { plan, .. } = receipt else {
            anyhow::bail!("no pending claim");
        };
        let observed = observed.ok_or_else(|| anyhow::anyhow!("missing result evidence"))?;
        ensure!(
            observed.claim_plan == *plan && observed.intent == action.intent,
            "result does not match claim"
        );
        self.completed
            .insert((plan.clone(), digest(&action.intent)));
        *receipt = Receipt::Confirmed;
        Ok(())
    }
}

#[test]
fn wire_rejections_and_immutable_identity() {
    let (plan, trusted) = baseline();
    assert_eq!(digest(&plan), FIXTURE_ID);
    assert_eq!(digest(&plan.actions[0].intent), START_KEY);
    inspect(FIXTURE.as_bytes(), &trusted).unwrap();
    for (pointer, value) in [
        (
            "/schema",
            serde_json::json!("agent-board/executor-proposal/v2"),
        ),
        ("/config_digest", serde_json::json!("0".repeat(64))),
        ("/producer_revision", serde_json::json!("0".repeat(40))),
        ("/expires_at", serde_json::json!(plan.expires_at + 1)),
        ("/actions/0/intent/plan_comment", serde_json::json!(18)),
        ("/actions/1/intent/head", serde_json::json!("0".repeat(40))),
        ("/actions/2/intent/status", serde_json::json!("done")),
        (
            "/actions/0/evidence_digest",
            serde_json::json!("0".repeat(64)),
        ),
    ] {
        let mut altered = serde_json::to_value(&plan).unwrap();
        *altered.pointer_mut(pointer).unwrap() = value;
        assert!(inspect(&serde_json::to_vec(&altered).unwrap(), &trusted).is_err());
    }
    for pointer in [
        "",
        "/actions/0",
        "/actions/0/intent",
        "/actions/0/intent/item",
    ] {
        let mut value = serde_json::to_value(&plan).unwrap();
        value
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("unknown".into(), true.into());
        assert!(inspect(&serde_json::to_vec(&value).unwrap(), &trusted).is_err());
    }
    for bytes in [
        FIXTURE.replace("\"kind\": \"start\"", "\"kind\": \"http_request\""),
        FIXTURE.replace("\"plan_comment\": 17", "\"plan_comment\": null"),
        FIXTURE.replace(
            "\"plan_comment\": 17",
            "\"plan_comment\": 17, \"plan_comment\": 18",
        ),
        " ".repeat(MAX_BYTES + 1),
    ] {
        assert!(inspect(bytes.as_bytes(), &trusted).is_err());
    }
}

#[test]
fn trusted_binding_time_and_bounds_even_with_matching_identity() {
    let (plan, mut trusted) = baseline();
    for now in [plan.observed_at - 1, plan.expires_at, i64::MAX] {
        trusted.now = now;
        assert!(inspect(FIXTURE.as_bytes(), &trusted).is_err());
    }
    let (_, mut trusted) = baseline();
    trusted.config_digest = "0".repeat(64);
    assert!(inspect(FIXTURE.as_bytes(), &trusted).is_err());
    let (_, mut trusted) = baseline();
    trusted.revision = "0".repeat(40);
    assert!(inspect(FIXTURE.as_bytes(), &trusted).is_err());
    for case in [
        "empty",
        "cap",
        "duplicate",
        "interval",
        "overflow",
        "scope",
        "evidence",
    ] {
        let (mut plan, mut trusted) = baseline();
        match case {
            "empty" => plan.actions.clear(),
            "cap" => plan.actions = vec![plan.actions[0].clone(); MAX_ACTIONS + 1],
            "duplicate" => plan.actions.push(plan.actions[0].clone()),
            "interval" => plan.expires_at += 1,
            "overflow" => plan.observed_at = i64::MIN,
            "scope" => {
                if let Intent::Start { item, .. } = &mut plan.actions[0].intent {
                    item.repository = "other/work".into();
                }
            }
            "evidence" => plan.actions[0].evidence_digest.clear(),
            _ => unreachable!(),
        }
        trusted.identity = digest(&plan);
        assert!(
            inspect(&serde_json::to_vec(&plan).unwrap(), &trusted).is_err(),
            "{case}"
        );
    }
}

#[test]
fn fresh_rechecks_are_mandatory_for_every_kind() {
    let (_, trusted) = baseline();
    let plan = inspect(FIXTURE.as_bytes(), &trusted).unwrap();
    for (index, action) in plan.actions.iter().enumerate() {
        let mut forge = Forge::default();
        forge.enter(1).unwrap();
        let mut changed = action.clone();
        changed.evidence_digest = "0".repeat(64);
        for fresh in [None, Some(&changed)] {
            assert!(attempt(FIXTURE.as_bytes(), &trusted, &mut forge, 1, index, fresh).is_err());
            assert!(forge.receipts.is_empty());
            assert_eq!(forge.submissions, 0);
        }
        attempt(
            FIXTURE.as_bytes(),
            &trusted,
            &mut forge,
            1,
            index,
            Some(action),
        )
        .unwrap();
        assert_eq!(forge.submissions, 1);
    }
}

#[test]
fn rejected_or_queued_expired_proposals_never_submit() {
    let (plan, _) = baseline();
    for case in ["tampered", "stale", "config", "revision"] {
        let (_, mut trusted) = baseline();
        let mut bytes = FIXTURE.as_bytes().to_vec();
        match case {
            "tampered" => {
                let mut altered = plan.clone();
                altered.actions[0] = plan.actions[1].clone();
                bytes = serde_json::to_vec(&altered).unwrap();
            }
            "stale" => {
                // It was valid when queued; expiry is checked again at use.
                inspect(&bytes, &trusted).unwrap();
                trusted.now = plan.expires_at;
            }
            "config" => trusted.config_digest = "0".repeat(64),
            "revision" => trusted.revision = "0".repeat(40),
            _ => unreachable!(),
        }
        let mut forge = Forge::default();
        forge.enter(1).unwrap();
        assert!(
            attempt(&bytes, &trusted, &mut forge, 1, 0, Some(&plan.actions[0])).is_err(),
            "{case}"
        );
        assert_eq!(forge.submissions, 0);
        assert!(forge.receipts.is_empty());
    }
}

#[test]
fn replay_ambiguity_and_service_outage_use_only_forge_state() {
    let (plan, trusted) = baseline();
    let mut forge = Forge::default();
    forge.enter(1).unwrap();
    for action in &plan.actions {
        forge
            .submit(1, &trusted.identity, action, Some(action))
            .unwrap();
        assert!(
            forge
                .submit(1, &trusted.identity, action, Some(action))
                .is_err()
        );
    }
    assert!(
        forge.enter(2).is_err(),
        "fallback cannot steal an active writer"
    );
    let mut replacement = plan.clone();
    replacement.created_at += 1;
    let replacement_id = digest(&replacement);
    assert_ne!(trusted.identity, replacement_id);
    // Simulate old writer termination + credential drain, not timeout expiry.
    forge.owner = None;
    forge.enter(2).unwrap();
    for action in &replacement.actions {
        assert!(
            forge
                .submit(1, &trusted.identity, action, Some(action))
                .is_err()
        );
        assert!(
            forge
                .submit(2, &replacement_id, action, Some(action))
                .is_err()
        );
        let evidence = ObservedEffect {
            claim_plan: trusted.identity.clone(),
            intent: action.intent.clone(),
        };
        assert!(forge.recover(1, action, Some(&evidence)).is_err());
        assert!(forge.recover(2, action, None).is_err());
        let wrong_correlation = ObservedEffect {
            claim_plan: replacement_id.clone(),
            intent: action.intent.clone(),
        };
        assert!(forge.recover(2, action, Some(&wrong_correlation)).is_err());
        let mut wrong_intent = action.intent.clone();
        match &mut wrong_intent {
            Intent::Start { plan_comment, .. } => *plan_comment += 1,
            Intent::Merge { head, .. } => *head = "0".repeat(40),
            Intent::StatusLabel { item, .. } => item.number += 1,
        }
        let wrong_target = ObservedEffect {
            claim_plan: trusted.identity.clone(),
            intent: wrong_intent,
        };
        assert!(forge.recover(2, action, Some(&wrong_target)).is_err());
        // Expiry cannot block receipt recovery, but does block a new submission.
        let mut expired = baseline().1;
        expired.now = plan.expires_at;
        assert!(inspect(FIXTURE.as_bytes(), &expired).is_err());
        forge.recover(2, action, Some(&evidence)).unwrap();
        forge
            .submit(2, &trusted.identity, action, Some(action))
            .unwrap();
        forge.submit(2, &replacement_id, action, None).unwrap();
    }
    assert_eq!(forge.submissions, plan.actions.len());
    // Fallback has no proposal/service state: fresh full-pass decisions and the
    // same forge records suffice, including a pending item that stays held.
    let mut fresh = plan.actions[0].clone();
    if let Intent::Start { plan_comment, .. } = &mut fresh.intent {
        *plan_comment += 1;
    }
    forge.submit(2, "standalone", &fresh, Some(&fresh)).unwrap();
    assert!(forge.submit(2, "standalone", &fresh, Some(&fresh)).is_err());
    assert_eq!(forge.submissions, plan.actions.len() + 1);
    let mut fallback_label = plan.actions[2].clone();
    if let Intent::StatusLabel { status, .. } = &mut fallback_label.intent {
        *status = LabelStatus::Done;
    }
    forge
        .submit(2, "standalone", &fallback_label, Some(&fallback_label))
        .unwrap();
    assert_eq!(forge.submissions, plan.actions.len() + 2);
}

#[test]
fn status_cycles_reconcile_but_old_proposals_stay_consumed() {
    for fallback in [false, true] {
        let (original, mut trusted) = baseline();
        let original_id = trusted.identity.clone();
        let original_action = original.actions[2].clone();
        let mut forge = Forge::default();
        forge.enter(1).unwrap();
        for (step, status) in [
            LabelStatus::InProgress,
            LabelStatus::Done,
            LabelStatus::InProgress,
        ]
        .into_iter()
        .enumerate()
        {
            let mut plan = original.clone();
            plan.created_at += step as i64;
            if let Intent::StatusLabel {
                status: desired, ..
            } = &mut plan.actions[2].intent
            {
                *desired = status;
            }
            trusted.identity = digest(&plan);
            trusted.now = plan.created_at;
            let invocation = if fallback {
                format!("standalone/{step}")
            } else {
                trusted.identity.clone()
            };
            let action = &plan.actions[2];
            // Even after another transition, the original proposal is consumed.
            if step > 0 && !fallback {
                forge
                    .submit(1, &original_id, &original_action, Some(&original_action))
                    .unwrap();
                assert_eq!(forge.submissions, step);
            }
            if fallback {
                forge.submit(1, &invocation, action, Some(action)).unwrap();
            } else {
                attempt(
                    &serde_json::to_vec(&plan).unwrap(),
                    &trusted,
                    &mut forge,
                    1,
                    2,
                    Some(action),
                )
                .unwrap();
            }
            let evidence = ObservedEffect {
                claim_plan: invocation.clone(),
                intent: action.intent.clone(),
            };
            forge.recover(1, action, Some(&evidence)).unwrap();
            forge.submit(1, &invocation, action, Some(action)).unwrap();
            assert_eq!(forge.submissions, step + 1);
        }
        assert_eq!(forge.submissions, 3, "the final InProgress must be applied");
    }
}
