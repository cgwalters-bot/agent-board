//! `pace`: each run's pool follows the usage snapshot (tracker's usage.yml,
//! the comment the supervisor's pace check reads), so that a pool that has
//! used more of its window than has passed takes no new run
//! (agent-board#135). Without it, fix rounds alternated `fixPools` blind: on
//! 17 Oct the openai pool had used 26% of a fresh 7-day window, against a 4%
//! pace, and the poll kept starting rounds there. The snapshot's own `hold`
//! decides where it gives one (agent-board#233).
//!
//! Off by default. It only chooses among, or holds back, the pools the
//! configuration already names for a run; it never adds a pool, a run or a
//! round. Missing usage holds new runs, as the design's decision D says,
//! rather than reading as room.
use super::*;
use crate::supervise::health::{USAGE_MARKER, usage_snapshot};
use crate::supervise::millis;

/// The most `pace.marginPercent` may be: a pool can use at most all of its window.
pub const PACE_MARGIN_LIMIT: f64 = 100.0;
/// How old the snapshot may be and still pace: the supervisor's default
/// `usageStaleMinutes`. Older, it says nothing of the pools now.
const USAGE_STALE_SECONDS: i64 = 45 * 60;

/// `pace`: where the usage snapshot is (the issue and the bot that keeps its
/// comment, as tracker's usage/config.json names them), and how far over its
/// pace a pool may run, in points of its window, before it takes no run.
#[derive(Clone, Debug, PartialEq)]
pub struct Pace {
    pub repository: String,
    pub issue: u64,
    pub bot: String,
    pub margin_percent: f64,
}

impl Pace {
    pub(super) fn from_json(value: Option<&Value>) -> Result<Option<Self>> {
        let Some(value) = value else {
            return Ok(None);
        };
        let object = value.as_object().context("pace is not an object")?;
        for key in object.keys() {
            ensure!(
                ["repository", "issue", "bot", "marginPercent"].contains(&key.as_str()),
                "unknown pace field {key}"
            );
        }
        let repository = object
            .get("repository")
            .and_then(Value::as_str)
            .context("pace repository is not a string")?;
        validate_repository(repository).context("pace repository")?;
        let issue = object
            .get("issue")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
            .context("pace issue is not an issue number")?;
        let bot = object
            .get("bot")
            .and_then(Value::as_str)
            .context("pace bot is not a string")?;
        ensure!(is_github_login(bot), "pace bot {bot:?} is not a login");
        let margin_percent = match object.get("marginPercent") {
            None => 0.0,
            Some(margin) => margin
                .as_f64()
                .filter(|m| (0.0..=PACE_MARGIN_LIMIT).contains(m))
                .with_context(|| format!("pace marginPercent is not 0 to {PACE_MARGIN_LIMIT}"))?,
        };
        Ok(Some(Self {
            repository: repository.to_owned(),
            issue,
            bot: bot.to_owned(),
            margin_percent,
        }))
    }
}

/// A pool's mode by the snapshot.
#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    /// Not held: the share of its window used and the most it may use now,
    /// in percent; the pool with the most room between them is preferred.
    Normal { used: f64, ceiling: f64 },
    /// Held by the snapshot, or over its pace by more than the margin, until
    /// the time given, where there is one.
    Held { until: Option<OffsetDateTime> },
    /// No window to pace by, as for mistral's monthly dollar allowance until
    /// the snapshot reports it: usable, but never preferred to a paced pool.
    Unpaced,
}

/// The pools of a fresh snapshot by name, or why there is none.
pub type Paces = std::result::Result<BTreeMap<String, Mode>, String>;

/// A pool's mode from its snapshot entry. The snapshot's own `hold` decides
/// (agent-board#233): it knows each pool's `burst` and holds for reasons
/// beyond pace, such as a window or session limit, so recomputing the hold
/// here with one margin for every pool held claude while its burst left it
/// room. Only an entry without `hold`, from an older snapshot, is paced
/// here, by the share of its window used against the share passed.
pub fn mode(entry: &Value, now: OffsetDateTime, margin: f64) -> Mode {
    match entry.get("hold") {
        None => paced(entry, now, margin),
        Some(Value::Bool(false)) => {
            let used = entry["used_percent"].as_f64().unwrap_or(f64::NAN);
            let allowed = entry["allowed_percent"].as_f64().unwrap_or(f64::NAN);
            let ceiling = allowed + entry["burst"].as_f64().unwrap_or(0.0);
            if used.is_finite() && ceiling.is_finite() {
                Mode::Normal { used, ceiling }
            } else {
                Mode::Unpaced
            }
        }
        // `true`, or a hold this does not understand: missing usage holds
        // rather than reading as room.
        Some(_) => Mode::Held {
            until: entry["next_dispatch_at"]
                .as_str()
                .and_then(|at| OffsetDateTime::parse(at, &Rfc3339).ok()),
        },
    }
}

/// An older snapshot's pool, held while it has used more of its window than
/// has passed, plus `margin`.
fn paced(entry: &Value, now: OffsetDateTime, margin: f64) -> Mode {
    let window = entry["window_minutes"].as_f64().unwrap_or(f64::NAN) * 60_000.0;
    let used = entry["used_percent"].as_f64().unwrap_or(f64::NAN);
    let Some(resets) = millis(&entry["resets_at"]) else {
        return Mode::Unpaced;
    };
    if !(window > 0.0 && used.is_finite()) {
        return Mode::Unpaced;
    }
    let now_ms = (now.unix_timestamp_nanos() / 1_000_000) as i64;
    let left = (resets - now_ms) as f64;
    // A window past its reset says nothing of the new one, as the
    // supervisor's pace check skips it too.
    if left <= 0.0 {
        return Mode::Unpaced;
    }
    let elapsed = 100.0 * (1.0 - left / window).clamp(0.0, 1.0);
    if used - elapsed <= margin {
        return Mode::Normal {
            used,
            ceiling: elapsed,
        };
    }
    // Held until the share passed is within the margin of the share used,
    // or the window resets, whichever comes first.
    let caught_up = (used - margin).min(100.0);
    let at = resets as f64 - window * (1.0 - caught_up / 100.0);
    let until = OffsetDateTime::from_unix_timestamp_nanos(at as i128 * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    Mode::Held { until: Some(until) }
}

/// The modes of the snapshot's pools, if it is fresh. A pool whose entry is
/// null, or absent, is unpaced: the snapshot does not pace it, and the line
/// says so, so a pool named in the configuration but not the snapshot shows.
pub fn paces(snapshot: Option<&Value>, now: OffsetDateTime, margin: f64) -> Paces {
    let snapshot = snapshot.ok_or("there is no usage snapshot")?;
    let fresh = (now.unix_timestamp_nanos() / 1_000_000) as i64 - USAGE_STALE_SECONDS * 1000;
    if !millis(&snapshot["updated_at"]).is_some_and(|at| at >= fresh) {
        return Err("the usage snapshot is not fresh".into());
    }
    let pools = snapshot["pools"]
        .as_object()
        .ok_or("the usage snapshot has no pools")?;
    Ok(pools
        .iter()
        .map(|(name, entry)| (name.clone(), mode(entry, now, margin)))
        .collect())
}

/// Of `candidates`, the pool with the most room under its ceiling; an
/// unpaced one only where no other is unheld. A tie goes to `turn`, the pool
/// the round would take without pace, so that pools the snapshot does not
/// tell apart still take turns; else to the configured order. Held pools
/// are skipped, and the reason the run waits is the error.
pub fn choose(
    paces: &Paces,
    candidates: &[String],
    turn: Option<&str>,
) -> std::result::Result<String, String> {
    let paces = paces
        .as_ref()
        .map_err(|why| format!("{why}, so new runs wait"))?;
    let mut best: Option<(&String, f64)> = None;
    let mut held = Vec::new();
    for pool in candidates {
        let room = match paces.get(pool).unwrap_or(&Mode::Unpaced) {
            Mode::Normal { used, ceiling } => ceiling - used,
            Mode::Unpaced => f64::NEG_INFINITY,
            Mode::Held { until } => {
                held.push(format!("{pool}{}", until_minute(*until)));
                continue;
            }
        };
        if best.is_none_or(|(_, most)| room > most || (room == most && turn == Some(pool))) {
            best = Some((pool, room));
        }
    }
    best.map(|(pool, _)| pool.clone())
        .ok_or_else(|| format!("every pool is held ({})", held.join(", ")))
}

/// The pass's line naming each pool's mode, the snapshot's and those the
/// configuration names, in name order.
pub fn line(paces: &Paces, configured: &BTreeSet<String>) -> String {
    let paces = match paces {
        Ok(paces) => paces,
        Err(why) => return format!("pools: {why}, so new runs wait"),
    };
    let names: BTreeSet<&String> = paces.keys().chain(configured).collect();
    let modes: Vec<String> = names
        .into_iter()
        .map(|pool| match paces.get(pool).unwrap_or(&Mode::Unpaced) {
            Mode::Normal { used, ceiling } => format!("{pool} normal {used:.0}%/{ceiling:.0}%"),
            Mode::Held { until } => format!("{pool} held{}", until_minute(*until)),
            Mode::Unpaced => format!("{pool} unpaced"),
        })
        .collect();
    format!("pools: {}", modes.join(", "))
}

/// What a pool setting names to take a tier's pools instead of one pool:
/// `tier-1` is the pools file's pools with `"tier": 1` (agent-board#228).
pub const TIER_PREFIX: &str = "tier-";
/// The highest tier a pool may have in the pools file.
pub const TIER_LIMIT: u64 = 9;

/// The automatic pools of each tier, by tier name, in name order.
pub type Tiers = BTreeMap<String, Vec<String>>;

/// Whether a pool setting names a tier rather than a pool.
pub fn is_tier(name: &str) -> bool {
    name.starts_with(TIER_PREFIX)
}

/// The tiers of the caller's pools file (tracker's pools.json, from its
/// default branch), read only where a setting in `named` names one, so that
/// a configuration naming none needs no file and is as before. Each pool
/// then needs its `tier`; `"auto": false` keeps a pool dispatched only by
/// hand, whose allowance is apart, out of its tier's choice. A tier the file
/// does not give, or gives no automatic pool, is refused: without that, a
/// mistyped tier would go to the dispatch as a pool's name, and a tier of
/// hand-dispatch pools would spend the allowance they keep apart.
pub fn tiers(pools: Option<&Value>, named: &[(&str, &str)]) -> Result<Tiers> {
    let named: Vec<_> = named.iter().filter(|(_, n)| is_tier(n)).collect();
    let Some((setting, tier)) = named.first() else {
        return Ok(Tiers::new());
    };
    let pools = pools
        .with_context(|| format!("{setting} names {tier}, but no pools file gives the tiers"))?
        .as_object()
        .context("pools file is not an object")?;
    let mut tiers = Tiers::new();
    for (pool, entry) in pools {
        ensure!(
            !is_tier(pool),
            "pools file: pool {pool:?} is named as a tier"
        );
        let tier = entry["tier"]
            .as_u64()
            .filter(|t| (1..=TIER_LIMIT).contains(t))
            .with_context(|| format!("pools file: {pool} tier is not 1 to {TIER_LIMIT}"))?;
        let auto = match entry.get("auto") {
            None => true,
            Some(auto) => auto
                .as_bool()
                .with_context(|| format!("pools file: {pool} auto is not a boolean"))?,
        };
        let pools = tiers.entry(format!("{TIER_PREFIX}{tier}")).or_default();
        if auto {
            pools.push(pool.clone());
        }
    }
    for (setting, tier) in named {
        match tiers.get(*tier) {
            None => bail!("{setting} names {tier}, which is not a tier of the pools file"),
            Some(pools) => ensure!(
                !pools.is_empty(),
                "{setting} names {tier}, which has no automatic pool in the pools file"
            ),
        }
    }
    Ok(tiers)
}

/// `names` with each tier in place of its pools, which pace then chooses
/// among as among any configured pools.
pub fn expand(tiers: &Tiers, names: &[String]) -> Vec<String> {
    names
        .iter()
        .flat_map(|name| {
            tiers
                .get(name)
                .cloned()
                .unwrap_or_else(|| vec![name.clone()])
        })
        .collect()
}

/// ` until 2026-10-19T03:24Z`, or nothing for a hold the snapshot gives no end.
fn until_minute(until: Option<OffsetDateTime>) -> String {
    until
        .map(|at| format!(" until {}", minute(at)))
        .unwrap_or_default()
}

/// `2026-10-19T03:24Z`.
fn minute(at: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute()
    )
}

impl<R: Rest, G: Transport> Pass<'_, R, G> {
    /// The pools' modes, read once a pass where `pace` is set.
    pub(super) fn paces(&mut self) -> Option<Paces> {
        let pace = self.config.policy.pace.clone()?;
        if let Some(read) = &self.paces {
            return Some(read.clone());
        }
        let read = self
            .paged(
                &format!(
                    "repos/{}/issues/{}/comments?per_page=100",
                    pace.repository, pace.issue
                ),
                false,
            )
            .map_err(|error| format!("the usage snapshot could not be read ({error:#})"))
            .and_then(|comments| {
                let snapshot = comments
                    .iter()
                    .find(|c| {
                        text(c, "/user/login") == pace.bot
                            && text(c, "/body").starts_with(USAGE_MARKER)
                    })
                    .and_then(|c| usage_snapshot(text(c, "/body")));
                paces(snapshot.as_ref(), self.now, pace.margin_percent)
            });
        self.paces = Some(read.clone());
        Some(read)
    }

    /// The pool a run goes to: `chosen` as configured where `pace` is off,
    /// else the pace's choice of `candidates`, or why the run waits.
    pub(super) fn paced(
        &mut self,
        chosen: Option<String>,
        candidates: &[String],
    ) -> std::result::Result<Option<String>, String> {
        if candidates.is_empty() {
            return Ok(chosen);
        }
        match self.paces() {
            None => Ok(chosen),
            Some(paces) => choose(&paces, candidates, chosen.as_deref()).map(Some),
        }
    }

    /// The pass's pool line, where `pace` is set.
    pub(super) fn pace_line(&mut self) -> Option<String> {
        let paces = self.paces()?;
        let policy = &self.config.policy;
        let kinds: Vec<String> = policy.kind_pools.values().cloned().collect();
        let mut configured: BTreeSet<String> = expand(&policy.tiers, &kinds).into_iter().collect();
        configured.extend(policy.continuation.pools.values().cloned());
        configured.extend(
            [
                &policy.toil_fix_pool,
                &policy.conflict_fix_pool,
                &policy.diagnostic_pool,
            ]
            .into_iter()
            .filter(|p| !p.is_empty())
            .cloned(),
        );
        if let Some(apply) = &self.apply {
            configured.extend(expand(&policy.tiers, &apply.pools));
        }
        Some(line(&paces, &configured))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(stamp: &str) -> OffsetDateTime {
        OffsetDateTime::parse(stamp, &Rfc3339).unwrap()
    }

    const NOW: &str = "2026-10-17T03:24:00Z";
    const WEEK: u64 = 7 * 24 * 60;

    /// A pool `passed` hours into a week, having used `used` percent of it.
    fn pool(used: f64, passed: i64) -> Value {
        let resets = at(NOW) + time::Duration::hours(7 * 24 - passed);
        json!({"used_percent": used, "window_minutes": WEEK, "resets_at": resets.format(&Rfc3339).unwrap()})
    }

    fn snapshot(pools: Value) -> Value {
        json!({"schema": "bot-usage/v1", "updated_at": "2026-10-17T03:00:00Z", "pools": pools})
    }

    fn names(pools: &[&str]) -> Vec<String> {
        pools.iter().map(|p| (*p).to_owned()).collect()
    }

    #[test]
    fn config_is_off_by_default_and_bounded() {
        assert_eq!(Pace::from_json(None).unwrap(), None);
        let good = json!({"repository": "o/ops", "issue": 1, "bot": "worker"});
        let pace = Pace::from_json(Some(&good)).unwrap().unwrap();
        assert_eq!(pace.margin_percent, 0.0);
        for (key, value) in [
            ("repository", json!("not a repository")),
            ("issue", json!(0)),
            ("bot", json!("no such/login")),
            ("marginPercent", json!(-1)),
            ("marginPercent", json!(101)),
            ("other", json!(true)),
        ] {
            let mut bad = good.clone();
            bad[key] = value;
            assert!(Pace::from_json(Some(&bad)).is_err(), "{key}");
        }
        assert!(Pace::from_json(Some(&json!("o/ops#1"))).is_err());
    }

    #[test]
    fn a_pool_over_its_pace_is_held_until_the_window_catches_up() {
        let now = at(NOW);
        // 17 Oct: 26% used against a 4% pace, held until 26% of the week has passed.
        let openai = mode(&pool(26.0, 7), now, 0.0);
        let Mode::Held { until: Some(until) } = openai else {
            panic!("{openai:?}")
        };
        assert_eq!(minute(until), "2026-10-18T16:04Z");
        // Within the margin it may still run, as may a pool under its pace.
        assert!(matches!(
            mode(&pool(26.0, 7), now, 25.0),
            Mode::Normal { .. }
        ));
        assert!(matches!(
            mode(&pool(50.0, 84), now, 0.0),
            Mode::Normal { .. }
        ));
        // Used past the window: held until it resets.
        let Mode::Held { until: Some(until) } = mode(&pool(120.0, 160), now, 0.0) else {
            panic!()
        };
        assert_eq!(minute(until), "2026-10-17T11:24Z");
        // No window or share to pace by, as for a dollar allowance, or a
        // window past its reset.
        for entry in [
            Value::Null,
            json!({"used_percent": 120, "window_minutes": WEEK, "resets_at": "2026-10-17T00:00:00Z"}),
            json!({"spent_usd": 12}),
            json!({"used_percent": 10, "window_minutes": 0, "resets_at": "2026-10-20T00:00:00Z"}),
            json!({"used_percent": 10, "window_minutes": WEEK, "resets_at": "soon"}),
        ] {
            assert_eq!(mode(&entry, now, 0.0), Mode::Unpaced, "{entry}");
        }
    }

    #[test]
    fn the_pool_furthest_under_its_pace_is_chosen_and_held_ones_wait() {
        let now = at(NOW);
        let read = |pools: Value| paces(Some(&snapshot(pools)), now, 0.0);
        for (case, pools, (candidates, turn), expected) in [
            (
                "an openai hold sends the round to claude",
                json!({"claude": pool(40.0, 84), "openai": pool(26.0, 7)}),
                (&["claude", "openai"][..], Some("openai")),
                Ok("claude"),
            ),
            (
                "furthest under pace, whoever's turn",
                json!({"claude": pool(40.0, 84), "openai": pool(10.0, 84)}),
                (&["claude", "openai"], Some("claude")),
                Ok("openai"),
            ),
            (
                "a tie goes to the round's turn",
                json!({"claude": pool(10.0, 84), "openai": pool(10.0, 84)}),
                (&["claude", "openai"], Some("openai")),
                Ok("openai"),
            ),
            (
                "pools the snapshot does not pace take turns",
                json!({}),
                (&["claude", "openai"], Some("openai")),
                Ok("openai"),
            ),
            (
                "else a tie keeps the configured order",
                json!({"claude": pool(10.0, 84), "openai": pool(10.0, 84)}),
                (&["openai", "claude"], None),
                Ok("openai"),
            ),
            (
                "an unpaced pool only where no paced one has room",
                json!({"claude": pool(50.0, 84), "mistral": null}),
                (&["mistral", "claude"], Some("mistral")),
                Ok("claude"),
            ),
            (
                "an unpaced pool rather than a held one",
                json!({"claude": pool(90.0, 84)}),
                (&["claude", "mistral"], Some("claude")),
                Ok("mistral"),
            ),
            (
                "every pool held",
                json!({"claude": pool(90.0, 84), "openai": pool(26.0, 7)}),
                (&["claude", "openai"], Some("claude")),
                Err(
                    "every pool is held (claude until 2026-10-19T22:36Z, openai until 2026-10-18T16:04Z)",
                ),
            ),
        ] {
            let got = choose(&read(pools), &names(candidates), turn);
            assert_eq!(
                got.as_deref(),
                expected.map_err(str::to_owned).as_deref(),
                "{case}"
            );
        }
        // Missing usage holds new runs rather than reading as room.
        let mut old = snapshot(json!({"claude": pool(0.0, 84)}));
        old["updated_at"] = json!("2026-10-17T02:00:00Z");
        for (snap, why) in [
            (None, "there is no usage snapshot"),
            (Some(old), "the usage snapshot is not fresh"),
            (
                Some(json!({"updated_at": "2026-10-17T03:00:00Z"})),
                "the usage snapshot has no pools",
            ),
        ] {
            assert_eq!(
                choose(&paces(snap.as_ref(), now, 0.0), &names(&["claude"]), None),
                Err(format!("{why}, so new runs wait"))
            );
        }
    }

    /// A pool the snapshot paces itself: `hold`, and `burst` where given.
    fn led(used: f64, allowed: f64, burst: Option<f64>, hold: Value) -> Value {
        let mut entry = pool(used, 84);
        entry["allowed_percent"] = json!(allowed);
        entry["hold"] = hold;
        if let Some(burst) = burst {
            entry["burst"] = json!(burst);
        }
        entry
    }

    #[test]
    fn the_snapshots_hold_decides_and_room_counts_its_burst() {
        let now = at(NOW);
        let until = Some(at("2026-10-22T00:33:00Z"));
        let held = |mut entry: Value| {
            entry["next_dispatch_at"] = json!("2026-10-22T00:33:00Z");
            entry
        };
        // The margin is 5, as the poll's on 21 Oct, so only the fallback uses it.
        for (case, entry, expected) in [
            (
                "21 Oct claude: over its pace, under its burst",
                led(70.0, 26.0, Some(95.0), json!(false)),
                Mode::Normal {
                    used: 70.0,
                    ceiling: 121.0,
                },
            ),
            (
                "not held without a burst, even over its pace",
                led(30.0, 20.0, None, json!(false)),
                Mode::Normal {
                    used: 30.0,
                    ceiling: 20.0,
                },
            ),
            (
                "21 Oct openai: held until the snapshot's next dispatch",
                held(led(26.0, 18.0, Some(3.0), json!(true))),
                Mode::Held { until },
            ),
            (
                "held under its pace, for a window or session reason",
                held(led(1.0, 50.0, None, json!(true))),
                Mode::Held { until },
            ),
            (
                "held with no next dispatch",
                led(1.0, 50.0, Some(95.0), json!(true)),
                Mode::Held { until: None },
            ),
            (
                "a hold that is not a boolean holds",
                led(1.0, 50.0, None, json!("no")),
                Mode::Held { until: None },
            ),
            (
                "a null hold holds, unlike a missing one",
                led(1.0, 50.0, None, Value::Null),
                Mode::Held { until: None },
            ),
            (
                "a numeric hold holds",
                led(1.0, 50.0, None, json!(0)),
                Mode::Held { until: None },
            ),
            (
                "not held, with nothing to rank by",
                json!({"hold": false, "spent_usd": 12}),
                Mode::Unpaced,
            ),
        ] {
            assert_eq!(mode(&entry, now, 5.0), expected, "{case}");
        }
        // Without `hold`, as from an older snapshot, the margin paces the
        // pool and its burst is ignored: 26% used, held until 21% has passed.
        let mut older = pool(26.0, 1);
        older["burst"] = json!(95);
        let Mode::Held { until: Some(until) } = mode(&older, now, 5.0) else {
            panic!("{older}")
        };
        assert_eq!(minute(until), "2026-10-18T13:40Z");

        let read = |pools: Value| paces(Some(&snapshot(pools)), now, 5.0);
        let october = json!({
            "claude": led(70.0, 26.0, Some(95.0), json!(false)),
            "openai": held(led(26.0, 18.0, Some(3.0), json!(true))),
        });
        for (case, pools, turn, expected) in [
            (
                "21 Oct: claude, on openai's turn",
                october.clone(),
                "openai",
                Ok("claude"),
            ),
            (
                "the bigger burst's room, whoever's turn",
                json!({"claude": led(70.0, 26.0, Some(95.0), json!(false)),
                    "openai": led(10.0, 18.0, Some(3.0), json!(false))}),
                "openai",
                Ok("claude"),
            ),
            (
                "an older entry ranks by its pace beside a snapshot-led one",
                json!({"claude": led(70.0, 26.0, Some(95.0), json!(false)),
                    "openai": pool(10.0, 120)}),
                "claude",
                Ok("openai"),
            ),
            (
                "every pool held by the snapshot",
                json!({"claude": led(1.0, 50.0, None, json!(true)),
                    "openai": held(led(26.0, 18.0, Some(3.0), json!(true)))}),
                "claude",
                Err("every pool is held (claude, openai until 2026-10-22T00:33Z)"),
            ),
        ] {
            let got = choose(&read(pools), &names(&["claude", "openai"]), Some(turn));
            assert_eq!(
                got.as_deref(),
                expected.map_err(str::to_owned).as_deref(),
                "{case}"
            );
        }
        assert_eq!(
            line(&read(october), &BTreeSet::new()),
            "pools: claude normal 70%/121%, openai held until 2026-10-22T00:33Z"
        );
    }

    #[test]
    fn the_line_names_every_pools_mode() {
        let now = at(NOW);
        let read = paces(
            Some(&snapshot(
                json!({"claude": pool(50.0, 84), "openai": pool(26.0, 7), "mistral": null}),
            )),
            now,
            0.0,
        );
        let configured = BTreeSet::from(["small".to_owned(), "claude".to_owned()]);
        assert_eq!(
            line(&read, &configured),
            "pools: claude normal 50%/50%, mistral unpaced, openai held until 2026-10-18T16:04Z, small unpaced"
        );
        assert_eq!(
            line(&paces(None, now, 0.0), &configured),
            "pools: there is no usage snapshot, so new runs wait"
        );
    }
}
