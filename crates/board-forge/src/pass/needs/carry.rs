//! Cheap carry retirement is evidence, not a second scheduling decision.
use super::*;

/// State is sufficient to retire a closed target. Labels prove clearance
/// only for the particular source that actually depends on that label.
fn retired(source: &str, kind: &str, item: &str, nodes: &BTreeMap<String, Value>) -> bool {
    let Some(node) = nodes.get(&item.to_ascii_lowercase()) else {
        return false;
    };
    if matches!(node["state"].as_str(), Some("CLOSED" | "MERGED")) {
        return source != "pass";
    }
    if node["state"] != "OPEN" {
        return false;
    }
    let label = match (source, kind) {
        ("items" | "epics", "needs-human") => "needs-human",
        ("items", "pr-open") => "needs-operator-approval",
        (holds::DRAFTS, _) => "review-draft",
        // Poll stops, digest recommendations and proposals have their own
        // policy/evidence. Absence of needs-human says nothing about them.
        _ => return false,
    };
    epics::labels(node).is_some_and(|labels| !labels.iter().any(|l| l == label))
}

fn trim_ask(ask: &mut Ask, nodes: &BTreeMap<String, Value>) -> bool {
    if let Some(group) = &mut ask.group {
        group.numbers.retain(|n| {
            !retired(
                &ask.source,
                &ask.kind,
                &format!("{}#{n}", ask.action.repository),
                nodes,
            )
        });
        let Some(number) = group.numbers.iter().min().copied() else {
            return false;
        };
        if number != ask.action.number {
            ask.action.number = number;
            ask.source_ref.number = number;
            ask.item = format!("{}#{number}", ask.action.repository);
            ask.key = ask.item.to_ascii_lowercase();
            ask.head = None;
            ask.action.head = None;
            ask.source_ref.head = None;
        }
        true
    } else {
        !retired(&ask.source, &ask.kind, &ask.item, nodes)
    }
}

/// Filter before reconciliation so cards, semantic requests, membership and
/// counts are all rebuilt together, including detail-evicted identities.
pub(super) fn retire(
    snapshot: &mut holds::Snapshot,
    previous: Option<&Value>,
    nodes: &BTreeMap<String, Value>,
) -> Option<Value> {
    snapshot.holds.retain(|h| {
        !h.item.as_ref().is_some_and(|item| {
            retired(
                if h.epic { "epics" } else { "items" },
                // Only an operator approval hold depends on this label;
                // ordinary open-PR waits are machine work, not clearance.
                if h.carried && h.operator {
                    h.kind.name()
                } else {
                    ""
                },
                item,
                nodes,
            )
        })
    });
    let mut previous = previous?.clone();
    let list = &mut previous["needs_you"];
    if let Some(items) = list["items"].as_array_mut() {
        items.retain_mut(|value| {
            let Ok(mut ask) = serde_json::from_value::<Ask>(value.clone()) else {
                return false;
            };
            if !trim_ask(&mut ask, nodes) {
                return false;
            }
            *value = json!(ask);
            true
        });
    }
    if let Some(sources) = list["request_sources"].as_array_mut() {
        sources.retain_mut(|value| {
            let Ok(mut source) =
                serde_json::from_value::<holds::requests::SourceAsk>(value.clone())
            else {
                return false;
            };
            let old_key = source.ask.key.clone();
            if !trim_ask(&mut source.ask, nodes) {
                return false;
            }
            if source.key == old_key {
                source.key = source.ask.key.clone();
            }
            *value = json!(source);
            true
        });
    }
    if let Some(identities) = list["identities"].as_array_mut() {
        identities.retain(|value| {
            serde_json::from_value::<holds::Identity>(value.clone())
                .is_ok_and(|i| !retired(&i.source, &i.kind, &i.item, nodes))
        });
    }
    Some(previous)
}

/// An old digest/comment can be read afresh without its target reopening.
/// Apply target evidence to incoming asks as well as carry state.
pub(super) fn sources(reading: &mut holds::Reading<'_>, nodes: &BTreeMap<String, Value>) {
    for collected in [
        &mut reading.reviews,
        &mut reading.proposals,
        &mut reading.digest,
    ]
    .into_iter()
    .chain(reading.inbox.values_mut())
    {
        collected.asks.retain_mut(|ask| trim_ask(ask, nodes));
    }
}

pub(super) fn evidence(
    list: &mut NeedsYou,
    previous: Option<&Value>,
    nodes: &BTreeMap<String, Value>,
) {
    for identity in list.identities.iter().filter(|i| i.carried) {
        let item = identity.item.to_ascii_lowercase();
        let checked = nodes.get(&item).is_some_and(|node| {
            matches!(node["state"].as_str(), Some("OPEN" | "CLOSED" | "MERGED"))
                && epics::labels(node).is_some()
        });
        let observed = previous
            .and_then(|p| p["needs_you"]["carry_validation"][&item]["checked_at"].as_str())
            .and_then(|t| OffsetDateTime::parse(t, &Rfc3339).ok())
            .filter(|t| {
                OffsetDateTime::parse(&list.validated_at, &Rfc3339).is_ok_and(|now| *t <= now)
            })
            .map(stamp);
        list.carry_validation.insert(
            item,
            json!({
                "status": if checked { "state-labels-checked" } else { "unknown" },
                "checked_at": if checked { Some(list.validated_at.clone()) } else { observed },
            }),
        );
        // A state/label check is not a fresh policy/head/decision read.
        // Preserve carried quality and its original observation clock.
        if !checked {
            list.complete = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_pr_wait_outlives_cleared_approval_hold() {
        let mut snapshot = holds::snapshot(&[], "2026-11-04T12:00:00Z".into(), Default::default());
        for (number, operator) in [(1, false), (2, true)] {
            let mut hold: holds::Published = serde_json::from_value(json!({
                "item":format!("a/work#{number}"),"url":null,"epic":false,
                "kind":"pr-open","reason":"open PR","operator":operator,
                "priority":null,"question":null,"since":"2026-11-04T11:00:00Z"
            }))
            .unwrap();
            hold.carried = true;
            snapshot.holds.push(hold);
        }
        let nodes = (1..=2)
            .map(|n| {
                (
                    format!("a/work#{n}"),
                    json!({"state":"OPEN","labels":{"nodes":[],"totalCount":0}}),
                )
            })
            .collect();
        for _ in 0..2 {
            retire(&mut snapshot, None, &nodes);
            assert_eq!(snapshot.holds.len(), 1);
            assert_eq!(snapshot.holds[0].item.as_deref(), Some("a/work#1"));
        }
    }

    #[test]
    fn source_specific_clearance_never_infers_resolution_from_missing_evidence() {
        let node = |state, labels| json!({"state":state,"labels":{"nodes":labels,"totalCount":0}});
        for (state, labels, expected) in [
            ("OPEN", json!([]), true),
            ("CLOSED", json!([]), true),
            ("OPEN", Value::Null, false),
            ("", json!([]), false),
        ] {
            let nodes = BTreeMap::from([("a/work#1".into(), node(state, labels))]);
            assert_eq!(
                retired("items", "needs-human", "a/work#1", &nodes),
                expected
            );
            assert_eq!(
                retired("reviews", "review-withheld", "a/work#1", &nodes),
                state == "CLOSED"
            );
            assert_eq!(
                retired("items", "needs-operator", "a/work#1", &nodes),
                state == "CLOSED"
            );
            assert_eq!(
                retired("proposals", "operator-action", "a/work#1", &nodes),
                state == "CLOSED"
            );
        }
        assert!(!retired(
            "items",
            "needs-human",
            "a/work#1",
            &BTreeMap::new()
        ));
    }

    #[test]
    fn grouped_retirement_rebuilds_members_cards_counts_and_unknown_age() {
        let at = "2026-11-04T12:00:00Z";
        let on = on_github("a/ops", 1);
        let waits = (1..=3)
            .map(|number| poll::Wait {
                repository: "a/work".into(),
                number,
                head: "a".repeat(40),
                kind: "needs-human",
                reason: "inspect stopped PR".into(),
            })
            .collect();
        let mut reading = holds::Reading {
            operator: "operator",
            epics: false,
            considered: true,
            options: &[],
            on: &on,
            reviews: review_asks(true, Some(&(waits, BTreeSet::new())), at),
            proposals: Collected::off(),
            digest: Collected::off(),
            inbox: BTreeMap::new(),
            observed: holds::Observed::off(),
            summaries: (Read::Complete, BTreeMap::new()),
        };
        let mut snapshot = holds::snapshot(&[], at.into(), Default::default());
        let first = holds::needs_you(&snapshot, None, &reading);
        // Re-reading unchanged digest prose must not resurrect a target
        // whose current state proves that its decision is retired.
        let mut digest = first.items[0].clone();
        digest.source = DIGEST.into();
        digest.kind = "digest-approve".into();
        digest.group = None;
        digest.key.push_str("/digest");
        let closed = BTreeMap::from([("a/work#1".into(), json!({"state":"CLOSED"}))]);
        for _ in 0..2 {
            reading.digest = Collected::of(Read::Complete);
            reading.digest.asks.push(digest.clone());
            sources(&mut reading, &closed);
            assert!(reading.digest.asks.is_empty());
        }
        reading.digest = Collected::off();
        let previous = json!({"schema":holds::HOLDS_SCHEMA,"needs_you":first});
        reading.reviews = Collected::of(Read::Unread);
        let nodes = BTreeMap::from([
            ("a/work#1".into(), json!({"state":"CLOSED"})),
            (
                "a/work#2".into(),
                json!({"state":"OPEN","labels":{"nodes":[],"totalCount":0}}),
            ),
        ]);
        let filtered = retire(&mut snapshot, Some(&previous), &nodes).unwrap();
        let mut next = holds::needs_you(&snapshot, Some(&filtered), &reading);
        assert_eq!(next.items[0].item, "a/work#2");
        assert_eq!(next.items[0].group.as_ref().unwrap().numbers, [2, 3]);
        assert!(next.items[0].head.is_none());
        assert_eq!(
            (
                next.count,
                next.counts.decisions,
                next.counts.groups,
                next.counts.members
            ),
            (2, 1, 1, 2)
        );
        evidence(&mut next, Some(&filtered), &nodes);
        assert_eq!(next.carry_validation["a/work#2"]["checked_at"], at);
        assert_eq!(next.carry_validation["a/work#3"]["status"], "unknown");
        assert!(!next.complete);
        let previous = json!({"schema":holds::HOLDS_SCHEMA,"needs_you":next});
        snapshot.at = "2026-11-04T13:00:00Z".into();
        let mut failed = holds::needs_you(&snapshot, Some(&previous), &reading);
        evidence(&mut failed, Some(&previous), &BTreeMap::new());
        assert_eq!(failed.carry_validation["a/work#2"]["checked_at"], at);
        assert_eq!(failed.carry_validation["a/work#2"]["status"], "unknown");
        assert_eq!(failed.count, 2);
        assert!(
            failed
                .requests
                .iter()
                .all(|r| r.quality == "unverified-carried")
        );
        let closed = BTreeMap::from([
            ("a/work#2".into(), json!({"state":"CLOSED"})),
            ("a/work#3".into(), json!({"state":"MERGED"})),
        ]);
        let filtered = retire(&mut snapshot, Some(&previous), &closed).unwrap();
        let gone = holds::needs_you(&snapshot, Some(&filtered), &reading);
        assert_eq!(
            (
                gone.count,
                gone.counts.decisions,
                gone.counts.groups,
                gone.counts.members
            ),
            (0, 0, 0, 0)
        );
        assert!(gone.items.is_empty() && gone.requests.is_empty() && gone.identities.is_empty());
    }
}
