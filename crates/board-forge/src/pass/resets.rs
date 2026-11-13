//! Banked capacity is a pacing deadline, never permission to reset a provider.
use super::*;
use crate::poll::pace::{Override, OverrideMode};

/// Bounds the deadline calculation and catches an accidentally expanded config.
const RESET_LIMIT: usize = 32;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Reset {
    pub expires: String,
    /// Spend the preceding capacity before the operator plans to press this reset.
    pub use_at: Option<String>,
}

pub(super) fn check(resets: &[Reset]) -> Result<()> {
    ensure!(
        resets.len() <= RESET_LIMIT,
        "at most {RESET_LIMIT} banked resets"
    );
    let mut seen = BTreeSet::new();
    for reset in resets {
        let expires = OffsetDateTime::parse(&reset.expires, &Rfc3339)
            .context("reset expires must be an RFC 3339 time")?;
        ensure!(seen.insert(expires), "duplicate reset expiry");
        if let Some(use_at) = &reset.use_at {
            let use_at = OffsetDateTime::parse(use_at, &Rfc3339)
                .context("reset useAt must be an RFC 3339 time")?;
            ensure!(use_at < expires, "reset useAt must precede expires");
        }
    }
    Ok(())
}

fn pending(resets: &[Reset], now: OffsetDateTime) -> Vec<(&Reset, OffsetDateTime)> {
    let mut pending: Vec<_> = resets
        .iter()
        .filter_map(|reset| {
            let expires = OffsetDateTime::parse(&reset.expires, &Rfc3339).ok()?;
            (expires > now).then_some((reset, expires))
        })
        .collect();
    pending.sort_by_key(|(_, expires)| *expires);
    pending
}

/// Capacity / time, relative to one full window per window length. Natural
/// resets strictly before the deadline each add one window; a reset exactly at
/// the deadline provides no capacity that can be spent before it.
pub(super) fn speed(entry: &Value, resets: &[Reset], now: OffsetDateTime) -> Option<f64> {
    let used = entry["used_percent"]
        .as_f64()
        .filter(|u| (0.0..=100.0).contains(u))?;
    let window = entry["window_minutes"].as_f64()? * 60.0;
    let natural = at(&entry["resets_at"])?;
    if !window.is_finite() || window <= 0.0 || natural <= now {
        return None;
    }
    let rate = |deadline: OffsetDateTime, banked: usize| {
        let seconds = (deadline - now).as_seconds_f64();
        let natural_windows = ((deadline - natural).as_seconds_f64() / window)
            .ceil()
            .max(0.0);
        (1.0 - used / 100.0 + natural_windows + banked as f64) * window / seconds
    };
    let mut factor: f64 = 1.0;
    for (index, (reset, expires)) in pending(resets, now).into_iter().enumerate() {
        factor = factor.max(rate(expires, index + 1));
        if let Some(planned) = reset
            .use_at
            .as_ref()
            .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
            && planned > now
        {
            factor = factor.max(rate(planned, index));
        }
    }
    Some(factor)
}

/// Reuse the multiplied-allowance path. Explicit overrides win, including hold
/// and spend. The shared path preserves exhausted, missing and grant holds.
pub(super) fn apply(config: &Config, mut snapshot: Value, now: OffsetDateTime) -> Value {
    if snapshot["pace_adjusted"] == true {
        return snapshot;
    }
    let mut settings = config.pace.clone();
    let mut automatic = Vec::new();
    let mut annotated = false;
    for (name, pool) in &config.pools.0 {
        let banked = pending(&pool.resets, now);
        if banked.is_empty() {
            continue;
        }
        let entry = &snapshot["pools"][name];
        let computed = speed(entry, &pool.resets, now);
        let active = settings
            .overrides
            .iter()
            .find(|o| o.pool == *name && o.in_force(now));
        let factor = match active.map(|o| o.mode) {
            Some(OverrideMode::Speed(factor)) => Some(factor),
            Some(_) => None,
            None => computed.map(|f| f.max(settings.speed)),
        };
        let note = json!({"count": banked.len(), "first_expires": banked[0].0.expires,
            "speed": factor, "overridden": active.is_some()});
        if active.is_none()
            && let Some(factor) = factor
        {
            settings.overrides.push(Override {
                pool: name.clone(),
                until: banked[0].1,
                mode: OverrideMode::Speed(factor),
                reason: "banked capacity".into(),
            });
            automatic.push(name);
        }
        if snapshot["pools"][name].is_object() {
            snapshot["pools"][name]["banked"] = note;
            annotated = true;
        }
    }
    // Banked capacity only speeds a pool up: where its multiplied allowance
    // would hold a pool the configured pace leaves open (by burst, say),
    // keep that pool as configured.
    let baseline = (!automatic.is_empty()).then(|| config.pace.apply(snapshot.clone(), now));
    let mut snapshot = settings.apply(snapshot, now);
    if let Some(baseline) = baseline {
        for name in automatic {
            let open = &baseline["pools"][name];
            if snapshot["pools"][name]["hold"] == true && open["hold"] == false {
                let banked = snapshot["pools"][name]["banked"].take();
                snapshot["pools"][name] = open.clone();
                snapshot["pools"][name]["banked"] = banked;
            }
        }
    }
    if annotated {
        snapshot["pace_adjusted"] = true.into();
    }
    snapshot
}

fn describe(entry: &Value) -> Option<String> {
    let banked = entry.get("banked")?;
    let speed = match banked["speed"].as_f64() {
        Some(factor) => format!("speed {factor:.1}x"),
        None if banked["overridden"] == true => "explicit override".into(),
        None => "speed unavailable".into(),
    };
    Some(format!(
        "{speed}: {} banked resets, first expires {}",
        banked["count"],
        text(banked, "/first_expires")
    ))
}

pub(super) fn lines(config: &Config, snapshot: &Value, now: OffsetDateTime) -> Vec<String> {
    config
        .pools
        .0
        .iter()
        .filter_map(|(name, _)| {
            let description = describe(&snapshot["pools"][name])?;
            let budget = budget_hold(config, Some(snapshot), now, 1.0)
                .map(|why| format!("; dollar budget holds work: {why}"))
                .unwrap_or_default();
            Some(format!("pools: {name} {description}{budget}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock(s: &str) -> OffsetDateTime {
        OffsetDateTime::parse(s, &Rfc3339).unwrap()
    }

    fn reset(s: &str) -> Reset {
        Reset {
            expires: s.into(),
            use_at: None,
        }
    }

    fn config(resets: Vec<Reset>) -> Config {
        let mut config =
            Config::parse(include_str!("../../../../fixtures/pass/config.json")).unwrap();
        config.pools = Pools(vec![(
            "openai".into(),
            PoolSettings {
                target: 0.95,
                burst: 50.0,
                resets,
            },
        )]);
        config
    }

    #[test]
    fn capacity_rate_table() {
        let now = clock("2026-11-13T00:00:00Z");
        let soon = "2026-11-15T00:00:00Z";
        let week = "2026-11-20T00:00:00Z";
        let late = "2026-11-27T00:00:00Z";
        // name, used, days until natural reset, expiries, expected speed.
        for (name, used, natural_days, expiries, expected) in [
            ("off", 0.0, 7, vec![], 1.0),
            ("one", 0.0, 7, vec![week], 2.0),
            ("remaining half", 50.0, 7, vec![week], 1.5),
            ("no slowdown", 100.0, 7, vec![late], 1.0),
            (
                "two deadlines",
                0.0,
                7,
                vec![week, "2026-11-21T00:00:00Z"],
                3.5,
            ),
            ("earliest binds", 50.0, 7, vec![late, soon], 5.25),
            (
                "later binds",
                0.0,
                7,
                vec![week, "2026-11-21T00:00:00Z", "2026-11-22T00:00:00Z"],
                35.0 / 9.0,
            ),
            ("natural before deadline", 0.0, 2, vec![week], 3.0),
            ("natural exactly at deadline", 0.0, 7, vec![week], 2.0),
            ("multiple naturals", 0.0, 1, vec![late], 2.0),
            (
                "expired excluded",
                0.0,
                7,
                vec!["2026-11-12T00:00:00Z", week],
                2.0,
            ),
            (
                "expires now excluded",
                0.0,
                7,
                vec!["2026-11-13T00:00:00Z"],
                1.0,
            ),
        ] {
            let entry = json!({"used_percent":used, "window_minutes":10080,
                "resets_at": (now + time::Duration::days(natural_days)).format(&Rfc3339).unwrap()});
            let resets: Vec<_> = expiries.into_iter().map(reset).collect();
            let actual = speed(&entry, &resets, now).unwrap();
            assert!(
                (actual - expected).abs() < 1e-9,
                "{name}: {actual} != {expected}"
            );
        }
    }

    #[test]
    fn planned_sunday_spends_current_week_first() {
        let now = clock("2026-11-13T00:00:00Z");
        let entry =
            json!({"used_percent":20, "window_minutes":10080, "resets_at":"2026-11-20T00:00:00Z"});
        let mut resets = vec![
            reset("2026-11-30T20:15:00Z"),
            reset("2026-12-01T22:45:00Z"),
            reset("2026-12-12T22:41:00Z"),
        ];
        let ordinary = speed(&entry, &resets, now).unwrap();
        resets[0].use_at = Some("2026-11-15T00:00:00Z".into());
        let planned = speed(&entry, &resets, now).unwrap();
        assert_eq!(planned, 2.8);
        assert!(planned > ordinary);
        // No extra reset is available before the operator presses the first one.
        assert_ne!(planned, 6.3);
    }

    #[test]
    fn multiplied_allowance_holds_and_overrides_table() {
        let now = clock("2026-11-13T00:00:00Z");
        let baseline = json!({"updated_at":"2026-11-13T00:00:00Z", "pools":{"openai":{
            "source":"praxis", "used_percent":60, "window_minutes":10080,
            "resets_at":"2026-11-16T12:00:00Z", "observed_at":"2026-11-13T00:00:00Z",
            "hold":true, "reason":"pace-only", "allowed_percent":47.5, "burst":0}}});
        for (name, patch, override_mode, expected_factor, held) in [
            ("pace release", json!({}), None, Some(2.4), false),
            (
                "burst stays open",
                json!({"used_percent":75, "resets_at":"2026-11-19T12:00:00Z",
                    "hold":false, "reason":null, "allowed_percent":7.14, "burst":50}),
                None,
                Some(2.25),
                false,
            ),
            (
                "weekly exhausted",
                json!({"used_percent":100}),
                None,
                Some(2.0),
                true,
            ),
            (
                "short window exhausted",
                json!({"reason":"window"}),
                None,
                Some(2.4),
                true,
            ),
            ("grant", json!({"source":"grant"}), None, Some(2.4), true),
            (
                "unread",
                json!({"used_percent":null, "reason":"no-reading"}),
                None,
                None,
                true,
            ),
            (
                "no window",
                json!({"window_minutes":null}),
                None,
                None,
                true,
            ),
            (
                "past natural",
                json!({"resets_at":"2026-11-12T00:00:00Z"}),
                None,
                None,
                true,
            ),
            (
                "speed override wins",
                json!({}),
                Some(OverrideMode::Speed(1.1)),
                Some(1.1),
                true,
            ),
            (
                "hold override wins",
                json!({}),
                Some(OverrideMode::Hold),
                None,
                true,
            ),
            (
                "spend override wins",
                json!({}),
                Some(OverrideMode::Spend),
                None,
                false,
            ),
        ] {
            let mut config = config(vec![reset("2026-11-20T00:00:00Z")]);
            if let Some(mode) = override_mode {
                config.pace.overrides.push(Override {
                    pool: "openai".into(),
                    until: clock("2026-11-14T00:00:00Z"),
                    mode,
                    reason: String::new(),
                });
            }
            let mut snapshot = baseline.clone();
            snapshot["pools"]["openai"]
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            let adjusted = apply(&config, snapshot, now);
            let entry = &adjusted["pools"]["openai"];
            assert_eq!(entry["hold"], held, "{name}: {entry}");
            assert_eq!(entry["banked"]["speed"].as_f64(), expected_factor, "{name}");
            assert_eq!(
                apply(&config, adjusted.clone(), now),
                adjusted,
                "idempotent {name}"
            );
        }
        let config = config(vec![]);
        assert_eq!(apply(&config, baseline.clone(), now), baseline);
    }

    #[test]
    fn reset_config_validation() {
        for value in [
            json!([{"expires":"not a time"}]),
            json!([{"expires":"2026-11-30T20:15:00Z", "extra":1}]),
            json!([{"expires":"2026-11-30T20:15:00Z", "useAt":"bad"}]),
            json!([{"expires":"2026-11-30T20:15:00Z", "useAt":"2026-12-01T00:00:00Z"}]),
            json!([{"expires":"2026-11-30T20:15:00Z"}, {"expires":"2026-11-30T21:15:00+01:00"}]),
            json!(vec![
                json!({"expires":"2026-11-30T20:15:00Z"});
                RESET_LIMIT + 1
            ]),
            Value::Null,
        ] {
            let read = serde_json::from_value::<Vec<Reset>>(value.clone())
                .map_err(anyhow::Error::from)
                .and_then(|r| check(&r));
            assert!(read.is_err(), "{value}");
        }
        check(&[Reset {
            expires: "2026-11-30T20:15:00Z".into(),
            use_at: Some("2026-11-15T00:00:00Z".into()),
        }])
        .unwrap();
    }

    #[test]
    fn config_loader_and_pool_report() {
        let now = clock("2026-11-13T00:00:00Z");
        let mut value: Value =
            serde_json::from_str(include_str!("../../../../fixtures/pass/config.json")).unwrap();
        let omitted = Config::parse(&value.to_string()).unwrap();
        assert!(omitted.pools.0.iter().all(|(_, s)| s.resets.is_empty()));
        value["pools"]["openai"]["resets"] = json!([
            {"expires":"2026-11-30T20:15:00Z", "useAt":"2026-11-15T00:00:00Z"},
            {"expires":"2026-12-01T22:45:00Z"}, {"expires":"2026-12-12T22:41:00Z"}]);
        let config = Config::parse(&value.to_string()).unwrap();
        let baseline = json!({"updated_at":"2026-11-13T00:00:00Z", "pools":{"openai":{
            "source":"praxis", "used_percent":20, "window_minutes":10080,
            "resets_at":"2026-11-19T00:00:00Z", "observed_at":"2026-11-13T00:00:00Z",
            "hold":true, "reason":"pace-only", "allowed_percent":13.57, "burst":0}}});
        let snapshot = apply(&config, baseline, now);
        let reported = lines(&config, &snapshot, now);
        assert_eq!(
            reported,
            ["pools: openai speed 2.8x: 3 banked resets, first expires 2026-11-30T20:15:00Z"]
        );
        assert_eq!(snapshot["pools"]["openai"]["hold"], false);
        let room = room(
            Some(&snapshot),
            "openai",
            config.pool("openai").unwrap(),
            now,
        )
        .unwrap();
        assert!((room - 20.0).abs() < 1e-9, "{room}");
        for invalid in [
            Value::Null,
            json!([{"expires":"bad"}]),
            json!(vec![json!({"expires":"2026-11-30T20:15:00Z"}); 2]),
        ] {
            value["pools"]["openai"]["resets"] = invalid;
            assert!(Config::parse(&value.to_string()).is_err());
        }
    }
}
