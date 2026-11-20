//! Diagnostic-only catalog. See docs/pass.md#supported-features-left-off.
use super::*;

#[derive(Debug, Default)]
pub(super) struct Off {
    unconfigured: BTreeSet<String>,
    disabled: BTreeSet<String>,
}

impl Off {
    pub(super) fn summary(&self) -> String {
        let names = |set: &BTreeSet<String>| {
            if set.is_empty() {
                "none".to_owned()
            } else {
                set.iter().cloned().collect::<Vec<_>>().join(", ")
            }
        };
        format!(
            "features off: unconfigured [{}]; disabled [{}]",
            names(&self.unconfigured),
            names(&self.disabled)
        )
    }
}

#[derive(Clone, Copy)]
enum Activation {
    Bool,
    Positive,
    Nonempty,
    Present,
    Triage,
}

impl Activation {
    fn enabled(self, value: &Value) -> bool {
        match self {
            Self::Bool => value == true,
            Self::Positive => value.as_u64().is_some_and(|n| n > 0),
            Self::Nonempty => match value {
                Value::Array(v) => !v.is_empty(),
                Value::Object(v) => !v.is_empty(),
                _ => false,
            },
            Self::Present => true,
            Self::Triage => value == true || value.is_object(),
        }
    }
}

pub(super) fn classify(source: &Value) -> Result<Off> {
    use Activation::*;
    // Only implemented features, not required identities, destinations, pool
    // preferences, paths, ordinary caps, or tuning with working defaults.
    let mut catalog = BTreeMap::new();
    for name in [
        "notifications",
        "issueIntake",
        "reserveScheduling",
        "poolHeadroom",
        "skipActiveWork",
        "idleReplan",
        "existingChildPlans",
        "researchPlans",
        "fixPlans",
        "sessionPlans",
        "triageClose",
        "replanInvalid",
        "operatorQuestionsOnly",
        "reserveOperator",
        "assignHolds",
        "readFreeHolds",
        "leanReads",
        "bodyPlans",
        "leases",
        "reruns",
        "reapOverdueRuns",
        "cleanupHistory",
        "topDown",
        "usage.probe",
        "holds.needsYou",
        "holds.approvalEvidence",
        "holds.actNow",
        "holds.passView",
        "holds.passes",
        "holds.today",
        "holds.epicFocus",
        "epics.close",
        "epics.inWorkRepositories",
        "sessions.apply",
        "promote.onApproval",
        "revisit.repairQuestions",
    ] {
        catalog.insert(name.to_owned(), Bool);
    }
    for name in [
        "authored",
        "openPrCap",
        "triageFollowUpDays",
        "triageReplans",
        "replanAfterDays",
        "changeSweepHours",
        "epics.heldPlanHours",
    ] {
        catalog.insert(name.to_owned(), Positive);
    }
    for name in ["mix", "follow"] {
        catalog.insert(name.to_owned(), Nonempty);
    }
    for name in [
        "midstream",
        "revisit",
        "forgejoIntake",
        "sweep",
        "sessions",
        "maintain",
        "overlap",
        "holds",
        "holds.patExpiry",
        "holds.proposals",
        "holds.drafts",
        "decisions",
        "starts",
        "direction",
        "cost",
        "requestShare",
        "requestWindow",
        "epics",
        "epics.focus",
        "store",
        "livingStatus",
        "sessions.compareAfter",
    ] {
        catalog.insert(name.to_owned(), Present);
    }
    catalog.insert("triage".to_owned(), Triage);

    // Share the poll's typed option metadata, but not its configuration/tuning
    // fields. quietWaits has a working default; directionFirst is inert in pass.
    let options = serde_json::to_value(poll::PrOptions::default())?;
    for (name, default) in options.as_object().context("PR option metadata")? {
        ensure!(
            poll::POLL_OPTIONS.contains(&name.as_str()),
            "unknown PR option {name}"
        );
        if default == false && name != "directionFirst" {
            catalog.insert(format!("pr.{name}"), Bool);
        }
    }
    for (name, activation) in [
        ("pr.rebaseRounds", Positive),
        ("pr.staleDays", Positive),
        ("pr.mergeQueue", Present),
        ("pr.reviewPools", Nonempty),
        ("pr.reviewDispatchRepositories", Nonempty),
        ("pr.maintainers", Nonempty),
        ("pr.draftReviewRepositories", Nonempty),
        ("pr.operatorPaths", Nonempty),
    ] {
        catalog.insert(name.to_owned(), activation);
    }

    let forgejo = source["board"]["forge"] == "forgejo";
    // Positive allowlist of implemented Forgejo features: accepting a field is
    // not evidence that this forge implements it (notably PR follow-up).
    const FORGEJO: &[&str] = &[
        "triage",
        "triageFollowUpDays",
        "triageClose",
        "leases",
        "reruns",
        "revisit",
        "forgejoIntake",
        "sweep",
        "holds",
        "holds.needsYou",
        "epics",
        "epics.close",
        "epics.inWorkRepositories",
    ];
    let mut off = Off::default();
    for (name, activation) in catalog {
        if (forgejo && !FORGEJO.contains(&name.as_str())) || (!forgejo && name == "forgejoIntake") {
            continue;
        }
        let pointer = format!("/{}", name.replace('.', "/"));
        match source.pointer(&pointer) {
            None | Some(Value::Null) => {
                off.unconfigured.insert(name);
            }
            Some(value) if !activation.enabled(value) => {
                off.disabled.insert(name);
            }
            _ => {}
        }
    }
    Ok(off)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_catalog_semantics() {
        for (value, enabled) in [
            (json!(false), false),
            (json!(true), true),
            (json!({}), true),
        ] {
            assert_eq!(Activation::Triage.enabled(&value), enabled);
        }
        let github = classify(&json!({})).unwrap();
        assert!(github.unconfigured.contains("openPrCap"));
        assert!(github.unconfigured.contains("pr.agentRecovery"));
        assert!(!github.unconfigured.contains("pr.quietWaits"));
        assert!(!github.unconfigured.contains("pr.fixRounds"));
        assert!(!github.unconfigured.contains("forgejoIntake"));
        let forgejo = classify(&json!({"board": {"forge": "forgejo"}})).unwrap();
        assert!(forgejo.unconfigured.contains("forgejoIntake"));
        assert!(forgejo.unconfigured.contains("triage"));
        assert!(!forgejo.unconfigured.iter().any(|n| n.starts_with("pr.")));
        assert!(!forgejo.unconfigured.contains("openPrCap"));
        let mut source = json!({"operator": "op", "bot": "bot", "dispatchRepo": "o/caller",
            "dispatchWorkflow": "dispatch.yml", "board": {"forge": "forgejo",
            "url": "https://forge.example/", "tokenEnv": "FORGEJO_TOKEN",
            "repositories": ["o/r"], "membership": "open", "recorders": [1, 2]}});
        let summary = Config::parse(&source.to_string())
            .unwrap()
            .off_feature_summary();
        assert_eq!(
            summary,
            "features off: unconfigured [epics, epics.close, epics.inWorkRepositories, forgejoIntake, holds, holds.needsYou, leases, reruns, revisit, sweep, triage, triageClose, triageFollowUpDays]; disabled [none]"
        );
        source["holds"] = json!({"repository": "o/private", "issue": 1, "passView": false});
        assert!(
            format!("{:#}", Config::parse(&source.to_string()).unwrap_err())
                .contains("unsupported_capability: holds.passView")
        );
        assert!(!github.unconfigured.contains("promote.publish"));
        // Defensive classifier behavior doesn't weaken Config's no-null rule.
        assert!(
            classify(&json!({"openPrCap": null}))
                .unwrap()
                .unconfigured
                .contains("openPrCap")
        );
        let on = classify(&json!({"triage": {}, "overlap": {}, "mix": {"variant": {}}, "pr": {"agentRecovery": true}})).unwrap();
        for name in ["triage", "overlap", "mix", "pr.agentRecovery"] {
            assert!(!on.unconfigured.contains(name) && !on.disabled.contains(name));
        }
    }
}
