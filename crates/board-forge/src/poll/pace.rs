//! `pace`: each run's pool follows the usage snapshot (tracker's usage.yml,
//! the comment the supervisor's pace check reads), so that a pool that has
//! used more of its window than has passed takes no new run
//! (agent-board#135). Without it, fix rounds alternated `fixPools` blind: on
//! 17 Oct the openai pool had used 26% of a fresh 7-day window, against a 4%
//! pace, and the poll kept starting rounds there.
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
    /// Within its pace: the shares of its window used and passed, in percent.
    Normal { used: f64, elapsed: f64 },
    /// Over its pace by more than the margin, until the window catches up.
    Held { until: OffsetDateTime },
    /// No window to pace by, as for mistral's monthly dollar allowance until
    /// the snapshot reports it: usable, but never preferred to a paced pool.
    Unpaced,
}

/// The pools of a fresh snapshot by name, or why there is none.
pub type Paces = std::result::Result<BTreeMap<String, Mode>, String>;

/// A pool's mode from its snapshot entry, by the share of its window used
/// against the share passed, as the supervisor's pace check compares them.
pub fn mode(entry: &Value, now: OffsetDateTime, margin: f64) -> Mode {
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
        return Mode::Normal { used, elapsed };
    }
    // Held until the share passed is within the margin of the share used,
    // or the window resets, whichever comes first.
    let caught_up = (used - margin).min(100.0);
    let at = resets as f64 - window * (1.0 - caught_up / 100.0);
    let until = OffsetDateTime::from_unix_timestamp_nanos(at as i128 * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    Mode::Held { until }
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

/// Of `candidates`, the pool furthest under its pace; an unpaced one only
/// where no paced one is within its pace. A tie goes to `turn`, the pool
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
            Mode::Normal { used, elapsed } => elapsed - used,
            Mode::Unpaced => f64::NEG_INFINITY,
            Mode::Held { until } => {
                held.push(format!("{pool} until {}", minute(*until)));
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
            Mode::Normal { used, elapsed } => format!("{pool} normal {used:.0}%/{elapsed:.0}%"),
            Mode::Held { until } => format!("{pool} held until {}", minute(*until)),
            Mode::Unpaced => format!("{pool} unpaced"),
        })
        .collect();
    format!("pools: {}", modes.join(", "))
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
        let mut configured: BTreeSet<String> = policy.kind_pools.values().cloned().collect();
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
            configured.extend(apply.pools.iter().cloned());
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
        let Mode::Held { until } = openai else {
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
        let Mode::Held { until } = mode(&pool(120.0, 160), now, 0.0) else {
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
