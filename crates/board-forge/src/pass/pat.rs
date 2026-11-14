//! Optional credential-health projection. No probe, secret access or rotation writes.
use super::holds::{self, Ask, Collected, Read, RefKind, Reference};
use anyhow::{Result, ensure};
use board_core::ItemRef;
use serde::Deserialize;
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

pub(super) const SOURCE: &str = "pat-expiry";
const THRESHOLDS: [i64; 3] = [1, 7, 30];
const SETTINGS: &str = "https://github.com/settings/personal-access-tokens";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rotation {
    pub repository: String,
    pub secret: String,
}

impl Rotation {
    pub(super) fn check(&self) -> Result<()> {
        crate::poll::validate_repository(&self.repository)?;
        // Threat: trusted configuration accidentally producing a shell command
        // containing metacharacters when the operator copies the recommendation.
        ensure!(
            (1..=100).contains(&self.secret.len())
                && self
                    .secret
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                && !self.secret.starts_with(|c: char| c.is_ascii_digit()),
            "holds patExpiry secret must be a shell-inert uppercase secret name (1–100 characters)"
        );
        Ok(())
    }
}

fn threshold(expiry: OffsetDateTime, now: OffsetDateTime) -> Option<i64> {
    let remaining = expiry - now;
    THRESHOLDS.into_iter().find(|days| {
        if *days == 30 {
            remaining < Duration::days(*days)
        } else {
            remaining <= Duration::days(*days)
        }
    })
}

pub(super) fn status(
    rotation: &Rotation,
    expiry: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Value {
    json!({
        "expires_at": expiry.and_then(|e| e.format(&Rfc3339).ok()),
        "threshold_days": expiry.and_then(|e| threshold(e, now)),
        "repository": rotation.repository,
        "secret": rotation.secret,
        "settings_url": SETTINGS,
    })
}

pub(super) fn asks(
    rotation: &Rotation,
    expiry: Option<OffsetDateTime>,
    now: OffsetDateTime,
    on: &ItemRef,
) -> Collected {
    let mut collected = Collected::of(Read::Complete);
    let Some((expiry, days)) = expiry.and_then(|e| threshold(e, now).map(|days| (e, days))) else {
        return collected;
    };
    let expiry = expiry.format(&Rfc3339).unwrap_or_default();
    let action = Reference::new(RefKind::Issue, &on.repository, on.number);
    // One current card, with a stable identity until the next threshold. A new
    // expiry after rotation retires it. No per-pass event or growing receipt log.
    collected.asks.push(Ask {
        key: format!("{}/pat-expiry/{expiry}/{days}", on.display().to_ascii_lowercase()),
        item: on.display(),
        source: SOURCE.into(),
        kind: "operator-action".into(),
        priority: Some(if days == 1 { "P0" } else { "P1" }.into()),
        waiting_since: now.format(&Rfc3339).unwrap_or_default(),
        ask: format!("Rotate {}: bot PAT expires {expiry} (under {days} days remaining).", rotation.secret),
        recommendation: Some(format!("Log in as the bot at {SETTINGS}, mint a replacement, then run `gh secret set {} -R {}` (reads stdin).", rotation.secret, rotation.repository)),
        target: action.url(),
        head: None,
        source_ref: action.clone(),
        action,
        carried: false,
        group: None,
        chain: holds::EpicChain::default(),
    });
    collected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_are_one_stable_operator_action_each() {
        let expiry = OffsetDateTime::parse("2026-12-31T00:00:00Z", &Rfc3339).unwrap();
        let rotation = Rotation {
            repository: "cgwalters-forge/tracker".into(),
            secret: "CGWALTERS_BOT_PAT".into(),
        };
        let on = ItemRef {
            host: "github.com".into(),
            repository: "example/private".into(),
            number: 1,
        };
        let mut keys = std::collections::BTreeSet::new();
        for (hours, expected) in [
            (1440, None),
            (720, None),
            (719, Some(30)),
            (169, Some(30)),
            (167, Some(7)),
            (25, Some(7)),
            (23, Some(1)),
            (0, Some(1)),
            (-1, Some(1)),
        ] {
            let now = expiry - Duration::hours(hours);
            assert_eq!(threshold(expiry, now), expected);
            let collected = asks(&rotation, Some(expiry), now, &on);
            assert_eq!(collected.asks.len(), usize::from(expected.is_some()));
            if let Some(ask) = collected.asks.first() {
                assert_eq!(ask.kind, "operator-action");
                assert!(ask.ask.contains("2026-12-31"));
                assert!(
                    ask.recommendation
                        .as_ref()
                        .unwrap()
                        .contains("gh secret set CGWALTERS_BOT_PAT -R cgwalters-forge/tracker")
                );
                keys.insert(ask.key.clone());
                assert_eq!(
                    ask.key,
                    asks(&rotation, Some(expiry), now + Duration::seconds(1), &on).asks[0].key
                );
            }
        }
        assert_eq!(keys.len(), 3);
        assert!(asks(&rotation, None, expiry, &on).asks.is_empty());
        assert!(
            asks(&rotation, Some(expiry + Duration::days(60)), expiry, &on)
                .asks
                .is_empty()
        );
        assert!(status(&rotation, None, expiry)["expires_at"].is_null());
    }

    #[test]
    fn publication_repeats_only_at_thresholds_and_clears_on_rotation() {
        let expiry = OffsetDateTime::parse("2026-12-31T00:00:00Z", &Rfc3339).unwrap();
        let rotation = Rotation {
            repository: "cgwalters-forge/tracker".into(),
            secret: "CGWALTERS_BOT_PAT".into(),
        };
        let on = ItemRef {
            host: "github.com".into(),
            repository: "example/private".into(),
            number: 1,
        };
        let project =
            |now: OffsetDateTime, token: Option<OffsetDateTime>, previous: Option<&Value>| {
                let mut snapshot =
                    holds::snapshot(&[], now.format(&Rfc3339).unwrap(), Default::default());
                snapshot.pass.candidates = Some(0);
                snapshot.pat_expiry = Some(status(&rotation, token, now));
                holds::carry(&mut snapshot, previous, (false, true));
                let reading = holds::Reading {
                    operator: "operator",
                    epics: false,
                    considered: true,
                    epics_whole: true,
                    options: &[],
                    on: &on,
                    reviews: Collected::off(),
                    proposals: Collected::off(),
                    digest: Collected::off(),
                    inbox: [(SOURCE.into(), asks(&rotation, token, now, &on))].into(),
                    observed: holds::Observed::off(),
                    summaries: (Read::Off, Default::default()),
                };
                snapshot.needs_you = Some(holds::needs_you(&snapshot, previous, &reading));
                snapshot
            };
        let mut previous = None;
        let mut identities = std::collections::BTreeSet::new();
        for (hours, days) in [(719, 30), (718, 30), (168, 7), (167, 7), (24, 1), (23, 1)] {
            let now = expiry - Duration::hours(hours);
            let snapshot = project(now, Some(expiry), previous.as_ref());
            let list = snapshot.needs_you.as_ref().unwrap();
            assert_eq!(
                (list.count, list.items.len(), list.requests.len()),
                (1, 1, 1)
            );
            assert!(list.complete);
            assert_eq!(list.requests[0].quality, holds::requests::DECISION);
            let action = list.requests[0]
                .actions
                .iter()
                .find(|a| a.kind == "rotate-token")
                .unwrap();
            assert!(!action.supported);
            identities.insert(list.requests[0].identity.clone());
            let (_, body) = holds::fit(snapshot).unwrap();
            let published = holds::parse(&body).unwrap();
            assert_eq!(published["pat_expiry"]["threshold_days"], days);
            let again = project(now + Duration::seconds(30), Some(expiry), Some(&published));
            let list = again.needs_you.as_ref().unwrap();
            assert_eq!(
                list.items[0].waiting_since,
                published["needs_you"]["items"][0]["waiting_since"]
            );
            assert_eq!(list.changed_at, published["needs_you"]["changed_at"]);
            assert!(!holds::changed(Some(&body), &again).unwrap());
            let mut evicted = published.clone();
            evicted["needs_you"]["items"] = json!([]);
            let restored = project(now + Duration::seconds(30), Some(expiry), Some(&evicted));
            assert_eq!(
                restored.needs_you.unwrap().items[0].waiting_since,
                list.items[0].waiting_since
            );
            previous = Some(published);
        }
        assert_eq!(identities.len(), 3);
        let now = expiry - Duration::hours(22);
        for token in [None, Some(expiry + Duration::days(60))] {
            assert_eq!(
                project(now, token, previous.as_ref())
                    .needs_you
                    .unwrap()
                    .count,
                0
            );
        }
    }

    #[test]
    fn configuration_is_opt_in_and_command_is_shell_inert() {
        let base = include_str!("../../../../fixtures/pass/config.json");
        let mut config: Value = serde_json::from_str(base).unwrap();
        assert!(
            super::super::Config::parse(&config.to_string())
                .unwrap()
                .holds
                .is_none_or(|h| h.pat_expiry.is_none())
        );
        config["holds"] = json!({"repository":"example/private", "issue":1, "needsYou":true});
        config["holds"]["patExpiry"] =
            json!({"repository":"cgwalters-forge/tracker", "secret":"CGWALTERS_BOT_PAT"});
        config["holds"]["needsYou"] = json!(true);
        assert!(super::super::Config::parse(&config.to_string()).is_ok());
        config["holds"]["needsYou"] = json!(false);
        assert!(super::super::Config::parse(&config.to_string()).is_err());
        for secret in ["", "X;evil", "X Y", "1X", "x", &"X".repeat(101)] {
            assert!(
                Rotation {
                    repository: "a/b".into(),
                    secret: secret.into()
                }
                .check()
                .is_err()
            );
        }
    }
}
