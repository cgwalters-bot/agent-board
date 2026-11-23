//! Opt-in machine recovery. Agent dispositions are checked-output comments,
//! never PR-body instructions; two triages and one grant bound autonomous spend.
use super::*;
use serde::{Deserialize, Serialize};

pub(super) const QUEUE_CONFLICT: &str = "merge queue conflict: rebase onto the base branch";
const RESULT_FENCE: &str = "```agent-board-pr-triage\n";
const RESULT_LIMIT: usize = 4096;
const TRIAGE_LIMIT: u64 = 2;

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Recovery {
    head: String,
    at: String,
    run: Option<u64>,
    granted: bool,
    pending: bool,
    attempts: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Disposition {
    head: String,
    run: u64,
    decision: Outcome,
    reason: String,
    question: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum Outcome {
    Obsolete,
    Replan,
    MoreRounds,
    Architecture,
}

pub(super) fn exhausted(config: &Config, reason: String) -> Decision {
    step(
        if config.policy.agent_recovery {
            "triage"
        } else {
            "needs-human"
        },
        reason,
    )
}

pub(super) fn granted(marker: &Marker) -> Marker {
    let mut next = marker.clone();
    next.continued_before = marker.continued_here();
    next.rounds = 0;
    next.diagnosed = false;
    next.continued = 0;
    next.rebased = 0;
    next.sha = None;
    next.run = None;
    next.continuing = false;
    next.rebasing = false;
    next.stopped = false;
    if let Some(recovery) = &mut next.recovery {
        recovery.granted = true;
        recovery.pending = false;
    }
    if let Some(queue) = &mut next.queue {
        queue.tries = 0;
        queue.reasons.clear();
    }
    next
}

pub(super) fn read(value: &Value) -> Recovery {
    // Corrupt or future reservations must not read as permission to dispatch
    // again. A person can explicitly release the record, as other stops are.
    serde_json::from_value(value.clone()).unwrap_or(Recovery {
        head: String::new(),
        at: String::new(),
        run: None,
        granted: true,
        pending: true,
        attempts: TRIAGE_LIMIT,
    })
}

pub(super) fn pending(marker: &Marker) -> bool {
    marker.recovery.as_ref().is_some_and(|r| r.pending)
}

fn disposition(
    comments: &[Value],
    bot: &str,
    marker: &Marker,
    recovery: &Recovery,
) -> Option<Disposition> {
    // Only the newest result for this reservation counts. A malformed newer
    // result cannot uncover an older grant. The run id binds the caller receipt.
    let body = comments
        .iter()
        .filter(|c| text(c, "/user/login") == bot && c["id"].as_u64() > marker.id)
        .filter(|c| text(c, "/body").starts_with(RESULT_FENCE))
        .max_by_key(|c| c["id"].as_u64())?
        .get("body")?
        .as_str()?;
    if body.len() > RESULT_LIMIT || body.matches(RESULT_FENCE).count() != 1 {
        return None;
    }
    let value = body.strip_prefix(RESULT_FENCE)?.split_once("\n```")?.0;
    let result: Disposition = serde_json::from_str(value).ok()?;
    let line = |s: &str| !s.trim().is_empty() && s.len() <= 1024 && !s.contains(['\n', '\r']);
    (result.head == recovery.head
        && Some(result.run) == recovery.run
        && line(&result.reason)
        && (result.decision != Outcome::Architecture
            || result.question.as_deref().is_some_and(line)))
    .then_some(result)
}

impl<R: Rest, G: Transport> Pass<'_, R, G> {
    pub(super) fn recovery_step(
        &mut self,
        _repo: &str,
        pull: &Value,
        marker: &Marker,
        comments: &[Value],
        mut decision: Decision,
    ) -> Result<Decision> {
        // Explicit holds were checked by the caller. Released historical stops
        // resume normal work; they are not a new exhaustion event.
        if marker.stopped || !truncated(pull).is_empty() {
            return Ok(decision);
        }
        let Some(recovery) = &marker.recovery else {
            if decision.action == "needs-human" {
                decision.action = "triage";
            }
            return Ok(decision);
        };
        let wait = |reason: &str| step("wait", reason).followed(timestamp(Some(&recovery.at)));
        // An old disposition cannot authorize a replacement head, but neither
        // may it veto progress independently justified by that head's gates.
        // Keep the reservation so its lifetime spend is not refunded.
        if !recovery.head.is_empty()
            && text(pull, "/headRefOid") != recovery.head
            && !matches!(decision.action, "triage" | "needs-human")
        {
            return Ok(decision);
        }
        if !recovery.pending {
            return Ok(
                if matches!(decision.action, "triage" | "needs-human")
                    && recovery.attempts >= TRIAGE_LIMIT
                {
                    wait(
                        "agent triage budget exhausted; needs agent replanning, not operator review",
                    )
                } else if matches!(decision.action, "triage" | "needs-human") {
                    step(
                        "triage",
                        "agent triage's bounded grant is exhausted; triage must close or re-plan it",
                    )
                } else {
                    decision
                },
            );
        }
        let Some(id) = recovery.run else {
            return Ok(wait(
                "agent triage reserved; dispatch outcome unknown, not dispatched twice",
            ));
        };
        let run = self.get(&format!(
            "repos/{}/actions/runs/{id}",
            self.config.dispatch_repo
        ))?;
        let workflow = self.apply.as_ref().map(|a| a.workflow.as_str()).or(self
            .config
            .policy
            .fix_caller_workflow
            .as_deref());
        if run["id"].as_u64() != Some(id)
            || run["event"] != DISPATCH_EVENT
            || run["head_branch"] != DISPATCH_REF
            || run["repository"]["full_name"] != self.config.dispatch_repo
            || !workflow.is_some_and(|w| run["path"] == format!("{WORKFLOWS_DIR}{w}"))
        {
            return Ok(wait(
                "agent triage run identity does not match its reservation",
            ));
        }
        if run["status"] != "completed" {
            return Ok(wait(
                "agent triage pending; no operator hold or automatic redispatch",
            ));
        }
        if run["conclusion"] != "success" {
            return Ok(if recovery.attempts < TRIAGE_LIMIT {
                step(
                    "triage",
                    "agent triage failed; one bounded replacement triage",
                )
            } else {
                wait(
                    "agent triage budget exhausted after failure; no operator hold or automatic redispatch",
                )
            });
        }
        if text(pull, "/headRefOid") != recovery.head {
            return Ok(wait(
                "head changed during agent triage; its disposition cannot authorize this head",
            ));
        }
        let Some(result) = disposition(comments, &self.config.bot, marker, recovery) else {
            return Ok(wait(
                "agent triage completed; waiting for its checked disposition",
            ));
        };
        Ok(match result.decision {
            Outcome::MoreRounds if !recovery.granted => step(
                "triage-grant",
                format!(
                    "Agent triage grants one bounded set of rounds: {}",
                    result.reason
                ),
            ),
            Outcome::MoreRounds => {
                wait("agent triage cannot grant rounds twice; it must close or re-plan the PR")
            }
            Outcome::Architecture => step(
                "needs-human",
                format!(
                    "Architectural decision: {} Q: {}",
                    result.reason,
                    result.question.unwrap_or_default()
                ),
            ),
            Outcome::Obsolete | Outcome::Replan => wait(&format!(
                "Agent triage chose {:?}: {}; caller's checked outputs close or re-plan the PR",
                result.decision, result.reason
            )),
        })
    }

    pub(super) fn start_triage(
        &mut self,
        repo: &str,
        pull: &Value,
        (marker, read): (&Marker, &[Value]),
        reason: &str,
    ) -> Result<String> {
        ensure!(
            marker
                .recovery
                .as_ref()
                .is_none_or(|r| r.attempts < TRIAGE_LIMIT),
            "PR triage budget exhausted"
        );
        if let Err(why) = self.admits() {
            return Ok(format!("not started: {why}"));
        }
        let candidates = self.config.policy.kind_pool_choices("triage");
        let pool = match self.paced(candidates.first().cloned(), &candidates) {
            Ok(pool) => pool,
            Err(why) => return Ok(format!("not started: {why}")),
        };
        if self.apply.is_none() {
            self.dispatched += 1;
            return Ok(format!(
                "would start agent triage pool={}",
                pool.as_deref().unwrap_or("default")
            ));
        }
        let number = pull["number"].as_u64().context("triage PR number")?;
        let head = text(pull, "/headRefOid");
        let (owner, name) = repo.split_once('/').context("triage repository")?;
        let current = self.read_pull(owner, name, number)?;
        if current["state"] != "OPEN"
            || current["headRefOid"] != pull["headRefOid"]
            || !truncated(&current).is_empty()
            || labelled(&current)
            || is_held(&current)
            || self
                .recheck_review_hold(
                    repo,
                    number,
                    &current,
                    marker,
                    review_hold(self.config, pull).is_some(),
                )?
                .is_some()
        {
            return Ok("not started: PR changed or held".into());
        }
        let state = json!({"from": claim_from(marker), "triage": head});
        let claim = match self.claim(
            repo,
            number,
            marker,
            read,
            (state, "Agent triage of exhausted PR work"),
        )? {
            Ok(claim) => claim,
            Err((id, _)) => {
                return Ok(format!(
                    "not started: triage already claimed in comment {id}"
                ));
            }
        };
        self.dispatched += 1;
        let mut next = Marker {
            recovery: Some(Recovery {
                head: head.into(),
                at: self.now.format(&Rfc3339)?,
                run: None,
                granted: marker.recovery.as_ref().is_some_and(|r| r.granted),
                pending: true,
                attempts: marker
                    .recovery
                    .as_ref()
                    .map_or(1, |r| r.attempts.saturating_add(1)),
            }),
            ..marker.clone()
        };
        self.mark_claim(
            repo,
            claim,
            &mut next,
            &format!("Agent triage reserved: {reason}"),
            (marker, read),
        )?;
        let task = format!(
            "Triage {repo}#{number} at head {head}. Its recovery budget is exhausted: {reason}. \
             Read the PR, issue, runs and current base. Decide whether it is obsolete (close it), \
             needs replanning (create a fresh issue with an implement plan and close this PR linking it), \
             or merits one more bounded set of repair rounds. Record the concrete evidence and why. \
             Engineering problems, conflicts and exhausted rounds are not operator questions. Only a truly \
             architectural decision may ask the operator, with a concrete question. Do not request review \
             or add needs-human. Use only the caller's independently checked safe outputs for closure, \
             issue creation and a comment on this PR. Never mutate the forge directly. Do not change the head. \
             The comment must start with this JSON fence (replace RUN with this caller run's numeric id, \
             select decision obsolete, replan, more-rounds or architecture; question is null except for architecture):\n\
             {RESULT_FENCE}{{\"head\":\"{head}\",\"run\":RUN,\"decision\":\"more-rounds\",\"reason\":\"evidence and why\",\"question\":null}}\n```"
        );
        let task = if next.recovery.as_ref().is_some_and(|r| r.granted) {
            format!(
                "{task}\nThis PR already received its one round grant. Do not grant more rounds: close as obsolete, re-plan into a fresh issue, or ask a concrete architectural question."
            )
        } else {
            task
        };
        let apply = self.apply.as_ref().context("triage apply")?;
        let (dispatcher, workflow) = (apply.dispatcher, apply.workflow.clone());
        let start = Start {
            issue_repository: None,
            repository: repo,
            item: number,
            kind: "triage",
            brief: &task,
            pool: pool.as_deref(),
            effort: None,
            runner: None,
            base: None,
        };
        // Reserve before dispatch; ambiguous failures retain the reservation.
        if let Some(run) = self.dispatch(dispatcher, &workflow, &start)? {
            if let Some(recovery) = &mut next.recovery {
                recovery.run = Some(run_number(&run)?);
            }
            self.record_run(
                repo,
                &next,
                &format!("Agent triage: [run]({}). {reason}", run.url),
            )?;
            return Ok(run.url);
        }
        Ok("agent triage dispatched; waiting for run receipt".into())
    }
}
