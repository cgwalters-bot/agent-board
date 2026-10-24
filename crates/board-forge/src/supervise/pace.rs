//! The supervisor workflow's pacing, moved from tracker's
//! `supervisor-due.cjs` and `supervisor-repeat.cjs`: whether a pass is a
//! sweep ([`due`]), and whether a pass that is not leaves the agent out
//! because nothing newly fails ([`unchanged`]).
//!
//! `supervisor-due.cjs` also rate-limited health passes that the poll's
//! chain started (tracker#730, #763). The caller's own crons start every
//! pass now (tracker#898), so each run is a health pass and only the sweep
//! is paced here.
use super::{HOUR_MS, iso, millis};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// How long after one sweep the next is due. GitHub's cron has started one
/// run in two hours where it was asked for eight (tracker#730), so the sweep
/// is paced from the time the previous run recorded, not from which cron
/// fired.
pub const SWEEP_MS: i64 = 2 * HOUR_MS;

/// Whether this pass is a sweep, `daily`: the trigger then bypasses the
/// failure cooldown. An explicit request (an operator's comment, a dispatch
/// with a prompt) always is; otherwise a sweep is due two hours after the
/// last. `previous` is the previous run's times, `None` when they could not
/// be looked up: unknown is not absent, and taking it as no previous sweep
/// would make every pass a sweep without a cooldown (tracker#814). A
/// missing, unreadable or future time, where the lookup worked, is no
/// previous sweep. Returns `daily` and the times this run records for the
/// next.
pub fn due(event: &str, prompt: &str, previous: Option<&Value>, now: i64) -> (bool, Value) {
    let explicit = event == "issue_comment" || (event == "workflow_dispatch" && !prompt.is_empty());
    let last = previous
        .and_then(|p| millis(&p["sweep"]))
        .filter(|at| *at <= now);
    let daily = explicit || (previous.is_some() && last.is_none_or(|at| now - at >= SWEEP_MS));
    let sweep = if daily { Some(now) } else { last };
    (daily, json!({"sweep": sweep.map(iso)}))
}

/// A check that cannot clear (a probe switched off, a pin waiting on a
/// review) fails every pass, so the trigger started an agent every cooldown
/// to read the same report (tracker#804). With `agent.skipUnchangedFailures`
/// a pass that is not a sweep leaves the agent out unless a check fails that
/// did not fail in the previous report: ids, not evidence, which changes
/// every pass for some checks. What this holds back, new evidence on a
/// failing check, the next sweep reads.
///
/// `Some` with the failing ids when the trigger is left out; never on a
/// sweep, nor without two reports that are lists: one this cannot read is
/// left to the trigger.
pub fn unchanged(
    enabled: bool,
    daily: bool,
    current: Option<&Value>,
    previous: Option<&Value>,
) -> Option<BTreeSet<String>> {
    let failing = |report: &Value| -> Option<BTreeSet<String>> {
        Some(
            report
                .as_array()?
                .iter()
                .filter(|c| c["status"] == "fail")
                .filter_map(|c| c["id"].as_str().map(str::to_owned))
                .collect(),
        )
    };
    if !enabled || daily {
        return None;
    }
    let now = failing(current?)?;
    let before = failing(previous?)?;
    now.is_subset(&before).then_some(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_760_000_000_000;

    fn ago(minutes: i64) -> Value {
        json!(iso(NOW - minutes * 60_000))
    }

    #[test]
    fn a_sweep_is_due_two_hours_after_the_last_or_on_request() {
        let times = |sweep: Value| json!({"health": ago(5), "sweep": sweep});
        // (event, prompt, previous times or None when not looked up, daily)
        let cases: &[(&str, &str, Option<Value>, bool)] = &[
            ("schedule", "", Some(times(ago(60))), false),
            ("schedule", "", Some(times(ago(119))), false),
            ("schedule", "", Some(times(ago(120))), true),
            // A prompt-less dispatch is a health pass, not a request.
            ("workflow_dispatch", "", Some(times(ago(60))), false),
            ("workflow_dispatch", "a request", Some(times(ago(60))), true),
            ("issue_comment", "", Some(times(ago(60))), true),
            // No previous run, no times file, an unreadable or future time:
            // no previous sweep.
            ("schedule", "", Some(Value::Null), true),
            ("schedule", "", Some(json!({})), true),
            ("schedule", "", Some(times(json!("yesterday"))), true),
            ("schedule", "", Some(times(ago(-10))), true),
            // Times that could not be looked up are unknown: only a request
            // is a sweep.
            ("schedule", "", None, false),
            ("workflow_dispatch", "", None, false),
            ("workflow_dispatch", "a request", None, true),
            ("issue_comment", "", None, true),
        ];
        for (event, prompt, previous, daily) in cases {
            let (got, times) = due(event, prompt, previous.as_ref(), NOW);
            assert_eq!(got, *daily, "{event} {prompt:?} {previous:?}");
            // A sweep records itself; otherwise the last valid sweep stays.
            let kept = previous
                .as_ref()
                .and_then(|p| millis(&p["sweep"]))
                .filter(|at| *at <= NOW)
                .map(iso);
            let expected = if *daily { Some(iso(NOW)) } else { kept };
            assert_eq!(times, json!({"sweep": expected}), "{event} {previous:?}");
        }
    }

    #[test]
    fn standing_failures_leave_the_agent_to_the_sweep_only_when_enabled() {
        let report = |ids: &[&str]| -> Value {
            let mut checks: Vec<Value> = ids
                .iter()
                .map(|id| json!({"id": id, "status": "fail"}))
                .collect();
            checks.push(json!({"id": "idle", "status": "warn"}));
            Value::Array(checks)
        };
        let standing = report(&["broker", "release"]);
        let set = |ids: &[&str]| Some(ids.iter().map(|s| (*s).to_owned()).collect());
        type Case = (
            bool,
            bool,
            Option<Value>,
            Option<Value>,
            Option<BTreeSet<String>>,
        );
        // (enabled, daily, current, previous, result)
        let cases: &[Case] = &[
            (
                true,
                false,
                Some(standing.clone()),
                Some(standing.clone()),
                set(&["broker", "release"]),
            ),
            // Fewer failures than before is nothing new.
            (
                true,
                false,
                Some(report(&["broker"])),
                Some(standing.clone()),
                set(&["broker"]),
            ),
            (
                true,
                false,
                Some(report(&[])),
                Some(standing.clone()),
                set(&[]),
            ),
            // A new failure goes to the trigger.
            (
                true,
                false,
                Some(report(&["broker", "held"])),
                Some(standing.clone()),
                None,
            ),
            // Off, or a sweep.
            (
                false,
                false,
                Some(standing.clone()),
                Some(standing.clone()),
                None,
            ),
            (
                true,
                true,
                Some(standing.clone()),
                Some(standing.clone()),
                None,
            ),
            // A report that is missing or not a list is left to the trigger.
            (true, false, Some(standing.clone()), None, None),
            (
                true,
                false,
                Some(standing.clone()),
                Some(json!({"not": "a list"})),
                None,
            ),
            (true, false, None, Some(standing.clone()), None),
            (
                true,
                false,
                Some(json!({"checks": []})),
                Some(standing.clone()),
                None,
            ),
        ];
        for (enabled, daily, current, previous, result) in cases {
            assert_eq!(
                unchanged(*enabled, *daily, current.as_ref(), previous.as_ref()),
                *result,
                "{enabled} {daily} {current:?} {previous:?}"
            );
        }
    }
}
