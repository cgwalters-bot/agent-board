//! Read-only library seam for persistent consumers (tracker#1456).
//!
//! This calls the normal pass in preview, not another scheduler. Transport
//! rejection protects a consumer from accidentally reaching an applying path;
//! it is not a substitute for deploying with read-only credentials. No local
//! persistence, publication, background loop or deployment policy lives here.

use super::*;

pub const SCHEMA: &str = "agent-board/decision/v1";
/// Reuse the existing single-comment projection bound, independently of whether
/// a caller publishes holds across multiple comments.
pub const MAX_PROJECTION_CHARS: usize = holds::MAX_COMMENT_CHARS;
pub const MAX_POOLS: usize = 16;

/// A conservative read-only adapter. It forwards only GETs, bounded downloads,
/// broker reads and GraphQL documents without a `mutation` token. Accounting
/// callbacks are deliberately not forwarded: they are not reads.
pub struct ReadOnly<'a, C> {
    inner: &'a C,
}

impl<'a, C> ReadOnly<'a, C> {
    pub fn new(inner: &'a C) -> Self {
        Self { inner }
    }
}

fn query_only(query: &str) -> Result<()> {
    // Deliberately conservative, including comments and string literals: no
    // GraphQL lexer ambiguity (or second operation) can hide a mutation. These
    // are library-owned query documents, not arbitrary user GraphQL input.
    ensure!(
        !query
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|token| token == "mutation"),
        "read-only decision transport rejects GraphQL mutations"
    );
    Ok(())
}

impl<C: Rest> Rest for ReadOnly<'_, C> {
    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
        self.inner.get(url, validator)
    }

    fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        // Preserve status-returning GETs: unlike get(), these let readers
        // interpret a missing resource (404/410) themselves.
        ensure!(
            method == "GET",
            "read-only decision transport rejects {method}"
        );
        self.inner.send("GET", url, body)
    }

    fn download(&self, url: &str, limit: u64) -> Result<Vec<u8>> {
        self.inner.download(url, limit)
    }
}

impl<C: Transport> Transport for ReadOnly<'_, C> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        query_only(query)?;
        self.inner.graphql(query, variables)
    }
}

impl<C: crate::github::GraphQl> crate::github::GraphQl for ReadOnly<'_, C> {
    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        query_only(query)?;
        self.inner.graphql(query, variables).await
    }
}

impl<C: Broker> Broker for ReadOnly<'_, C> {
    fn usage(&self, url: &str) -> Result<Value> {
        self.inner.usage(url)
    }

    fn grant(&self, url: &str) -> Result<Value> {
        self.inner.grant(url)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceState {
    Missing,
    Stale,
    Partial,
    Observed,
}

/// Provenance is about the computation, not approval or mutation authority.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Provenance {
    pub producer: &'static str,
    pub mode: &'static str,
    pub holds: &'static str,
    pub usage: EvidenceState,
    /// When the usage projection was computed; source observation clocks are
    /// per-pool and may be missing even with a fresh computation.
    pub usage_computed_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Work {
    pub schema: String,
    pub repositories: BTreeMap<String, RepositoryWork>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RepositoryWork {
    pub reviews_complete: bool,
    pub issues_complete: bool,
    pub count: usize,
    pub counts: BTreeMap<String, usize>,
    pub omitted: usize,
    pub items: Vec<WorkItem>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkItem {
    pub item: String,
    pub kind: String,
    pub operator: bool,
    pub reason: Option<String>,
    pub head: Option<String>,
    /// Previewed by this pass, not proof of execution.
    pub taken: Option<bool>,
    pub plan_kind: Option<String>,
    pub hold_kind: Option<holds::HoldKind>,
    pub carried: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Pool {
    pub name: String,
    pub evidence: EvidenceState,
    pub hold: Option<bool>,
    pub reason: Option<String>,
    pub used_percent: Option<f64>,
    pub allowed_percent: Option<f64>,
    pub observed_at: Option<String>,
    pub resets_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Hold {
    #[serde(flatten)]
    pub decision: holds::Published,
    /// Carried evidence was not re-observed by this pass.
    pub carried: bool,
}

/// A bounded decision projection. Missing sections mean unavailable, never
/// empty. Completeness concerns coverage, not eligibility or authorization.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Snapshot {
    pub schema: &'static str,
    pub observed_at: String,
    /// Set only by the existing needs builder when it validates its sources;
    /// never promoted from `observed_at` just because a preview ran.
    pub validated_at: Option<String>,
    pub complete: bool,
    pub holds_complete: bool,
    pub failures: usize,
    pub provenance: Provenance,
    pub holds: Vec<Hold>,
    pub holds_omitted: usize,
    pub needs_you: Option<holds::NeedsYou>,
    pub work: Work,
    pub wip: holds::Wip,
    pub candidates: Option<usize>,
    pub preview_starts: BTreeMap<String, u64>,
    pub confirmed_effects: Effects,
    pub pools: Vec<Pool>,
    pub pools_omitted: usize,
}

pub(super) struct Observation {
    snapshot: holds::Snapshot,
    work: Value,
    usage: Option<Value>,
    holds_complete: bool,
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    /// Capture only already-read facts, without making publication a dependency
    /// of the library. Configured needs/Act now/next fields use the publication
    /// builder verbatim; absent ones remain explicitly unavailable.
    pub(super) fn observe_decision(&mut self) {
        let snapshot = self.report.projection.take().unwrap_or_else(|| {
            holds::snapshot(
                &self.report.holds,
                self.now.format(&Rfc3339).unwrap_or_default(),
                self.report.tally.clone(),
            )
        });
        let work = work::snapshot(
            (&self.follows, &self.config.work_repositories),
            self.followed.as_ref(),
            &self.stepped,
            &self.report.accepted_plans.borrow(),
            &snapshot.holds,
            &self.report.upstreams.borrow(),
        );
        self.report.observation = Some(Observation {
            snapshot,
            work,
            usage: self.snapshot.clone().flatten(),
            holds_complete: self.holds_whole
                && self.considered
                && self.unconsidered == 0
                && self.deferred_epics.is_empty()
                && self.unread == 0,
        });
    }
}

/// Explicit opt-in library invocation; no CLI option, local state write or
/// persistent service is enabled. Config, state, transports and clock belong to
/// the caller. Existing pass request/time bounds and caps still apply.
pub async fn preview<C: Rest + Transport + crate::github::GraphQl + Broker>(
    client: &C,
    dispatcher: &C,
    config: &Config,
    state: State,
    limits: Limits,
    now: OffsetDateTime,
) -> Result<Snapshot> {
    let client = ReadOnly::new(client);
    let dispatcher = ReadOnly::new(dispatcher);
    let report = super::run(
        &client,
        &dispatcher,
        config,
        state,
        (Mode::Preview, limits),
        now,
    )
    .await?;
    from_report(report, now)
}

fn from_report(report: Report, now: OffsetDateTime) -> Result<Snapshot> {
    let observation = report
        .observation
        .context("preview did not reach decision observation")?;
    let work: Work =
        serde_json::from_value(observation.work).context("typed pass work projection")?;
    let (snapshot, _) = holds::fit(observation.snapshot)?;
    let usage = observation.usage.as_ref();
    let timestamp = |v: &Value| {
        v.as_str()
            .filter(|s| s.len() <= 64)
            .filter(|s| OffsetDateTime::parse(s, &Rfc3339).is_ok())
            .map(str::to_owned)
    };
    let usage_computed_at = usage.and_then(|u| timestamp(&u["updated_at"]));
    let fresh = usage_computed_at
        .as_deref()
        .and_then(|t| OffsetDateTime::parse(t, &Rfc3339).ok())
        .is_some_and(|t| {
            now - t <= time::Duration::seconds(USAGE_STALE_SECONDS)
                && t - now <= time::Duration::seconds(CLOCK_SKEW_SECONDS)
        });
    let entries = usage.and_then(|u| u["pools"].as_object());
    let mut entries: Vec<_> = entries.into_iter().flatten().collect();
    entries.sort_by_key(|(name, _)| *name);
    let pools_omitted = entries.len().saturating_sub(MAX_POOLS);
    let pools: Vec<_> = entries
        .into_iter()
        .take(MAX_POOLS)
        .map(|(name, p)| Pool {
            name: name.chars().take(64).collect(),
            evidence: match timestamp(&p["observed_at"]) {
                None => EvidenceState::Missing,
                Some(_) if !fresh => EvidenceState::Stale,
                Some(at) => {
                    let age =
                        OffsetDateTime::parse(&at, &Rfc3339).map(|at| (now - at).as_seconds_f64());
                    let limit = if p["source"] == usage::GRANT_SOURCE {
                        usage::GRANT_STALE_SECONDS
                    } else {
                        OBSERVED_STALE_SECONDS as f64
                    };
                    if age.is_ok_and(|age| (-(CLOCK_SKEW_SECONDS as f64)..=limit).contains(&age)) {
                        EvidenceState::Observed
                    } else {
                        EvidenceState::Stale
                    }
                }
            },
            hold: p["hold"].as_bool(),
            reason: p["reason"]
                .as_str()
                .map(|s| s.chars().take(holds::MAX_ASK).collect()),
            used_percent: p["used_percent"].as_f64(),
            allowed_percent: p["allowed_percent"].as_f64(),
            observed_at: timestamp(&p["observed_at"]),
            resets_at: timestamp(&p["resets_at"]),
        })
        .collect();
    // Pool pacing already discards missing/stale broker windows as no-reading.
    // Never promote a fresh computation clock to proof that windows were read.
    let evidence = if usage.is_none() || pools.is_empty() {
        EvidenceState::Missing
    } else if !fresh {
        EvidenceState::Stale
    } else if pools.iter().all(|p| p.evidence == EvidenceState::Observed) && pools_omitted == 0 {
        EvidenceState::Observed
    } else if pools.iter().all(|p| p.evidence == EvidenceState::Missing) {
        EvidenceState::Missing
    } else if pools.iter().all(|p| p.evidence == EvidenceState::Stale) {
        EvidenceState::Stale
    } else {
        EvidenceState::Partial
    };
    let validated_at = snapshot
        .needs_you
        .as_ref()
        .filter(|n| n.complete)
        .map(|n| n.validated_at.clone());
    let complete = report.failures == 0
        && observation.holds_complete
        && evidence == EvidenceState::Observed
        && snapshot.truncated == 0
        && pools_omitted == 0
        && snapshot.needs_you.as_ref().is_some_and(|n| n.complete)
        && work
            .repositories
            .values()
            .all(|r| r.reviews_complete && r.issues_complete && r.omitted == 0);
    Ok(Snapshot {
        schema: SCHEMA,
        observed_at: now.format(&Rfc3339)?,
        validated_at,
        complete,
        holds_complete: observation.holds_complete,
        failures: report.failures,
        provenance: Provenance {
            producer: "board-forge/pass",
            mode: "preview",
            holds: holds::HOLDS_SCHEMA,
            usage: evidence,
            usage_computed_at,
        },
        holds: snapshot
            .holds
            .into_iter()
            .map(|decision| Hold {
                carried: decision.carried,
                decision,
            })
            .collect(),
        holds_omitted: snapshot.truncated,
        needs_you: snapshot.needs_you,
        work,
        wip: snapshot.wip,
        candidates: snapshot.pass.candidates,
        preview_starts: report.planned_starts,
        confirmed_effects: report.effects,
        pools,
        pools_omitted,
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{block, config, replay};
    use super::*;

    struct Spy {
        replay: Replay,
        effects: Cell<usize>,
    }

    impl Rest for Spy {
        fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
            self.replay.get(url, validator)
        }

        fn send(&self, method: &str, url: &str, _: &Value) -> Result<(u16, Value)> {
            if method == "GET" {
                let response = self.replay.get(url, None)?;
                return Ok((response.status, response.body));
            }
            self.effects.set(self.effects.get() + 1);
            bail!("write reached underlying transport")
        }

        fn start_kind(&self, _: &str) {
            self.effects.set(self.effects.get() + 1);
        }

        fn under_way(&self, _: bool) {
            self.effects.set(self.effects.get() + 1);
        }
    }

    impl Transport for Spy {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            if query_only(query).is_err() {
                self.effects.set(self.effects.get() + 1);
            }
            Transport::graphql(&self.replay, query, variables)
        }
    }

    impl crate::github::GraphQl for Spy {
        async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            Transport::graphql(self, query, variables)
        }
    }

    impl Broker for Spy {
        fn usage(&self, url: &str) -> Result<Value> {
            self.replay.usage(url)
        }

        fn grant(&self, url: &str) -> Result<Value> {
            self.replay.grant(url)
        }
    }

    #[test]
    fn rejection_precedes_transport_for_every_effect_path() {
        let spy = Spy {
            replay: replay(),
            effects: Cell::new(0),
        };
        let client = ReadOnly::new(&spy);
        for (method, path) in [
            ("POST", "issues/1/comments"),
            ("PATCH", "issues/1"),
            ("DELETE", "issues/1/labels/held"),
            ("PUT", "pulls/1/merge"),
            ("POST", "actions/workflows/dispatch.yml/dispatches"),
            ("POST", "actions/runs/1/rerun"),
            ("HEAD", "user"),
        ] {
            assert!(client.send(method, path, &json!({})).is_err());
        }
        for query in [
            "mutation { updateIssue { id } }",
            " \nmutation M { x }",
            "# misleading prefix\nmutation { x }",
            "query { x } mutation M { x }",
            "\u{feff}mutation { x }",
            "query Q { x }\nmutation{ x }",
        ] {
            assert!(Transport::graphql(&client, query, json!({})).is_err());
            assert!(block(crate::github::GraphQl::graphql(&client, query, json!({}))).is_err());
        }
        client.start_kind("implement");
        client.under_way(true);
        assert_eq!(spy.effects.get(), 0);
    }

    #[test]
    fn replay_parity_determinism_and_zero_effects() {
        for scenario in ["normal", "missing-usage", "stale", "partial-board", "caps"] {
            let mut recording = replay();
            if scenario == "missing-usage" {
                recording.requests.retain(|r| {
                    !r["request"]
                        .to_string()
                        .contains("bot-ops/issues/1/comments")
                });
            }
            if scenario == "stale" {
                recording.now += time::Duration::hours(2);
            }
            if scenario == "partial-board" {
                let entry = recording
                    .requests
                    .iter_mut()
                    .find(|r| {
                        r["answer"]
                            .pointer("/data/repositoryOwner/projectV2/items")
                            .is_some()
                    })
                    .unwrap();
                // An unread next page must not become a complete empty board.
                entry["answer"]["data"]["repositoryOwner"]["projectV2"]["items"]["pageInfo"] =
                    json!({"hasNextPage":true,"endCursor":"unrecorded"});
            }
            let config = config(json!({}));
            let limits = if scenario == "caps" {
                Limits {
                    dispatch: Some(0),
                    writes: Some(0),
                    ..Limits::default()
                }
            } else {
                Limits::default()
            };
            let plain = block(run(
                &recording,
                &recording,
                &config,
                State::default(),
                (Mode::Preview, limits),
                recording.now,
            ))
            .unwrap();
            let spy = Spy {
                replay: recording,
                effects: Cell::new(0),
            };
            let client = ReadOnly::new(&spy);
            let guarded = block(run(
                &client,
                &client,
                &config,
                State::default(),
                (Mode::Preview, limits),
                spy.replay.now,
            ))
            .unwrap();
            assert_eq!(plain.lines, guarded.lines, "{scenario}");
            assert_eq!(plain.summary_line(), guarded.summary_line(), "{scenario}");
            let expected = from_report(plain, spy.replay.now).unwrap();
            let snapshot = block(preview(
                &spy,
                &spy,
                &config,
                State::default(),
                limits,
                spy.replay.now,
            ))
            .unwrap();
            assert_eq!(snapshot, expected, "{scenario}");
            assert_eq!(
                serde_json::to_string(&snapshot).unwrap(),
                serde_json::to_string(
                    &block(preview(
                        &spy,
                        &spy,
                        &config,
                        State::default(),
                        limits,
                        spy.replay.now
                    ))
                    .unwrap()
                )
                .unwrap()
            );
            assert_eq!(snapshot.confirmed_effects, Effects::default());
            assert_eq!(spy.effects.get(), 0, "{scenario}");
            assert!(!snapshot.complete); // Issue work is not a census.
            if scenario == "missing-usage" {
                assert_eq!(snapshot.provenance.usage, EvidenceState::Missing);
            }
            if scenario == "stale" {
                assert_eq!(snapshot.provenance.usage, EvidenceState::Stale);
                assert_ne!(
                    snapshot.provenance.usage_computed_at.as_deref(),
                    Some(snapshot.observed_at.as_str())
                );
            }
            if scenario == "partial-board" {
                assert!(!snapshot.holds_complete);
            }
            if scenario == "caps" {
                assert!(snapshot.preview_starts.is_empty());
            }
        }
    }

    #[test]
    fn configured_needs_and_act_now_reuse_the_publication_builder() {
        let recording = replay();
        let config = config(
            json!({"holds": {"repository":"cgwalters-forge/bot-ops", "issue":1,
            "needsYou":true,"approvalEvidence":true,"actNow":true}}),
        );
        let report = block(run(
            &recording,
            &recording,
            &config,
            State::default(),
            (Mode::Preview, Limits::default()),
            recording.now,
        ))
        .unwrap();
        let projection = &report.observation.as_ref().unwrap().snapshot;
        let expected = projection.needs_you.clone().unwrap();
        assert!(expected.act_now.is_some());
        let snapshot = from_report(report, recording.now).unwrap();
        assert_eq!(snapshot.needs_you, Some(expected));
        assert_eq!(snapshot.confirmed_effects, Effects::default());
    }

    #[test]
    fn status_returning_get_keeps_absence_distinct_from_read_error() {
        struct Missing;

        impl Rest for Missing {
            fn get(&self, _: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
                bail!("not found, as the live conditional reader reports")
            }

            fn send(&self, method: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
                assert_eq!(method, "GET");
                Ok((404, Value::Null))
            }
        }

        let client = ReadOnly::new(&Missing);
        let url = "https://api.github.com/repos/o/r/contents/cost?ref=data";
        assert!(client.get(url, None).is_err());
        assert_eq!(
            client.send("GET", url, &Value::Null).unwrap(),
            (404, Value::Null)
        );
        assert!(client.send("POST", url, &Value::Null).is_err());
        let config = config(json!({"cost":{"hourUtc":0},
            "prices":{"anthropic/claude-x":{"input":3,"cached_input":0.3,"output":15}}}));
        let now = replay().now;
        let mut plain = Report::default();
        let mut guarded = Report::default();
        let within = (
            now,
            false,
            Instant::now() + Duration::from_secs(PASS_SECONDS_LIMIT),
        );
        super::super::cost::stage(
            &Missing,
            &config,
            within,
            &Caps::new(Limits::default()),
            &mut plain,
        );
        super::super::cost::stage(
            &client,
            &config,
            within,
            &Caps::new(Limits::default()),
            &mut guarded,
        );
        assert_eq!(plain.failures, 0);
        assert_eq!(plain.lines, guarded.lines);
        assert_eq!(plain.failures, guarded.failures);
    }

    #[test]
    fn successful_broker_read_does_not_freshen_missing_or_old_windows() {
        for stale in [false, true] {
            let mut recording = replay();
            let mut broker = usage::tests::broker();
            if stale {
                broker["anthropic"]["unified_7d"]["observed_at"] = 1.into();
                broker["codex"]["primary"]["observed_at"] = 1.into();
            } else {
                broker["anthropic"] = json!({});
                broker["codex"] = json!({});
            }
            recording
                .requests
                .push(json!({"request":broker_key(USAGE_PATH),"answer":{"body":broker}}));
            let config = config(
                json!({"usage":{"repository":"cgwalters-forge/bot-ops", "issue":1,
                "url":"https://broker.invalid/usage"}}),
            );
            let spy = Spy {
                replay: recording,
                effects: Cell::new(0),
            };
            let snapshot = block(preview(
                &spy,
                &spy,
                &config,
                State::default(),
                Limits::default(),
                spy.replay.now,
            ))
            .unwrap();
            let computed = OffsetDateTime::parse(
                snapshot.provenance.usage_computed_at.as_deref().unwrap(),
                &Rfc3339,
            )
            .unwrap();
            assert!((spy.replay.now - computed).abs() < time::Duration::milliseconds(1));
            assert_eq!(snapshot.provenance.usage, EvidenceState::Missing);
            assert!(
                snapshot
                    .pools
                    .iter()
                    .all(|p| p.evidence == EvidenceState::Missing
                        && p.observed_at.is_none()
                        && p.hold == Some(true))
            );
            assert_eq!(spy.effects.get(), 0);
            assert_eq!(snapshot.confirmed_effects, Effects::default());
        }
    }

    #[test]
    fn oversized_observations_have_explicit_omission_counts() {
        let recording = replay();
        let mut report = super::super::tests::preview(&config(json!({})), &recording);
        let observation = report.observation.as_mut().unwrap();
        let template = observation
            .snapshot
            .holds
            .iter()
            .find(|h| h.item.is_some())
            .unwrap()
            .clone();
        observation.snapshot.holds = (1..=500)
            .map(|number| holds::Published {
                item: Some(format!("o/r#{number}")),
                reason: "long reason ".repeat(100),
                operator: false,
                ..template.clone()
            })
            .collect();
        let pools = (0..MAX_POOLS + 3)
            .map(|n| (format!("pool{n}"), json!({"hold":true})))
            .collect::<serde_json::Map<_, _>>();
        observation.usage =
            Some(json!({"updated_at":recording.now.format(&Rfc3339).unwrap(),"pools":pools}));
        let snapshot = from_report(report, recording.now).unwrap();
        assert!(snapshot.holds_omitted > 0);
        assert_eq!(snapshot.holds.len() + snapshot.holds_omitted, 500);
        assert_eq!(snapshot.pools.len(), MAX_POOLS);
        assert_eq!(snapshot.pools_omitted, 3);
        assert!(!snapshot.complete);
    }
}
