//! Stage 1 (docs/pass.md): the pools' usage, read from the inference
//! broker's `/usage` and paced as tracker's `usage.cjs` paced it, so that
//! the pass's runs, pull requests' included, pace by a fresh reading rather
//! than by whatever another job last published. An applying pass publishes
//! it to the bot's usage comment, where the review app reads it too.
use super::*;

/// The broker's answer this reads.
const BROKER_SCHEMA: &str = "praxis-broker-usage/v1";
/// The snapshot's pools' `source`.
const SOURCE: &str = "praxis";
/// The hold reason of a pool the broker has no current reading for.
pub(super) const NO_READING: &str = "no-reading";
/// Only a window this long is paced evenly; a shorter one (five hours) is
/// spent in bursts and holds work only once used up.
const MIN_PACED_MINUTES: f64 = 24.0 * 60.0;
/// Anthropic's windows in the broker's answer, by name, with their length.
const ANTHROPIC_WINDOWS: &[(&str, f64)] = &[
    ("unified_5h", 5.0 * 60.0),
    ("unified_7d", 7.0 * 24.0 * 60.0),
];
/// Codex's windows, which give their own length.
const CODEX_WINDOWS: &[&str] = &["primary", "secondary"];

/// The inference broker's `/usage`, read without the forge's token.
pub trait Broker {
    fn usage(&self, url: &str) -> Result<Value>;
}

/// One of a pool's windows as the broker last saw it, times in Unix seconds.
#[derive(Clone, Copy, Debug)]
struct Window {
    used: f64,
    minutes: f64,
    resets: f64,
    observed: f64,
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|n| n.is_finite())
}

/// A time to the millisecond, cut short as `usage.cjs` cut it.
fn instant(seconds: f64) -> Result<OffsetDateTime> {
    ensure!(seconds.is_finite(), "{seconds} is not a time");
    OffsetDateTime::from_unix_timestamp_nanos((seconds * 1e3).trunc() as i128 * 1_000_000)
        .with_context(|| format!("{seconds} is not a time"))
}

fn stamp(seconds: f64) -> Result<String> {
    Ok(instant(seconds)?.format(&Rfc3339)?)
}

fn seconds(at: OffsetDateTime) -> f64 {
    at.unix_timestamp_nanos() as f64 / 1e9
}

/// `pool`'s windows in the broker's answer, or `None` for a pool it does
/// not read: Anthropic's utilization is a fraction with an absolute reset
/// (claude); Codex's a percent, reset some seconds after it was observed
/// (openai).
fn windows(broker: &Value, pool: &str) -> Option<Vec<Window>> {
    let observed = |w: &Value| number(&w["observed_at"]);
    Some(match pool {
        "claude" => ANTHROPIC_WINDOWS
            .iter()
            .filter_map(|(name, minutes)| {
                let w = &broker["anthropic"][name];
                Some(Window {
                    used: number(&w["utilization"])? * 100.0,
                    minutes: *minutes,
                    resets: number(&w["reset"])?,
                    observed: observed(w)?,
                })
            })
            .collect(),
        "openai" => CODEX_WINDOWS
            .iter()
            .filter_map(|name| {
                let w = &broker["codex"][name];
                let observed = observed(w)?;
                Some(Window {
                    used: number(&w["used_percent"])?,
                    minutes: number(&w["window_minutes"]).filter(|m| *m > 0.0)?,
                    resets: observed + number(&w["reset_after_seconds"])?,
                    observed,
                })
            })
            .collect(),
        _ => return None,
    })
}

/// A pool's held entry for want of a reading. Never null or left out: PR
/// follow-up pacing reads both of those as an unpaced pool with room.
fn unread(at: &str) -> Value {
    json!({"source": SOURCE, "computed_at": at, "hold": true,
        "reason": NO_READING, "next_dispatch_at": null})
}

/// A pool's snapshot entry from its windows, held without a window of a
/// day or more that has yet to reset and was read in the last 30 hours. The longest window is paced: its use
/// against [`allowed`], plus `burst` points but never past the target, and a
/// shorter window used up holds until it resets. Its `observed_at` is that
/// window's own, so a recent short-window reading does not freshen it.
fn pace(
    windows: &[Window],
    settings: &PoolSettings,
    (now, at): (OffsetDateTime, &str),
) -> Result<Value> {
    let longest = (0..windows.len()).reduce(|a, b| match windows[b].minutes > windows[a].minutes {
        true => b,
        false => a,
    });
    let Some(index) = longest else {
        return Ok(unread(at));
    };
    let paced = windows[index];
    let resets = instant(paced.resets)?;
    // An old or future reading is none: PR pacing trusts `hold` as it is.
    let age = seconds(now) - paced.observed;
    if paced.minutes < MIN_PACED_MINUTES
        || resets <= now
        || !(-(CLOCK_SKEW_SECONDS as f64)..=OBSERVED_STALE_SECONDS as f64).contains(&age)
    {
        return Ok(unread(at));
    }
    let (goal, burst) = (settings.target * 100.0, settings.burst);
    let allowed = allowed(settings.target, paced.minutes, resets, now);
    let window = paced.minutes * 60.0;
    let mut reason = None;
    let mut next = None;
    if paced.used > (allowed + burst).min(goal) {
        reason = Some("pace");
        next = Some(match paced.used > goal {
            true => paced.resets,
            false => paced.resets - window + (paced.used - burst) / goal * window,
        });
    }
    let now_seconds = seconds(now);
    for (_, spent) in windows
        .iter()
        .enumerate()
        .filter(|(i, w)| *i != index && w.resets > now_seconds && w.used >= 100.0)
    {
        reason.get_or_insert("window");
        next = Some(next.map_or(spent.resets, |n: f64| n.max(spent.resets)));
    }
    Ok(json!({
        "source": SOURCE,
        "observed_at": stamp(paced.observed)?,
        "computed_at": at,
        "used_percent": paced.used,
        "window_minutes": paced.minutes,
        "resets_at": resets.format(&Rfc3339)?,
        "target_percent": goal,
        "burst": burst,
        "allowed_percent": allowed,
        "ahead": paced.used - allowed,
        "hold": reason.is_some(),
        "reason": reason,
        "next_dispatch_at": next.map(stamp).transpose()?,
    }))
}

/// The broker's Mistral counters, which have no window and so no pace:
/// every successful response, metered or not, and its tokens, since the
/// broker started. No allowance or spend: the review app reads that from
/// the pools file.
fn mistral(broker: &Value, at: &str) -> Result<Option<Value>> {
    let counts = &broker["mistral"]["counts"];
    let count = |key: &str| counts[key].as_u64().unwrap_or(0);
    let requests = count("requests").saturating_add(count("unmetered"));
    let Some(tokens) = counts["tokens"]["total"].as_u64() else {
        return Ok(None);
    };
    let mut out = json!({"requests": requests, "tokens": tokens, "observed_at": at});
    if let Some(since) = number(&broker["started_at"]) {
        out["since"] = stamp(since)?.into();
    }
    Ok(Some(out))
}

/// The snapshot's `budget`: `weekly`, and the broker's trailing week of
/// spend at list prices, in dollars, of runs alone (`spend.runs`,
/// praxis-credential-broker#38), so that operators' interactive use neither
/// counts against the board nor, unpriced, holds it. Anything but a week in
/// dollars at list prices is no reading, and so is a broker without `runs`,
/// or a total that leaves runs' requests unpriced or does not say it prices
/// them all: a model missing from the broker's price table would spend
/// unbounded at $0.
///
/// `since` (when metering began) and the unmetered count are published for
/// the review app but hold nothing: spend resets on each broker restart
/// until praxis-credential-broker#35, and holding on a young `since` would
/// stall the board for a week after every deploy.
fn spend(broker: &Value, weekly: f64) -> Value {
    let spend = &broker["spend"];
    let runs = &spend["runs"];
    let week = spend["currency"] == "USD"
        && spend["basis"] == "list"
        && spend["window_days"].as_u64() == Some(7);
    let unpriced = runs["unpriced_requests"].as_u64();
    let total = number(&runs["total"]).filter(|t| week && unpriced == Some(0) && *t >= 0.0);
    let mut out = match total {
        Some(total) => json!({"weekly_usd": weekly, "spent_usd": total,
            "prices": spend["prices"].as_str(), "unpriced_requests": 0}),
        None => {
            let mut none = json!({"weekly_usd": weekly, "spent_usd": null, "reason": NO_READING});
            if let Some(n) = unpriced.filter(|n| week && *n > 0) {
                none["unpriced_requests"] = n.into();
            }
            none
        }
    };
    if let Some(n) = runs["unmetered_requests"].as_u64() {
        out["unmetered_requests"] = n.into();
    }
    if let Some(since) = number(&spend["since"]).and_then(|s| stamp(s).ok()) {
        out["since"] = since.into();
    }
    out
}

/// The `bot-usage/v1` snapshot of the configured pools the broker reads,
/// from its answer at `now`.
pub(super) fn snapshot(broker: &Value, pools: &Pools, now: OffsetDateTime) -> Result<Value> {
    ensure!(
        broker["schema"] == BROKER_SCHEMA,
        "the broker's answer is not {BROKER_SCHEMA}"
    );
    let at = stamp(seconds(now))?;
    let mut paced = serde_json::Map::new();
    for (name, settings) in &pools.0 {
        let entry = match windows(broker, name) {
            Some(windows) => {
                pace(&windows, settings, (now, &at)).with_context(|| format!("{name}'s reading"))?
            }
            None => unread(&at),
        };
        paced.insert(name.clone(), entry);
    }
    let mut out = json!({"schema": USAGE_SCHEMA, "updated_at": at, "pools": paced});
    if let Some(mistral) = mistral(broker, &at)? {
        out["mistral"] = mistral;
    }
    Ok(out)
}

/// The broker's provider for each pool it reads, under which it counts the
/// requests it has sent since it started, as it does Mistral's.
const PROVIDERS: &[(&str, &str)] = &[("claude", "anthropic"), ("openai", "codex")];

/// The pools of `snapshot` without a reading that the broker has sent no
/// request for since it started: it learns a pool's windows only from its
/// responses, so after a restart no reading arrives until a run asks. A pool
/// with requests and still no reading is stale, and stays held; so is one
/// whose count the broker does not give.
pub(super) fn unasked(broker: &Value, snapshot: &Value) -> BTreeSet<String> {
    let unread = |pool: &str| snapshot["pools"][pool]["reason"] == NO_READING;
    let asked = |provider: &str| {
        let counts = &broker[provider]["counts"];
        let count = |key: &str| counts[key].as_u64();
        Some(count("requests")?.saturating_add(count("unmetered").unwrap_or(0)))
    };
    PROVIDERS
        .iter()
        .filter(|(pool, provider)| unread(pool) && asked(provider) == Some(0))
        .map(|(pool, _)| (*pool).to_owned())
        .collect()
}

fn describe(name: &str, entry: &Value) -> String {
    if entry["reason"] == NO_READING {
        return format!("{name}: no reading (held)");
    }
    let percent = |key: &str| entry[key].as_f64().unwrap_or(f64::NAN).round();
    let hold = match entry["hold"] == true {
        true => format!(
            "; holds new work until {} ({})",
            text(entry, "/next_dispatch_at"),
            text(entry, "/reason")
        ),
        false => String::new(),
    };
    format!(
        "{name}: {}% used, pace allows {}%, resets {}, read {}{hold}",
        percent("used_percent"),
        percent("allowed_percent"),
        text(entry, "/resets_at"),
        text(entry, "/observed_at"),
    )
}

/// Each pool's [`describe`], then the budget's where there is one.
fn pools(snapshot: &Value) -> Vec<String> {
    let pools = snapshot["pools"].as_object().into_iter().flatten();
    let mut lines: Vec<String> = pools.map(|(name, entry)| describe(name, entry)).collect();
    let budget = &snapshot["budget"];
    if budget.is_object() {
        let weekly = budget["weekly_usd"].as_f64().unwrap_or(f64::NAN);
        lines.push(match budget["spent_usd"].as_f64() {
            Some(spent) => format!("budget: ${spent:.0} of ${weekly:.0} this week at list prices"),
            None => match budget["unpriced_requests"].as_u64() {
                Some(n) => format!("budget: ${weekly:.0} a week, {n} request(s) unpriced (held)"),
                None => format!("budget: ${weekly:.0} a week, no spend reading (held)"),
            },
        });
    }
    lines
}

/// The comment: the marker, a line per pool, then the JSON that pacing and
/// the review app read.
fn render(snapshot: &Value) -> Result<String> {
    let pools: String = pools(snapshot).iter().map(|p| format!("- {p}\n")).collect();
    Ok(format!(
        "{USAGE_MARKER}\nUsage as of {}, from the inference broker's `/usage`. Written by `agent-board pass`; the review app's ops view reads the JSON below.\n\n{pools}\n```json\n{}\n```\n",
        text(snapshot, "/updated_at"),
        serde_json::to_string_pretty(snapshot)?
    ))
}

impl<C: Rest + Transport + Broker> Scheduler<'_, C> {
    /// Stage 1: read the broker and pace this pass's runs by it; an
    /// applying pass publishes it, one write, and a preview paces by it
    /// unpublished. A failed read is one line,
    /// and pacing falls back to the published snapshot, which holds once
    /// it is stale.
    pub(super) fn refresh(&mut self) {
        let config = self.config;
        let Some(usage) = &config.usage else {
            return;
        };
        let Some(url) = &usage.url else {
            self.line("usage: no usage.url; pacing reads the published snapshot".into());
            return;
        };
        let read = self.client.usage(url).and_then(|broker| {
            let mut snapshot = snapshot(&broker, &config.pools, self.now)?;
            if let Some(budget) = &config.budget {
                snapshot["budget"] = spend(&broker, budget.weekly_usd);
            }
            Ok((unasked(&broker, &snapshot), snapshot))
        });
        let snapshot = match read {
            Ok((unasked, snapshot)) => {
                if usage.probe {
                    self.probes = unasked;
                }
                snapshot
            }
            Err(error) => {
                // Left unset, the published snapshot is read when pacing needs it.
                self.report.failures += 1;
                self.line(format!(
                    "usage error: reading the broker: {error:#}; pacing reads the published snapshot"
                ));
                return;
            }
        };
        self.line(format!("usage: {}", pools(&snapshot).join("; ")));
        self.snapshot = Some(Some(snapshot.clone()));
        let said = format!("usage: publish to {}#{}", usage.repository, usage.issue);
        if let Some(held) = self.caps.check(Kind::Write) {
            self.line(format!("{held}: {said}"));
            return;
        }
        if !self.apply {
            self.caps.count(Kind::Write);
            self.line(format!("{said}; a preview paces by it unpublished"));
            return;
        }
        let comment = self.usage_comment();
        match comment.and_then(|c| self.publish(usage, c.as_ref(), &snapshot)) {
            Ok(url) => self.line(format!("usage: published {url}")),
            Err(error) => {
                self.report.failures += 1;
                self.line(format!("usage error: {said}: {error:#}"));
            }
        }
    }

    /// Write `snapshot` to the bot's usage comment, `comment`, or a new one,
    /// only on a private repository: it says what the subscriptions are
    /// used for, and when. Its URL.
    fn publish(&self, usage: &Usage, comment: Option<&Value>, snapshot: &Value) -> Result<String> {
        let repo = &usage.repository;
        let response = self.client.get(&format!("{API_ROOT}repos/{repo}"), None)?;
        ensure!(
            response.status == 200,
            "reading {repo}: GitHub HTTP status {}",
            response.status
        );
        ensure!(
            response.body["private"] == true,
            "{repo} is not private; the usage snapshot is not written there"
        );
        let body = json!({"body": render(snapshot)?});
        let (method, url) = match comment {
            Some(comment) => {
                let id = comment["id"]
                    .as_u64()
                    .context("the usage comment has no id")?;
                (
                    "PATCH",
                    format!("{API_ROOT}repos/{repo}/issues/comments/{id}"),
                )
            }
            None => {
                // Only the bot's comment is found again: under another
                // login, each pass would add one more.
                let account = self.client.get(&format!("{API_ROOT}user"), None)?;
                let login = text(&account.body, "/login");
                ensure!(
                    login.eq_ignore_ascii_case(&self.config.bot),
                    "the token is {login:?}'s, not {}'s; not adding a comment it would not find again",
                    self.config.bot
                );
                let url = format!("{API_ROOT}repos/{repo}/issues/{}/comments", usage.issue);
                ("POST", url)
            }
        };
        self.caps.count(Kind::Write);
        let (status, answer) = self.client.send(method, &url, &body)?;
        ensure!(
            (200..300).contains(&status),
            "{method} the usage comment: GitHub HTTP status {status}"
        );
        Ok(text(&answer, "/html_url").to_owned())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// The broker's answer on 23 October, its times moved to the recorded
    /// board's clock ([`super::super::tests::replay`]): claude 18% of its
    /// week and 11% of five hours, openai 3% of a week that began this
    /// morning.
    pub(in crate::pass) fn broker() -> Value {
        json!({
            "schema": "praxis-broker-usage/v1", "started_at": 1792679468,
            "anthropic": {
                "unified_5h": {"utilization": 0.11, "reset": 1792774800, "status": "allowed", "observed_at": 1792768000},
                "unified_7d": {"utilization": 0.18, "reset": 1793055600, "status": "allowed", "observed_at": 1792768000},
            },
            "codex": {
                "primary": {"used_percent": 3.0, "window_minutes": 10080, "reset_after_seconds": 593600, "observed_at": 1792768000},
                "secondary": null,
            },
            "mistral": {"counts": {"requests": 2, "unmetered": 1,
                "tokens": {"input": 1, "cache_read": 0, "output": 1, "reasoning": 0, "total": 2}}},
        })
    }

    pub(in crate::pass) fn now() -> OffsetDateTime {
        OffsetDateTime::parse("2026-10-23T15:15:10.452609311Z", &Rfc3339).unwrap()
    }

    pub(in crate::pass) fn pools() -> Pools {
        let pools = json!({"claude": {"target": 0.95, "burst": 95}, "openai": {"target": 0.95, "burst": 3}});
        serde_json::from_value(pools).unwrap()
    }

    /// Each pool's hold, reason and next dispatch from the broker's answer with `edits` (JSON pointer, value).
    /// The expected values are what tracker's usage.cjs computes from the
    /// same answers.
    #[test]
    fn pace_from_recorded_answers() {
        type Pool = Option<(bool, Option<&'static str>, Option<&'static str>)>;
        type Case = (&'static str, Vec<(&'static str, Value)>, Pool, Pool);
        let free: Pool = Some((false, None, None));
        let unread: Pool = Some((true, Some(NO_READING), None));
        let codex_spent = |reset_after: u64| {
            vec![
                (
                    "/codex/secondary",
                    json!({"used_percent": 10, "window_minutes": 10080,
                    "reset_after_seconds": 593600, "observed_at": 1792768000}),
                ),
                (
                    "/codex/primary",
                    json!({"used_percent": 100, "window_minutes": 300,
                    "reset_after_seconds": reset_after, "observed_at": 1792768000}),
                ),
            ]
        };
        let cases: Vec<Case> = vec![
            ("as recorded", vec![], free, free),
            (
                "openai over pace and burst",
                vec![("/codex/primary/used_percent", json!(10))],
                free,
                Some((true, Some("pace"), Some("2026-10-24T00:22:44.21Z"))),
            ),
            (
                "claude over target, past its burst",
                vec![("/anthropic/unified_7d/utilization", json!(0.96))],
                Some((true, Some("pace"), Some("2026-10-26T23:00:00Z"))),
                free,
            ),
            (
                "claude's five hours used up",
                vec![("/anthropic/unified_5h/utilization", json!(1.0))],
                Some((true, Some("window"), Some("2026-10-23T17:00:00Z"))),
                free,
            ),
            (
                "openai over pace, five hours used up before it catches up",
                codex_spent(1200),
                free,
                Some((true, Some("pace"), Some("2026-10-24T00:22:44.21Z"))),
            ),
            (
                "claude over target with five hours used up: the later",
                vec![
                    ("/anthropic/unified_7d/utilization", json!(0.96)),
                    ("/anthropic/unified_5h/utilization", json!(1.0)),
                ],
                Some((true, Some("pace"), Some("2026-10-26T23:00:00Z"))),
                free,
            ),
            (
                "openai over pace, five hours used up past it",
                codex_spent(36000),
                free,
                Some((true, Some("pace"), Some("2026-10-24T01:06:40Z"))),
            ),
            (
                "claude with five hours only, as after a restart",
                vec![("/anthropic/unified_7d", Value::Null)],
                unread,
                free,
            ),
            (
                "claude's week reset since its reading",
                vec![("/anthropic/unified_7d/reset", json!(1792768000))],
                unread,
                free,
            ),
            ("no codex", vec![("/codex", Value::Null)], free, unread),
            (
                "claude's week read 31 hours ago",
                vec![(
                    "/anthropic/unified_7d/observed_at",
                    json!(1792768000 - 31 * 3600),
                )],
                unread,
                free,
            ),
            (
                "openai's week read an hour from now",
                vec![("/codex/primary/observed_at", json!(1792768000 + 3600))],
                free,
                unread,
            ),
        ];
        for (name, edits, claude, openai) in cases {
            let mut broker = broker();
            for (pointer, value) in edits {
                *broker.pointer_mut(pointer).unwrap() = value;
            }
            let snap = snapshot(&broker, &pools(), now()).unwrap();
            let got = |pool: &str| {
                let e = &snap["pools"][pool];
                let hold = (
                    e["hold"] == true,
                    e["reason"].as_str(),
                    e["next_dispatch_at"].as_str(),
                );
                e.is_object().then_some(hold)
            };
            assert_eq!((got("claude"), got("openai")), (claude, openai), "{name}");
        }
    }

    /// A pool without a current reading, here one missing and one stale,
    /// holds PR follow-up pacing too, which trusts the published `hold`.
    #[test]
    fn unread_pools_hold_pr_pacing() {
        use crate::poll::pace::{choose, paces};
        let mut broker = broker();
        *broker.pointer_mut("/anthropic/unified_7d").unwrap() = Value::Null;
        *broker.pointer_mut("/codex/primary/observed_at").unwrap() = json!(1792768000 - 31 * 3600);
        let snap = snapshot(&broker, &pools(), now()).unwrap();
        let paces = paces(Some(&snap), now(), 0.0, &[]);
        let pools = ["claude".to_owned(), "openai".to_owned()];
        let why = choose(&paces, &pools, None).unwrap_err();
        assert!(why.contains("claude") && why.contains("openai"), "{why}");
    }

    /// The snapshot's `budget`, which the review app reads: the broker's
    /// week of runs at list prices in dollars, or no reading, never $0.
    #[test]
    fn spend_of_the_week() {
        let operator = json!({"total": 88.1, "unpriced_requests": 0, "unmetered_requests": 0});
        let week = json!({"currency": "USD", "basis": "list", "prices": "2026-10-20",
            "window_days": 7, "since": 1_792_454_400, "total": 500.47,
            "unpriced_requests": 0, "unmetered_requests": 3, "by_operator": {"alice": operator},
            "runs": {"total": 412.37, "unpriced_requests": 0, "unmetered_requests": 3}});
        let extras = json!({"since": "2026-10-20T00:00:00Z", "unmetered_requests": 3});
        let with = |mut budget: Value| {
            for (key, value) in extras.as_object().unwrap() {
                budget[key] = value.clone();
            }
            budget
        };
        let read = with(json!({"weekly_usd": 1000.0, "spent_usd": 412.37,
            "prices": "2026-10-20", "unpriced_requests": 0}));
        let bare = json!({"weekly_usd": 1000.0, "spent_usd": null, "reason": NO_READING});
        let none = with(bare.clone());
        let mut unpriced = none.clone();
        unpriced["unpriced_requests"] = 2.into();
        let mut unsince = read.clone();
        unsince.as_object_mut().unwrap().remove("since");
        for (edit, expected) in [
            (None, &read),
            (Some(("/currency", json!("EUR"))), &none),
            (Some(("/basis", json!("effective"))), &none),
            (Some(("/window_days", json!(30))), &none),
            (Some(("/runs/total", json!(-1))), &none),
            (Some(("/runs/total", json!("412"))), &none),
            (Some(("/runs/unpriced_requests", json!(2))), &unpriced),
            (Some(("/runs/unpriced_requests", Value::Null)), &none),
            (Some(("/runs/unpriced_requests", json!("0"))), &none),
            (Some(("/runs/unpriced_requests", json!(-1))), &none),
            (Some(("/runs/unpriced_requests", json!(0.5))), &none),
            // An operator's interactive use, priced or not, is not the board's.
            (Some(("/unpriced_requests", json!(5))), &read),
            (
                Some(("/by_operator/alice/unpriced_requests", json!(5))),
                &read,
            ),
            (Some(("/total", json!(5000))), &read),
            // `since` is three days before `now()` and holds nothing; one
            // that is not a time is left out.
            (Some(("/since", json!(1e15))), &unsince),
            (Some(("/since", json!("today"))), &unsince),
        ] {
            let mut broker = broker();
            broker["spend"] = week.clone();
            if let Some((pointer, value)) = &edit {
                *broker["spend"].pointer_mut(pointer).unwrap() = value.clone();
            }
            assert_eq!(&spend(&broker, 1000.0), expected, "{edit:?}");
        }
        let mut norunsince = bare.clone();
        norunsince["since"] = extras["since"].clone();
        for (parent, key, expected) in [
            ("/runs", "unpriced_requests", &none),
            ("", "runs", &norunsince),
        ] {
            let mut broker = broker();
            broker["spend"] = week.clone();
            let object = broker["spend"].pointer_mut(parent).unwrap();
            object.as_object_mut().unwrap().remove(key);
            assert_eq!(&spend(&broker, 1000.0), expected, "{parent} without {key}");
        }
        assert_eq!(spend(&self::broker(), 1000.0), bare);
        let mut snap = snapshot(&broker(), &pools(), now()).unwrap();
        for (budget, line) in [
            (read, "- budget: $412 of $1000 this week at list prices\n"),
            (
                unpriced,
                "- budget: $1000 a week, 2 request(s) unpriced (held)\n",
            ),
        ] {
            snap["budget"] = budget;
            let body = render(&snap).unwrap();
            assert!(body.contains(line), "{body}");
        }
    }

    /// The whole snapshot from the recorded answer: the shape pacing and the
    /// review app read, each pool's own observation time, and Mistral's
    /// counters without an allowance.
    #[test]
    fn snapshot_of_the_recorded_answer() {
        let snap = snapshot(&broker(), &pools(), now()).unwrap();
        let allowed = snap["pools"]["claude"]["allowed_percent"].as_f64().unwrap();
        // usage.cjs: 49.90491557539683, at the millisecond.
        assert!((allowed - 49.904_915_575).abs() < 1e-6, "{allowed}");
        let mut claude = snap["pools"]["claude"].clone();
        claude["allowed_percent"] = json!(0);
        claude["ahead"] = json!(0);
        assert_eq!(
            claude,
            json!({"source": "praxis", "observed_at": "2026-10-23T15:06:40Z",
                "computed_at": "2026-10-23T15:15:10.452Z", "used_percent": 18.0,
                "window_minutes": 10080.0, "resets_at": "2026-10-26T23:00:00Z",
                "target_percent": 95.0, "burst": 95.0, "allowed_percent": 0, "ahead": 0,
                "hold": false, "reason": null, "next_dispatch_at": null})
        );
        assert_eq!(
            snap["mistral"],
            json!({"requests": 3, "tokens": 2, "observed_at": "2026-10-23T15:15:10.452Z",
                "since": "2026-10-22T14:31:08Z"})
        );
        let body = render(&snap).unwrap();
        assert!(body.starts_with(USAGE_MARKER), "{body}");
        assert_eq!(usage_snapshot(&body), Some(snap));
        let mut other = broker();
        other["schema"] = json!("praxis-broker-usage/v2");
        let error = snapshot(&other, &pools(), now()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the broker's answer is not praxis-broker-usage/v1"
        );
    }
}
