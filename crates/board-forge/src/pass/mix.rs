//! Opt-in model sampling. Variants share their account's pace; they never
//! acquire a separate allowance or enter the ordinary automatic pool list.
use super::*;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setting {
    pub share: f64,
    pub when: Vec<String>,
}

/// These are caller pool names, not model identifiers or broker addresses.
pub(super) fn account(pool: &str) -> Option<&'static str> {
    match pool {
        "fable" => Some("claude"),
        "astra" => Some("openai"),
        _ => None,
    }
}

pub(super) fn validate(config: &Config) -> Result<()> {
    let mut total = 0.0;
    for (pool, setting) in &config.mix {
        let parent = account(pool).with_context(|| format!("mix names unsupported pool {pool}"))?;
        ensure!(
            config.pool(parent).is_some(),
            "mix.{pool} needs pools.{parent}"
        );
        ensure!(
            config.pool(pool).is_none(),
            "mix.{pool} must not also be in pools"
        );
        ensure!(
            setting.share.is_finite() && setting.share > 0.0 && setting.share <= 1.0,
            "mix.{pool}.share must be above 0 and at most 1"
        );
        ensure!(
            !setting.when.is_empty()
                && setting
                    .when
                    .iter()
                    .all(|s| matches!(s.as_str(), "P0" | "review:deep" | "epic")),
            "mix.{pool}.when must contain P0, review:deep or epic"
        );
        total += setting.share;
    }
    ensure!(total <= 1.0, "mix shares must sum to at most 1");
    Ok(())
}

pub(super) fn select(
    config: &Config,
    snapshot: Option<&Value>,
    facts: &Facts,
    epic: bool,
    fallback: String,
    now: OffsetDateTime,
) -> String {
    select_with(
        config,
        snapshot,
        (
            facts.priority.as_deref(),
            facts.labels_complete.then_some(facts.labels.as_slice()),
            epic,
        ),
        fallback,
        now,
        crate::pool_choice::index,
    )
}

pub(super) fn planning(
    config: &Config,
    snapshot: Option<&Value>,
    facts: &epics::EpicFacts,
    fallback: String,
    now: OffsetDateTime,
) -> String {
    select_with(
        config,
        snapshot,
        (facts.priority.as_deref(), Some(&facts.labels), true),
        fallback,
        now,
        crate::pool_choice::index,
    )
}

// One draw over disjoint intervals: an unavailable variant's share falls
// back, rather than increasing another variant's share. No retry loop.
fn select_with(
    config: &Config,
    snapshot: Option<&Value>,
    (priority, labels, epic): (Option<&str>, Option<&[String]>, bool),
    fallback: String,
    now: OffsetDateTime,
    mut draw: impl FnMut(usize) -> std::result::Result<usize, String>,
) -> String {
    const TICKETS: usize = 1_000_000;
    if config.mix.is_empty() {
        return fallback;
    }
    let Ok(ticket) = draw(TICKETS) else {
        return fallback;
    };
    let point = ticket as f64 / TICKETS as f64;
    let mut end = 0.0;
    for (pool, setting) in &config.mix {
        end += setting.share;
        if point >= end {
            continue;
        }
        let matches = setting
            .when
            .iter()
            .any(|condition| match condition.as_str() {
                "P0" => priority == Some("P0"),
                "epic" => {
                    epic || labels
                        .is_some_and(|ls| ls.iter().any(|l| l.eq_ignore_ascii_case(EPIC_LABEL)))
                }
                "review:deep" => labels
                    .is_some_and(|ls| ls.iter().any(|l| l.eq_ignore_ascii_case("review:deep"))),
                _ => false,
            });
        let under = account(pool)
            .and_then(|parent| config.pool(parent).map(|s| (parent, s)))
            .is_some_and(|(parent, settings)| {
                room(snapshot, parent, settings, now).is_ok_and(|r| r > 0.0)
            });
        return if matches && under {
            pool.clone()
        } else {
            fallback
        };
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_shares_require_positive_parent_room() {
        let mut replay = super::super::tests::replay();
        let snapshot = super::super::tests::usage(&mut replay);
        let now = replay.now;
        let config = super::super::tests::config(json!({"mix": {
            "fable": {"share": 0.25, "when": ["P0", "review:deep", "epic"]},
            "astra": {"share": 0.25, "when": ["P0"]}
        }}));
        let plain = super::super::tests::facts("o/r", 1, "2026-10-23T00:00:00Z");
        let mut hard = plain.clone();
        hard.priority = Some("P0".into());
        let mut deep = plain.clone();
        deep.labels.push("review:deep".into());
        let mut incomplete = deep.clone();
        incomplete.labels_complete = false;
        // BTreeMap order is astra, fable. astra's parent is held in this snapshot.
        for (facts, epic, ticket, expected) in [
            (&hard, false, 0, "claude"),
            (&hard, false, 250_000, "fable"),
            (&hard, false, 499_999, "fable"),
            (&hard, false, 500_000, "claude"),
            (&plain, false, 250_000, "claude"),
            (&deep, false, 250_000, "fable"),
            (&incomplete, false, 250_000, "claude"),
            (&plain, true, 250_000, "fable"),
        ] {
            assert_eq!(
                select_with(
                    &config,
                    Some(&snapshot),
                    (
                        facts.priority.as_deref(),
                        facts.labels_complete.then_some(facts.labels.as_slice()),
                        epic
                    ),
                    "claude".into(),
                    now,
                    |_| Ok(ticket)
                ),
                expected
            );
        }
        for state in ["missing", "held", "ahead", "equal", "stale"] {
            let mut reading = snapshot.clone();
            match state {
                "missing" => reading["pools"]["claude"] = Value::Null,
                "held" => reading["pools"]["claude"]["hold"] = true.into(),
                "ahead" => reading["pools"]["claude"]["used_percent"] = 99.into(),
                "equal" => {
                    let entry = &reading["pools"]["claude"];
                    reading["pools"]["claude"]["used_percent"] = allowed(
                        config.pool("claude").unwrap().target,
                        entry["window_minutes"].as_f64().unwrap(),
                        at(&entry["resets_at"]).unwrap(),
                        now,
                    )
                    .into();
                }
                _ => reading["updated_at"] = "2020-01-01T00:00:00Z".into(),
            }
            assert_eq!(
                select_with(
                    &config,
                    Some(&reading),
                    (Some("P0"), None, false),
                    "openai".into(),
                    now,
                    |_| Ok(250_000)
                ),
                "openai",
                "{state}"
            );
        }
        assert_eq!(
            select_with(
                &config,
                Some(&snapshot),
                (Some("P0"), None, false),
                "claude".into(),
                now,
                |_| Err("no entropy".into())
            ),
            "claude"
        );
    }

    #[test]
    fn invalid_mix_is_refused() {
        let base: Value =
            serde_json::from_str(include_str!("../../../../fixtures/pass/config.json")).unwrap();
        for mix in [
            json!({"unknown": {"share": 0.2, "when": ["P0"]}}),
            json!({"fable": {"share": 0, "when": ["P0"]}}),
            json!({"fable": {"share": 1.1, "when": ["P0"]}}),
            json!({"fable": {"share": 0.2, "when": []}}),
            json!({"fable": {"share": 0.2, "when": ["title:hard"]}}),
            json!({"fable": {"share": 0.6, "when": ["P0"]}, "astra": {"share": 0.6, "when": ["P0"]}}),
            json!({"fable": {"share": 0.2, "when": ["P0"], "extra": true}}),
        ] {
            let mut value = base.clone();
            value["mix"] = mix.clone();
            assert!(Config::parse(&value.to_string()).is_err(), "{mix}");
        }
    }
}
