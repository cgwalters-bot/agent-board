//! Read-only library seam for persistent consumers (tracker#1456).
//!
//! This calls the normal pass in preview, not another scheduler. Transport
//! rejection protects a consumer from accidentally reaching an applying path;
//! it is not a substitute for deploying with read-only credentials. No local
//! persistence, publication, background loop or deployment policy lives here.

use super::*;
use std::sync::OnceLock;

pub const SCHEMA: &str = "agent-board/decision/v1";
/// Reuse the existing single-comment projection bound, independently of whether
/// a caller publishes holds across multiple comments.
pub const MAX_PROJECTION_CHARS: usize = holds::MAX_COMMENT_CHARS;
pub const MAX_POOLS: usize = 16;

/// A conservative read-only adapter. It forwards only GETs, bounded downloads,
/// broker reads and GraphQL documents without a `mutation` token, plus the
/// two counters the shared response cache binds to: the credential context
/// (a digest, never the credential) and the transport's send count. Accounting
/// callbacks are deliberately not forwarded: they are not reads.
pub struct ReadOnly<'a, C> {
    inner: &'a C,
    scope: Option<CredentialScope<C>>,
}

impl<'a, C> ReadOnly<'a, C> {
    pub fn new(inner: &'a C) -> Self {
        Self { inner, scope: None }
    }

    fn check_credential(&self) -> Result<()> {
        if let Some(scope) = &self.scope {
            scope.observe(self.inner);
            ensure!(
                scope.changed.get().is_none(),
                "preview credential context changed; retry with a stable transport"
            );
        }
        Ok(())
    }
}

/// One preview must not return decisions or state made under a different
/// primary credential. Once a change is seen, switching back isn't enough.
struct CredentialScope<C> {
    read: fn(&C) -> Option<String>,
    initial: OnceLock<Option<String>>,
    changed: OnceLock<()>,
}

impl<C> CredentialScope<C> {
    fn observe(&self, client: &C) -> Option<String> {
        if self.changed.get().is_some() {
            return None;
        }
        let context = (self.read)(client);
        if self.initial.get_or_init(|| context.clone()) != &context {
            self.changed.get_or_init(|| ());
            return None;
        }
        context
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
    fn network_requests(&self) -> Option<usize> {
        self.inner.network_requests()
    }

    fn cache_context(&self) -> Option<String> {
        match &self.scope {
            Some(scope) => scope.observe(self.inner),
            None => self.inner.cache_context(),
        }
    }

    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
        self.check_credential()?;
        self.inner.get(url, validator)
    }

    fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        // Preserve status-returning GETs: unlike get(), these let readers
        // interpret a missing resource (404/410) themselves.
        ensure!(
            method == "GET",
            "read-only decision transport rejects {method}"
        );
        self.check_credential()?;
        self.inner.send("GET", url, body)
    }

    fn download(&self, url: &str, limit: u64) -> Result<Vec<u8>> {
        self.check_credential()?;
        self.inner.download(url, limit)
    }
}

impl<C: Transport> Transport for ReadOnly<'_, C> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        query_only(query)?;
        self.check_credential()?;
        self.inner.graphql(query, variables)
    }
}

impl<C: crate::github::GraphQl> crate::github::GraphQl for ReadOnly<'_, C> {
    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        query_only(query)?;
        self.check_credential()?;
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
    /// Where `leanReads` carried it, what was not re-observed: the whole
    /// decision, or the plan with this pass's open pull request count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reused: Option<super::lean::Reused>,
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

/// One read-only preview's decisions and what the next one may reuse.
pub struct Preview {
    pub snapshot: Snapshot,
    /// The pass's state as it would keep it: fetched responses scoped to the
    /// fetch context and representation, saved holds also scoped to config,
    /// and cursors the applying pass carries. Passing it to the next preview
    /// makes that one warm. Losing only its responses costs reads; losing the whole
    /// state also loses those cursors and retained intake, which can change
    /// what the next preview scans. With a `state` path, invalid disposable
    /// responses are discarded, but invalid durable state fails the preview. A
    /// failed preview returns none: a caller that must carry on past one
    /// keeps a copy. Persisting it, and keeping
    /// it as private as the responses it holds, is the caller's job: the
    /// library writes no file. A transport without a credential context
    /// binds no responses or quiet waits, so none come back. Without a
    /// configured `state` path the input comes back unchanged, but its cached
    /// responses, quiet waits and carried holds are not used. Change
    /// watermarks loaded from the caller's store are returned unchanged;
    /// previewing does not advance that store's discovery state.
    pub state: State,
    /// What the pass spent: charged requests, refunded 304s and GraphQL points.
    pub spent: Spent,
    /// The transports' sends, retries included, which a warm preview saves
    /// separately from charged requests; none where the transport does not
    /// count ([`Rest::network_requests`]).
    pub network_sends: Option<usize>,
}

/// No decision snapshot was read because the pass had no request budget.
/// Keep the input state and retry on the next pass.
#[derive(Debug)]
pub struct PreviewDeferred;

impl std::fmt::Display for PreviewDeferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("decision preview deferred: zero request budget")
    }
}

impl std::error::Error for PreviewDeferred {}

/// Explicit opt-in library invocation; no CLI option, local state write or
/// persistent service is enabled. Config, state, transports and clock belong to
/// the caller. Existing pass request/time bounds and caps still apply.
/// A stateful preview fails if it observes a change in the primary transport's
/// cache context. That transport must bind each context check to the credential
/// used for the REST or GraphQL request, including retries.
pub async fn preview<C: Rest + Transport + crate::github::GraphQl + Broker>(
    client: &C,
    dispatcher: &C,
    config: &Config,
    state: State,
    limits: Limits,
    now: OffsetDateTime,
) -> Result<Preview> {
    let mut guarded = ReadOnly::new(client);
    if config.state.is_some() {
        guarded.scope = Some(CredentialScope {
            read: C::cache_context,
            initial: OnceLock::new(),
            changed: OnceLock::new(),
        });
    }
    // One transport stays one, so the pass counts its sends once.
    let same = std::ptr::eq(client, dispatcher);
    let dispatcher = ReadOnly::new(dispatcher);
    // With a `state` path the pass always returns a state; without one it
    // keeps none, so the input goes back as it came.
    let (state, unchanged) = match config.state {
        Some(_) => (carried(state)?, None),
        None => {
            let mut input = state.clone();
            discard_cached_evidence(&mut input);
            (input, Some(state))
        }
    };
    let changes = state.changes.clone();
    let report = super::run(
        &guarded,
        if same { &guarded } else { &dispatcher },
        config,
        state,
        (Mode::Preview, limits),
        now,
    )
    .await;
    // The pass reports many read failures as observations. An identity change
    // is not a partial observation: discard the entire result and its state.
    guarded.check_credential()?;
    let mut report = report?;
    if report.zero_request_budget {
        return Err(PreviewDeferred.into());
    }
    let state = match unchanged {
        Some(state) => state,
        None => {
            let mut state = report.state.take().context("preview returned no state")?;
            // The changes stage consumes its input, reporting new watermarks
            // separately for the store. Keep the caller's discovery state.
            state.changes = changes;
            // Unbound responses, quiet waits and carried holds are not safe
            // to reuse in another preview.
            if state.response_scope.is_none() {
                discard_cached_evidence(&mut state);
            } else {
                for entry in state.pass_responses.values_mut() {
                    *entry = cache::seal(std::mem::take(entry));
                }
                cache::bound_state(&mut state);
            }
            state
        }
    };
    let spent = std::mem::take(&mut report.spent);
    let network_sends = report.network_sends;
    Ok(Preview {
        snapshot: from_report(report, now)?,
        state,
        spent,
        network_sends,
    })
}

/// Without a verified scope, keep durable state but never reuse evidence from
/// an earlier preview or poll. A 304 cannot confirm the credential scope.
fn discard_cached_evidence(state: &mut State) {
    state.response_scope = None;
    state.hold_scope = None;
    state.responses.clear();
    state.pass_responses.clear();
    state.quiet_waits.clear();
    state.carried_holds.clear();
}

/// Drop invalid disposable pass entries before the legacy decoder, which
/// still enforces the cache bounds and validates durable state.
fn carried(mut state: State) -> Result<State> {
    if state.interval_seconds == 0 {
        state.interval_seconds = crate::poll::MIN_INTERVAL_SECONDS;
    }
    // A REST 304 cannot attest to GraphQL's editor identity, which the
    // legacy pass response map can carry without an integrity check.
    state.pass_responses.retain(|url, entry| {
        let keep = cache::admissible(url, entry, true);
        if !keep {
            eprintln!("cache: invalid disposable pass response discarded; cold read");
        }
        keep
    });
    // Read from the caller's store, never from the file.
    let changes = state.changes.take();
    let mut checked = State::from_json(&state.to_json()).context("carried preview state")?;
    checked.changes = changes;
    if checked.response_scope.is_none() {
        discard_cached_evidence(&mut checked);
    }
    Ok(checked)
}

pub(super) fn from_report(report: Report, now: OffsetDateTime) -> Result<Snapshot> {
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
                carried: decision.carried || decision.reused.is_some(),
                reused: decision.reused,
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
    use super::super::tests::{
        ConditionalRecording, block, config, followed, replay, rest_comment_pages,
    };
    use super::*;

    struct Spy {
        replay: Replay,
        effects: Cell<usize>,
        /// Identity 0 lets the recording wrapper supply its default context.
        identity: Cell<u64>,
        rotate_to: Cell<Option<u64>>,
    }

    impl Spy {
        fn new(replay: Replay) -> Self {
            Self {
                replay,
                effects: Cell::new(0),
                identity: Cell::new(0),
                rotate_to: Cell::new(None),
            }
        }
    }

    impl Rest for Spy {
        fn cache_context(&self) -> Option<String> {
            let identity = self.identity.get();
            if let Some(next) = self.rotate_to.take() {
                self.identity.set(next);
            }
            (identity != 0).then(|| identity.to_string())
        }

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
        let spy = Spy::new(replay());
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
            let spy = Spy::new(recording);
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
            let previewed = block(preview(
                &spy,
                &spy,
                &config,
                State::default(),
                limits,
                spy.replay.now,
            ))
            .unwrap();
            let snapshot = previewed.snapshot;
            assert_eq!(snapshot, expected, "{scenario}");
            // Without a `state` path no responses are carried.
            assert!(previewed.state.responses.is_empty(), "{scenario}");
            assert_eq!(previewed.state.response_scope, None, "{scenario}");
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
                    .snapshot
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
            let spy = Spy::new(recording);
            let snapshot = block(preview(
                &spy,
                &spy,
                &config,
                State::default(),
                Limits::default(),
                spy.replay.now,
            ))
            .unwrap()
            .snapshot;
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

    /// A consumer carries the returned state from one preview to the next.
    /// The warm one decides the same at the same clock with fewer sends and
    /// charged requests; a lost, corrupt or rescoped index only costs reads.
    #[test]
    fn carried_preview_state_is_warm_only_within_its_scope() {
        let mut recording = replay();
        rest_comment_pages(&mut recording);
        let forge = ConditionalRecording {
            replay: Spy::new(recording),
            reads: RefCell::default(),
            traffic: RefCell::default(),
            contextless: Cell::default(),
        };
        // The same decisions and reads under another configuration.
        let [config, other_config] =
            [".state/poll.json", ".state/other.json"].map(|path| config(json!({"state": path})));
        let now = forge.replay.replay.now;
        let run = |config: &Config, state: State| {
            let before: usize = forge.reads.borrow().values().sum();
            let previewed = block(preview(
                &forge,
                &forge,
                config,
                state,
                Limits::default(),
                now,
            ))
            .unwrap();
            let sends: usize = forge.reads.borrow().values().sum();
            assert_eq!(previewed.snapshot.failures, 0);
            // One transport behind both roles is counted once.
            assert_eq!(previewed.network_sends, Some(sends - before));
            (previewed, sends - before)
        };
        let sealed = |state: &State| {
            state
                .responses
                .iter()
                .all(|(url, entry)| cache::admissible(url, entry, true))
        };
        let (cold, cold_sends) = run(&config, State::default());
        assert!(!cold.state.responses.is_empty());
        assert!(!cold.state.pass_responses.is_empty());
        assert!(sealed(&cold.state));
        // The returned state is what a caller persists, and it decodes whole.
        assert_eq!(State::from_json(&cold.state.to_json()).unwrap(), cold.state);
        let (warm, warm_sends) = run(&config, cold.state.clone());
        let (reconfigured, reconfigured_sends) = run(&other_config, cold.state.clone());
        for (case, again, sends) in [
            ("same configuration", &warm, warm_sends),
            ("other state path", &reconfigured, reconfigured_sends),
        ] {
            assert_eq!(again.snapshot, cold.snapshot, "{case}");
            assert_eq!(
                again.state.response_scope, cold.state.response_scope,
                "{case}"
            );
            assert!(sends < cold_sends, "{case}: {sends} < {cold_sends}");
            assert!(again.spent.requests < cold.spent.requests, "{case}");
            assert!(again.spent.revalidated > cold.spent.revalidated, "{case}");
        }
        let mut lost = cold.state.clone();
        lost.responses.clear();
        let mut corrupt = cold.state.clone();
        for entry in corrupt.responses.values_mut() {
            entry["body"] = json!("tampered after sealing");
        }
        let mut unsigned = cold.state.clone();
        for entry in unsigned.responses.values_mut() {
            entry.as_object_mut().unwrap().remove("digest");
        }
        for (case, config, state, identity) in [
            ("lost", &config, lost, 0),
            ("corrupt", &config, corrupt, 0),
            ("unsigned", &config, unsigned, 0),
            ("other credential", &config, cold.state.clone(), 1),
        ] {
            forge.replay.identity.set(identity);
            let (again, sends) = run(config, state);
            assert_eq!(again.snapshot, cold.snapshot, "{case}");
            assert_eq!(sends, cold_sends, "{case}");
            assert_eq!(again.spent.requests, cold.spent.requests, "{case}");
            assert!(sealed(&again.state), "{case}");
            assert_eq!(
                again.state.response_scope == cold.state.response_scope,
                identity == 0,
                "{case}"
            );
        }
        forge.replay.identity.set(0);
        // Scoped, then contextless, then scoped again: a transport that names
        // no context reads as cold as one given no state, even though it
        // answers validators, and hands back nothing to reuse under any scope.
        forge.contextless.set(true);
        let (baseline, contextless_sends) = run(&config, State::default());
        let (contextless, sends) = run(&config, cold.state.clone());
        assert_eq!(contextless.snapshot, cold.snapshot);
        assert_eq!(sends, contextless_sends);
        assert_eq!(contextless.spent.requests, baseline.spent.requests);
        assert_eq!(contextless.spent.revalidated, baseline.spent.revalidated);
        assert!(contextless.state.responses.is_empty());
        assert!(contextless.state.pass_responses.is_empty());
        assert_eq!(contextless.state.response_scope, None);
        forge.contextless.set(false);
        let (rescoped, sends) = run(&config, contextless.state);
        assert_eq!(rescoped.snapshot, cold.snapshot);
        assert_eq!(sends, cold_sends);
        assert_eq!(rescoped.spent.requests, cold.spent.requests);
        assert_eq!(rescoped.state.response_scope, cold.state.response_scope);
        // The poll's own unsuffixed responses are checked as a file's are.
        let mut tampered = cold.state.clone();
        for (url, entry) in &cold.state.responses {
            if let Some(url) = url.strip_suffix(cache::REPRESENTATION) {
                let mut entry = entry.clone();
                entry["body"] = json!("tampered after sealing");
                tampered.responses.insert(url.to_owned(), entry);
            }
        }
        assert!(tampered.responses.len() > cold.state.responses.len());
        assert_eq!(carried(tampered).unwrap().responses, cold.state.responses);
        // Without a `state` path the input comes back as it went in, even
        // where the pass follows work and would carry poll state.
        let follow = super::super::tests::config(json!({"follow": ["cgwalters-forge/tracker"],
            "pr": {"fixPools": ["claude"]}}));
        assert!(follow.state.is_none());
        let unchanged = block(preview(
            &forge,
            &forge,
            &follow,
            warm.state.clone(),
            Limits::default(),
            now,
        ))
        .unwrap();
        assert_eq!(unchanged.state, warm.state);
        assert_eq!(forge.replay.effects.get(), 0);
        assert!(
            forge
                .traffic
                .borrow()
                .iter()
                .all(|(request, _)| request.starts_with("GET ") || request.starts_with("LIVE "))
        );
    }

    #[test]
    fn no_state_preview_ignores_unscoped_poll_responses() {
        const REPO: &str = "cgwalters-forge/agentic-job";
        let mut recording = replay();
        followed(&mut recording, (REPO, 9), json!({}), json!([]));
        let now = recording.now;
        let forge = ConditionalRecording {
            replay: Spy::new(recording),
            reads: RefCell::default(),
            traffic: RefCell::default(),
            contextless: Cell::default(),
        };
        let config = config(json!({"wip":0, "follow":[REPO], "pr":{"fixPools":["claude"]}}));
        assert!(config.state.is_none());
        let url = format!("{API_ROOT}repos/{REPO}/pulls?state=open&per_page=100");
        let etag = forge.get(&url, None).unwrap().etag.unwrap();
        // The earlier credential saw no open PRs. The new one sees PR 9,
        // but the transport will answer the old validator with 304.
        let legacy = State::from_json(&json!({
            "schema": crate::poll::POLL_STATE_SCHEMA,
            "interval_seconds": crate::poll::MIN_INTERVAL_SECONDS,
            "responses": {(&url): {"etag": etag, "body": []}},
        }))
        .unwrap();
        assert_eq!(legacy.response_scope, None);
        assert!(legacy.responses.contains_key(&url));
        forge.replay.identity.set(1);
        assert_eq!(
            forge
                .get(&url, Some(("If-None-Match", &etag)))
                .unwrap()
                .status,
            304
        );
        let run = |state| {
            block(preview(
                &forge,
                &forge,
                &config,
                state,
                Limits::default(),
                now,
            ))
            .unwrap()
        };
        let cold = run(State::default());
        forge.traffic.borrow_mut().clear();
        let previewed = run(legacy.clone());
        let traffic = forge.traffic.borrow();
        let request = format!("GET {url}");
        assert_eq!(
            traffic
                .iter()
                .filter(|(key, _)| key == &request)
                .map(|(_, conditional)| *conditional)
                .collect::<Vec<_>>(),
            [false],
        );
        assert_eq!(previewed.snapshot, cold.snapshot);
        assert_eq!(previewed.state, legacy);
    }

    #[test]
    fn carried_comment_editor_is_not_reused_after_a_rest_304() {
        let mut recording = replay();
        rest_comment_pages(&mut recording);
        let now = recording.now;
        let forge = ConditionalRecording {
            replay: recording,
            reads: RefCell::default(),
            traffic: RefCell::default(),
            contextless: Cell::default(),
        };
        let config = config(json!({"state": ".state/poll.json"}));
        let preview = |state| {
            block(preview(
                &forge,
                &forge,
                &config,
                state,
                Limits::default(),
                now,
            ))
            .unwrap()
        };
        let cold = preview(State::default());
        let mut carried = cold.state.clone();
        // Keep the separate, sealed REST entry untouched.
        let (url, entry) = carried
            .pass_responses
            .iter_mut()
            .find(|(_, entry)| {
                entry["comment_evidence"]
                    .pointer("/repository/issue/comments/nodes/0")
                    .is_some()
            })
            .unwrap();
        let baseline_entry = entry.clone();
        let rest = format!("GET {url}");
        assert!(cache::admissible(
            url,
            &carried.responses[&format!("{url}{}", cache::REPRESENTATION)],
            true
        ));
        let comments = format!("GraphQL {}", plan::COMMENTS_QUERY);
        let url = url.clone();
        let reads = |state| {
            forge.reads.borrow_mut().clear();
            forge.traffic.borrow_mut().clear();
            let warm = preview(state);
            assert!(forge.traffic.borrow().contains(&(rest.clone(), true)));
            assert_eq!(warm.snapshot, cold.snapshot);
            forge.reads.borrow().get(&comments).copied().unwrap_or(0)
        };
        let warm_reads = reads(cold.state.clone());
        let mut tampered = baseline_entry.clone();
        tampered["comment_evidence"]["repository"]["issue"]["comments"]["nodes"][0]["editor"] =
            json!({"login": "tampered-editor"});
        let without = |field| {
            let mut entry = baseline_entry.clone();
            entry.as_object_mut().unwrap().remove(field);
            entry
        };
        for (case, entry) in [
            ("tampered editor", tampered),
            ("missing digest", without("digest")),
            ("missing body", without("body")),
        ] {
            carried.pass_responses.insert(url.clone(), entry);
            assert_eq!(reads(carried.clone()), warm_reads + 1, "{case}");
        }
        carried.interval_seconds = -1;
        let error = super::carried(carried).unwrap_err();
        assert!(format!("{error:#}").contains("interval_seconds is missing or out of bounds"));
    }

    #[test]
    fn preview_keeps_the_callers_pending_checks() {
        use crate::changes::{diff, format_time};

        const REPO: &str = "cgwalters-forge/agentic-job";
        let now = replay().now;
        let since = format_time(now - diff::FIRST_WINDOW).unwrap();
        let listing = format!(
            "{API_ROOT}repos/{REPO}/issues?state=all&sort=updated&direction=asc&since={since}&per_page=100"
        );
        let query = diff::detail_query(&[9]);
        let request = graphql_key(
            &query,
            &json!({"owner":"cgwalters-forge", "name":"agentic-job", "since":since}),
        );
        let changes = diff::State {
            pending: [format!("{REPO}#9")].into(),
            ..diff::State::default()
        };
        let mut state = State {
            changes: Some(changes.clone()),
            ..State::default()
        };
        let config = config(
            json!({"state":".state/poll.json", "store":".state/changes.db",
            "changeSweepHours":6, "wip":0, "follow":[REPO],
            "pr":{"noMerge":[REPO], "fixPools":["claude"]}}),
        );
        for body in [json!([{"number":9}]), json!([])] {
            let mut recording = replay();
            followed(&mut recording, (REPO, 9), json!({}), json!([]));
            recording.requests.extend([
                json!({"request":{"get":listing}, "answer":{"status":200, "body":body}}),
                json!({"request":request, "answer":{"data":{"repository":{"i0":{
                    "__typename":"PullRequest", "number":9, "title":"Pending checks",
                    "url":format!("https://github.com/{REPO}/pull/9"), "state":"OPEN",
                    "createdAt":since, "updatedAt":since, "merged":false,
                    "timelineItems":{"nodes":[], "pageInfo":{"hasNextPage":false}},
                    "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"PENDING"}}}]}
                }}}}}),
            ]);
            let forge = ConditionalRecording {
                replay: recording,
                reads: RefCell::default(),
                traffic: RefCell::default(),
                contextless: Cell::default(),
            };
            let previewed = block(preview(
                &forge,
                &forge,
                &config,
                state,
                Limits::default(),
                now,
            ))
            .unwrap();
            assert_eq!(
                forge.reads.borrow().get(&format!("GraphQL {query}")),
                Some(&1)
            );
            state = previewed.state;
            assert_eq!(state.changes, Some(changes.clone()));
        }
    }

    #[test]
    fn quiet_pull_requests_require_a_stable_preview_scope() {
        const REPO: &str = "cgwalters-forge/agentic-job";
        let mut recording = replay();
        let now = recording.now;
        let updated = now.format(&Rfc3339).unwrap();
        followed(
            &mut recording,
            (REPO, 9),
            json!({"isDraft":true, "updatedAt":updated}),
            json!([]),
        );
        let listing = format!("{API_ROOT}repos/{REPO}/pulls?state=open&per_page=100");
        let listed = recording
            .requests
            .iter_mut()
            .find(|r| r["request"]["get"] == listing)
            .unwrap();
        listed["answer"]["body"][0]["head"] = json!({"sha":"abc"});
        listed["answer"]["body"][0]["updated_at"] = json!(updated);
        let forge = ConditionalRecording {
            replay: Spy::new(recording),
            reads: RefCell::default(),
            traffic: RefCell::default(),
            contextless: Cell::default(),
        };
        let config = config(json!({"state":".state/poll.json", "wip":0, "follow":[REPO],
            "pr":{"fixPools":["claude"], "noMerge":[REPO], "quietWaits":true}}));
        let reads = format!("GraphQL {}", poll::PULL_QUERY);
        let preview = |state| {
            block(preview(
                &forge,
                &forge,
                &config,
                state,
                Limits::default(),
                now,
            ))
        };
        // Confirm this unchanged listing can replay the wait when scoped.
        let scoped = preview(State::default()).unwrap();
        assert!(scoped.state.quiet_waits.contains_key(&format!("{REPO}#9")));
        let warm = preview(scoped.state.clone()).unwrap();
        assert_eq!(forge.reads.borrow()[&reads], 1);

        // Rotate to B after capturing A's namespace. Don't send validators or
        // return a snapshot or state for this mixed-credential preview.
        forge.traffic.borrow_mut().clear();
        forge.replay.rotate_to.set(Some(1));
        let Err(error) = preview(scoped.state.clone()) else {
            panic!("rotated preview returned decisions or state");
        };
        assert!(
            error
                .to_string()
                .contains("preview credential context changed")
        );
        assert_eq!(forge.replay.identity.get(), 1);
        assert!(forge.traffic.borrow().is_empty());
        forge.replay.identity.set(0);
        let retried = preview(scoped.state).unwrap();
        assert_eq!(retried.snapshot, warm.snapshot);
        assert_eq!(forge.reads.borrow()[&reads], 1);

        forge.contextless.set(true);
        let first = preview(State::default()).unwrap();
        assert_eq!(forge.reads.borrow()[&reads], 2);
        let second = preview(first.state).unwrap();
        assert_eq!(forge.reads.borrow()[&reads], 3);
        assert!(second.state.quiet_waits.is_empty());
        assert_eq!(first.snapshot, second.snapshot);
    }
}
