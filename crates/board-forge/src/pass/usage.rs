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

/// The broker's grant this reads (praxis-credential-broker#41).
const GRANT_SCHEMA: &str = "praxis-broker-grant/v1";
/// The snapshot's pools' `source` where the grant paces them.
pub(super) const GRANT_SOURCE: &str = "grant";
/// The hold reason of a pool its grant holds.
const GRANT_HOLD: &str = "grant";
/// The hold reason of every pool when the grant cannot be read.
const NO_GRANT: &str = "no-grant";
/// How old a grant may be: the broker itself calls a reading stale past one
/// five-hour window.
const GRANT_STALE_SECONDS: f64 = 5.0 * 3600.0;

/// The inference broker's `/usage`, and its grant for this board's consumer
/// (`budget.url`), read without the forge's token.
pub trait Broker {
    fn usage(&self, url: &str) -> Result<Value>;
    fn grant(&self, url: &str) -> Result<Value>;
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
    // Published, so the review app shows the pass's limit, not its own.
    let limit = (allowed + burst).min(goal);
    let window = paced.minutes * 60.0;
    let mut reason = None;
    let mut next = None;
    if paced.used > limit {
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
        "limit_percent": limit,
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

/// A pool's snapshot entry from `grant`, the broker's answer for this
/// board's consumer, on its grant pool `on`: the use and allowance of its
/// window with the least room, which ranks it.
///
/// No grant, a grant that holds, or a pool it lacks (no share) holds, and is
/// never probed, and so does a window that has used what it allows or gives
/// no figures, or a grant without a time or with one in the future. Short of
/// that, a pool the grant calls stale, whose holds are then the staleness, is
/// no reading: held, and probed where `usage.probe` lets it, as the broker
/// learns use only from requests. Otherwise the pool holds when it or any
/// window says `hold` or it has no windows, however old the grant; one with
/// none of those but not observed in the last [`GRANT_STALE_SECONDS`] is no
/// reading.
fn granted(grant: Option<&Value>, on: &str, (now, at): (OffsetDateTime, &str)) -> Result<Value> {
    let held = |reason: &str| {
        json!({"source": GRANT_SOURCE, "grant_pool": on, "computed_at": at,
            "hold": true, "reason": reason, "next_dispatch_at": null})
    };
    let Some(grant) = grant else {
        return Ok(held(NO_GRANT));
    };
    let pool = &grant["pools"][on];
    if grant["hold"] != false || !pool.is_object() {
        return Ok(held(GRANT_HOLD));
    }
    let observed = number(&grant["observed_at"]);
    let age = observed.map(|o| seconds(now) - o);
    let fresh = age.is_some_and(|a| a <= GRANT_STALE_SECONDS);
    // Held whatever else the grant says, stale or not.
    let mut firm = age.is_none_or(|a| a < -(CLOCK_SKEW_SECONDS as f64));
    let windows = pool["windows"].as_object().filter(|w| !w.is_empty());
    let mut flagged = pool["hold"] != false || pool["stale"] != false || windows.is_none();
    let mut least: Option<(&String, f64, f64)> = None;
    for (name, window) in windows.into_iter().flatten() {
        let allowed = number(&window["allowed_percent"]);
        let used = number(&window["used_percent"]);
        let (Some(allowed), Some(used)) = (allowed, used) else {
            firm = true;
            continue;
        };
        flagged |= window["hold"] != false;
        firm |= used >= allowed;
        if least.is_none_or(|(_, a, u)| allowed - used < a - u) {
            least = Some((name, allowed, used));
        }
    }
    if !firm && (pool["stale"] == true || !(flagged || fresh)) {
        let mut entry = unread(at);
        entry["grant_pool"] = on.into();
        return Ok(entry);
    }
    let held = firm || flagged;
    Ok(json!({
        "source": GRANT_SOURCE,
        "grant_pool": on,
        "window": least.map(|(name, _, _)| name),
        "observed_at": observed.and_then(|o| stamp(o).ok()),
        "computed_at": at,
        "used_percent": least.map(|(_, _, used)| used),
        "allowed_percent": least.map(|(_, allowed, _)| allowed),
        "hold": held,
        "reason": held.then_some(GRANT_HOLD),
        "next_dispatch_at": null,
    }))
}

/// `snapshot`'s pools, each on the grant pool `budget.pools` maps it to,
/// paced by `grant` in place of the broker's windows: every one held where
/// the grant could not be read.
pub(super) fn grants(
    snapshot: &mut Value,
    grant: Option<&Value>,
    budget: &WeeklyBudget,
    now: OffsetDateTime,
) -> Result<()> {
    let at = text(snapshot, "/updated_at").to_owned();
    let pools = snapshot["pools"]
        .as_object_mut()
        .context("the usage snapshot has no pools")?;
    for (name, entry) in pools {
        let on = budget.pools.get(name).map_or("", String::as_str);
        *entry = granted(grant, on, (now, &at)).with_context(|| format!("{name}'s grant"))?;
    }
    Ok(())
}

/// A snapshot of `pools` with nothing read, for [`grants`] to fill in when
/// the broker's `/usage` cannot be read.
fn bare(pools: &Pools, now: OffsetDateTime) -> Result<Value> {
    let at = stamp(seconds(now))?;
    let pools: serde_json::Map<String, Value> = (pools.0.iter())
        .map(|(name, _)| (name.clone(), unread(&at)))
        .collect();
    Ok(json!({"schema": USAGE_SCHEMA, "updated_at": at, "pools": pools}))
}

/// The broker's provider for each pool it reads, under which it counts the
/// requests it has sent since it started, as it does Mistral's. A grant
/// pool is a provider too.
const PROVIDERS: &[(&str, &str)] = &[("claude", "anthropic"), ("openai", "codex")];

/// The pools of `snapshot` without a reading that the broker has sent no
/// request for since it started: it learns a pool's windows only from its
/// responses, so after a restart no reading arrives until a run asks. A pool
/// with requests and still no reading is stale, and stays held; so is one
/// whose count the broker does not give. Each with its provider, which
/// pools on one grant pool share: one run on any of them reads them all.
pub(super) fn unasked(broker: &Value, snapshot: &Value) -> BTreeMap<String, String> {
    let asked = |provider: &str| {
        let counts = &broker[provider]["counts"];
        let count = |key: &str| counts[key].as_u64();
        Some(count("requests")?.saturating_add(count("unmetered").unwrap_or(0)))
    };
    let pools = snapshot["pools"].as_object().into_iter().flatten();
    pools
        .filter(|(_, entry)| entry["reason"] == NO_READING)
        .filter_map(|(pool, entry)| {
            let provider = match entry["grant_pool"].as_str() {
                Some(on) => Some(on),
                None => PROVIDERS.iter().find(|(p, _)| p == pool).map(|(_, on)| *on),
            };
            let provider = provider.filter(|on| asked(on) == Some(0))?;
            Some((pool.clone(), provider.to_owned()))
        })
        .collect()
}

fn describe(name: &str, entry: &Value) -> String {
    if entry["reason"] == NO_READING {
        return format!("{name}: no reading (held)");
    }
    let percent = |key: &str| entry[key].as_f64().unwrap_or(f64::NAN).round();
    if entry["source"] == GRANT_SOURCE {
        if !entry["used_percent"].is_number() {
            let reason = text(entry, "/reason");
            return format!("{name}: held by the grant ({reason})");
        }
        let held = match entry["hold"] == true {
            true => "; holds new work (grant)",
            false => "",
        };
        return format!(
            "{name}: {}% used of {}% granted on {} {}, read {}{held}",
            percent("used_percent"),
            percent("allowed_percent"),
            text(entry, "/grant_pool"),
            text(entry, "/window"),
            text(entry, "/observed_at"),
        );
    }
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
    /// it is stale; with a grant, to the grant alone.
    pub(super) fn refresh(&mut self) {
        let config = self.config;
        let Some(usage) = &config.usage else {
            return;
        };
        let Some(url) = &usage.url else {
            self.line("usage: no usage.url; pacing reads the published snapshot".into());
            return;
        };
        let grant = config.grant().map(|budget| (budget, self.grant(budget)));
        let mut read = self.client.usage(url).and_then(|broker| {
            let mut snapshot = snapshot(&broker, &config.pools, self.now)?;
            if let Some((budget, grant)) = &grant {
                grants(&mut snapshot, grant.as_ref(), budget, self.now)?;
            } else if let Some(weekly) = config.budget.as_ref().and_then(|b| b.weekly_usd) {
                snapshot["budget"] = spend(&broker, weekly);
            }
            Ok((unasked(&broker, &snapshot), snapshot))
        });
        // With a grant, pacing never falls back to the published snapshot,
        // which may be paced otherwise: the grant decides, or holds.
        if let (Err(error), Some((budget, grant))) = (&read, &grant) {
            self.report.failures += 1;
            self.line(format!(
                "usage error: reading the broker: {error:#}; pacing reads the grant"
            ));
            read = bare(&config.pools, self.now).and_then(|mut snapshot| {
                grants(&mut snapshot, grant.as_ref(), budget, self.now)?;
                Ok((BTreeMap::new(), snapshot))
            });
        }
        let snapshot = match read {
            Ok((unasked, snapshot)) => {
                let mut snapshot = snapshot;
                if usage.probe {
                    snapshot["probes"] = json!(unasked);
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
        let on = (usage.repository.as_str(), usage.issue);
        let comment = self.bot_comment(on, USAGE_MARKER, "usage snapshot");
        let published = comment.and_then(|c| {
            let body = render(&snapshot)?;
            self.publish(on, c.as_ref(), "usage", body)
        });
        match published {
            Ok(url) => self.line(format!("usage: published {url}")),
            Err(error) => {
                self.report.failures += 1;
                self.line(format!("usage error: {said}: {error:#}"));
            }
        }
    }

    /// The broker's grant at `budget.url`; one it cannot read, or of another
    /// schema, is a failure, and none, which holds every pool.
    fn grant(&mut self, budget: &WeeklyBudget) -> Option<Value> {
        let url = budget.url.as_deref().unwrap_or_default();
        let read = self.client.grant(url).and_then(|grant| {
            ensure!(
                grant["schema"] == GRANT_SCHEMA,
                "the grant is not {GRANT_SCHEMA}"
            );
            Ok(grant)
        });
        match read {
            Ok(grant) => Some(grant),
            Err(error) => {
                self.report.failures += 1;
                self.line(format!(
                    "usage error: reading the grant: {error:#}; every pool holds"
                ));
                None
            }
        }
    }

    /// Write `body` to the bot's `name` comment on issue `on` (repository
    /// and number), `comment`, or a new one, only on a private repository:
    /// the usage snapshot says what the subscriptions are used for, and
    /// when, the hold snapshot what the board's private issues wait on, and
    /// the maintain plan what a private digest reviews. Its URL.
    pub(in crate::pass) fn publish(
        &self,
        on: (&str, u64),
        comment: Option<&Value>,
        name: &str,
        body: String,
    ) -> Result<String> {
        let repo = on.0;
        let response = self.client.get(&format!("{API_ROOT}repos/{repo}"), None)?;
        ensure!(
            response.status == 200,
            "reading {repo}: GitHub HTTP status {}",
            response.status
        );
        ensure!(
            response.body["private"] == true,
            "{repo} is not private; the {name} snapshot is not written there"
        );
        self.write_comment(on, comment, name, body)
    }

    /// [`Self::publish`] without the private check, for what may be public.
    pub(in crate::pass) fn write_comment(
        &self,
        on: (&str, u64),
        comment: Option<&Value>,
        name: &str,
        body: String,
    ) -> Result<String> {
        let (repo, issue) = on;
        let body = json!({"body": body});
        let (method, url) = match comment {
            Some(comment) => {
                let id = comment["id"]
                    .as_u64()
                    .with_context(|| format!("the {name} comment has no id"))?;
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
                let url = format!("{API_ROOT}repos/{repo}/issues/{issue}/comments");
                ("POST", url)
            }
        };
        self.caps.count(Kind::Write);
        let (status, answer) = self.client.send(method, &url, &body)?;
        ensure!(
            (200..300).contains(&status),
            "{method} the {name} comment: GitHub HTTP status {status}"
        );
        Ok(text(&answer, "/html_url").to_owned())
    }

    /// Publishes stages 3 and 4's holds where [`Config::holds`] is set: one
    /// write step, skipped where the bot's hold comment already has the
    /// same holds. A failure is one line, and the pass goes on.
    pub(in crate::pass) fn publish_holds(&mut self) {
        let Some(at) = &self.config.holds else {
            return;
        };
        // That pass considered the candidates, and publishes their holds.
        if let Some(why) = self.other_holds() {
            self.line(format!("holds: not published; {why}"));
            return;
        }
        // Stage 4 may have stopped on these, which is not a failure.
        let bounds = self.client.bounds;
        if bounds.left.get() < WRITE_RESERVE + HOLDS_READS || Instant::now() > bounds.admit_by {
            let why = "the pass's request budget or time is spent";
            self.line(format!("holds: not published; {why}"));
            return;
        }
        let on = (at.repository.as_str(), at.issue);
        let said = format!("holds: publish to {}#{}", at.repository, at.issue);
        // The published snapshot first: its holds' times carry over.
        let comment = self.bot_comment(on, holds::HOLDS_MARKER, "hold snapshot");
        let changed = comment.and_then(|comment| {
            let published = comment.as_ref().map(|c| text(c, "/body"));
            let snapshot = self.hold_snapshot(published.and_then(holds::published).as_ref());
            let (snapshot, body) = holds::fit(snapshot)?;
            Ok((holds::changed(published, &snapshot)?, body, comment))
        });
        let (body, comment) = match changed {
            Ok((true, body, comment)) => (body, comment),
            Ok((false, ..)) => {
                self.line("holds: unchanged".into());
                return;
            }
            Err(error) => {
                self.report.failures += 1;
                self.line(format!("holds error: {said}: {error:#}"));
                return;
            }
        };
        if let Some(held) = self.caps.check(Kind::Write) {
            self.line(format!("{held}: {said}"));
            return;
        }
        if !self.apply {
            self.caps.count(Kind::Write);
            self.line(format!("{said}; a preview does not write it"));
            return;
        }
        let published = self.publish(on, comment.as_ref(), "hold", body);
        match published {
            Ok(url) => self.line(format!("holds: published {url}")),
            Err(error) => {
                self.report.failures += 1;
                self.line(format!("holds error: {said}: {error:#}"));
            }
        }
    }

    /// Stages 3 and 4's holds as a snapshot at this pass's clock, with what
    /// carries over from `previous`, the published one's JSON, and the
    /// stall alarms, each said in a `stall …` line ([`holds::alarm`]).
    fn hold_snapshot(&mut self, previous: Option<&Value>) -> holds::Snapshot {
        let stamp = self.now.format(&Rfc3339).unwrap_or_default();
        let tally = self.report.tally.clone();
        let mut snapshot = holds::snapshot(&self.report.holds, stamp, tally);
        snapshot.leases = self.report.leases.clone();
        let started = snapshot.pass.started.saturating_sub(self.not_started);
        let progressed = self.apply && (started > 0 || self.progressed > 0);
        let considered = snapshot.pass.candidates.is_some();
        holds::carry(&mut snapshot, previous, (progressed, considered));
        let Some(at) = &self.config.holds else {
            return snapshot;
        };
        let on = ItemRef {
            host: DEFAULT_HOST.into(),
            repository: at.repository.clone(),
            number: at.issue,
        };
        let top = (self.top.clone()).unwrap_or_else(|| DEFAULT_TOP.map(str::to_owned).to_vec());
        let default = holds::Stall::default();
        let watch = holds::Watch {
            stall: self.config.stall.as_ref().unwrap_or(&default),
            top: &top,
            on: &on,
            log: (self.own).map(|own| {
                format!(
                    "https://{DEFAULT_HOST}/{}/actions/runs/{}",
                    own.repository, own.id
                )
            }),
            free: self.free,
        };
        for line in holds::alarm(&mut snapshot, previous, &watch) {
            self.line(line);
        }
        snapshot
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

    /// The sample grant of praxis-credential-broker#41, observed when
    /// [`broker`]'s answer was: claude has 13 points of room on anthropic's
    /// week, openai 26 on codex's.
    pub(in crate::pass) fn grant() -> Value {
        let window = |share: f64, allowed: f64, used: f64, resets: Value| {
            json!({"share_percent": share, "allowed_percent": allowed, "used_percent": used,
                "resets_at": resets, "hold": false})
        };
        json!({
            "schema": "praxis-broker-grant/v1", "consumer": "public",
            "observed_at": 1792768000, "hold": false,
            "pools": {
                "anthropic": {"hold": false, "stale": false, "windows": {
                    "unified_7d": window(70.0, 31.5, 18.2, json!(1793055600)),
                    "unified_5h": window(70.0, 70.0, 12.0, json!(1792774800))}},
                "codex": {"hold": false, "stale": false, "windows": {
                    "secondary": window(70.0, 31.5, 5.1, json!(1793361600)),
                    "primary": window(70.0, 70.0, 2.0, json!(1792774800))}},
                "mistral": {"hold": false, "stale": false, "windows": {
                    "spend_7d": window(70.0, 31.5, 0.4, Value::Null)}},
            },
        })
    }

    /// Each pool's entry from the grant with `edits` (JSON pointer, value):
    /// held or not, its reason, and its room's window.
    #[test]
    fn grant_holds() {
        type Pool = (bool, Option<&'static str>, Option<&'static str>);
        type Case = (&'static str, Vec<(&'static str, Value)>, Pool, Pool);
        let free = |window| (false, None, Some(window));
        let held = |window| (true, Some(GRANT_HOLD), Some(window));
        let unread: Pool = (true, Some(NO_READING), None);
        let withheld: Pool = (true, Some(GRANT_HOLD), None);
        let cases: Vec<Case> = vec![
            ("as sampled", vec![], free("unified_7d"), free("secondary")),
            (
                "anthropic's week used to its allowance",
                vec![(
                    "/pools/anthropic/windows/unified_7d/used_percent",
                    json!(31.5),
                )],
                held("unified_7d"),
                free("secondary"),
            ),
            (
                "anthropic's five hours past its share, the tighter",
                vec![(
                    "/pools/anthropic/windows/unified_5h/used_percent",
                    json!(71),
                )],
                held("unified_5h"),
                free("secondary"),
            ),
            (
                "a window's own hold",
                vec![("/pools/codex/windows/primary/hold", json!(true))],
                free("unified_7d"),
                held("secondary"),
            ),
            (
                "a pool's hold",
                vec![("/pools/codex/hold", json!(true))],
                free("unified_7d"),
                held("secondary"),
            ),
            (
                "the whole grant's hold, stale pools too: never probed",
                vec![
                    ("/hold", json!(true)),
                    ("/pools/anthropic/stale", json!(true)),
                ],
                withheld,
                withheld,
            ),
            (
                "a window without figures",
                vec![("/pools/codex/windows/primary/used_percent", json!("2"))],
                free("unified_7d"),
                held("secondary"),
            ),
            (
                "a hold that is not false",
                vec![("/hold", Value::Null)],
                withheld,
                withheld,
            ),
            (
                "no codex: no share, never probed",
                vec![("/pools/codex", Value::Null)],
                free("unified_7d"),
                withheld,
            ),
            (
                "a stale anthropic, held for it",
                vec![
                    ("/pools/anthropic/stale", json!(true)),
                    ("/pools/anthropic/hold", json!(true)),
                ],
                unread,
                free("secondary"),
            ),
            (
                "a stale anthropic without windows",
                vec![
                    ("/pools/anthropic/stale", json!(true)),
                    ("/pools/anthropic/windows", json!({})),
                ],
                unread,
                free("secondary"),
            ),
            (
                "anthropic without windows, not stale: never probed",
                vec![("/pools/anthropic/windows", json!({}))],
                withheld,
                free("secondary"),
            ),
            (
                "a held anthropic without windows, not stale: never probed",
                vec![
                    ("/pools/anthropic/hold", json!(true)),
                    ("/pools/anthropic/windows", Value::Null),
                ],
                withheld,
                free("secondary"),
            ),
            (
                "a stale anthropic with its week used up: never probed",
                vec![
                    ("/pools/anthropic/stale", json!(true)),
                    ("/pools/anthropic/hold", json!(true)),
                    (
                        "/pools/anthropic/windows/unified_7d/allowed_percent",
                        json!(0),
                    ),
                ],
                held("unified_7d"),
                free("secondary"),
            ),
            (
                "a grant without a time, anthropic stale: never probed",
                vec![
                    ("/observed_at", Value::Null),
                    ("/pools/anthropic/stale", json!(true)),
                ],
                held("unified_7d"),
                held("secondary"),
            ),
            (
                "a stale anthropic with a window without figures: never probed",
                vec![
                    ("/pools/anthropic/stale", json!(true)),
                    (
                        "/pools/anthropic/windows/unified_5h/used_percent",
                        Value::Null,
                    ),
                ],
                held("unified_7d"),
                free("secondary"),
            ),
            (
                "a stale that is not true or false: never probed",
                vec![("/pools/anthropic/stale", Value::Null)],
                held("unified_7d"),
                free("secondary"),
            ),
            (
                "observed six hours ago",
                vec![("/observed_at", json!(1792768000 - 6 * 3600))],
                unread,
                unread,
            ),
            (
                "observed six hours ago, codex held: never probed",
                vec![
                    ("/observed_at", json!(1792768000 - 6 * 3600)),
                    ("/pools/codex/hold", json!(true)),
                ],
                unread,
                held("secondary"),
            ),
            (
                "observed six hours ago, a window held or used up: never probed",
                vec![
                    ("/observed_at", json!(1792768000 - 6 * 3600)),
                    ("/pools/anthropic/windows/unified_5h/hold", json!(true)),
                    ("/pools/codex/windows/secondary/used_percent", json!(31.5)),
                ],
                held("unified_7d"),
                held("secondary"),
            ),
            (
                "observed an hour from now, anthropic stale: never probed",
                vec![
                    ("/observed_at", json!(1792768000 + 3600)),
                    ("/pools/anthropic/stale", json!(true)),
                ],
                held("unified_7d"),
                held("secondary"),
            ),
        ];
        let budget: WeeklyBudget = serde_json::from_value(json!({"source": "broker",
            "pools": {"claude": "anthropic", "openai": "codex"}}))
        .unwrap();
        // A broker that has sent nothing on either provider: only no reading
        // is probed.
        let mut cold = broker();
        for provider in ["anthropic", "codex"] {
            cold[provider]["counts"] = json!({"requests": 0, "unmetered": 0});
        }
        for (name, edits, claude, openai) in cases {
            let mut grant = grant();
            for (pointer, value) in edits {
                *grant.pointer_mut(pointer).unwrap() = value;
            }
            let mut snap = snapshot(&cold, &pools(), now()).unwrap();
            grants(&mut snap, Some(&grant), &budget, now()).unwrap();
            decided(name, &snap);
            let probed: BTreeMap<String, String> =
                [("claude", claude, "anthropic"), ("openai", openai, "codex")]
                    .into_iter()
                    .filter(|(_, (_, reason, _), _)| *reason == Some(NO_READING))
                    .map(|(pool, _, on)| (pool.to_owned(), on.to_owned()))
                    .collect();
            assert_eq!(unasked(&cold, &snap), probed, "{name}");
            let got = |pool: &str| {
                let e = &snap["pools"][pool];
                (
                    e["hold"] == true,
                    e["reason"].as_str(),
                    e["window"].as_str(),
                )
            };
            assert_eq!((got("claude"), got("openai")), (claude, openai), "{name}");
        }
        let mut snap = snapshot(&broker(), &pools(), now()).unwrap();
        grants(&mut snap, None, &budget, now()).unwrap();
        decided("no grant", &snap);
        let openai = &snap["pools"]["openai"];
        assert_eq!(
            (&openai["hold"], &openai["reason"]),
            (&json!(true), &json!(NO_GRANT))
        );
        assert_eq!(openai["grant_pool"], "codex");
    }

    /// Every entry, paced, unread or granted, carries the pass's decision:
    /// a boolean `hold` and a `reason` exactly when held; a paced one its
    /// `limit_percent` too, past which it holds for pace.
    fn decided(name: &str, snap: &Value) {
        for (pool, e) in snap["pools"].as_object().unwrap() {
            assert!(e.get("reason").is_some(), "{name}: {pool} {e}");
            let hold = e["hold"].as_bool();
            assert_eq!(hold, Some(e["reason"].is_string()), "{name}: {pool} {e}");
            if e["source"] != SOURCE || e["reason"] == NO_READING {
                assert!(e.get("limit_percent").is_none(), "{name}: {pool} {e}");
                continue;
            }
            let (used, limit) = (e["used_percent"].as_f64(), e["limit_percent"].as_f64());
            let over = used.unwrap() > limit.unwrap();
            assert_eq!(over, e["reason"] == "pace", "{name}: {pool} {e}");
        }
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
            decided(name, &snap);
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
        // openai's burst of 3 is its limit's margin over its pace.
        let openai = &snap["pools"]["openai"];
        let margin =
            openai["limit_percent"].as_f64().unwrap() - openai["allowed_percent"].as_f64().unwrap();
        assert!((margin - 3.0).abs() < 1e-9, "{openai}");
        let mut claude = snap["pools"]["claude"].clone();
        claude["allowed_percent"] = json!(0);
        claude["ahead"] = json!(0);
        assert_eq!(
            claude,
            json!({"source": "praxis", "observed_at": "2026-10-23T15:06:40Z",
                "computed_at": "2026-10-23T15:15:10.452Z", "used_percent": 18.0,
                "window_minutes": 10080.0, "resets_at": "2026-10-26T23:00:00Z",
                "target_percent": 95.0, "burst": 95.0, "allowed_percent": 0,
                "limit_percent": 95.0, "ahead": 0,
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
