//! The pass view (`holds.passView`): the last passes, each pool's pace and
//! the agent runs in flight, published with the holds for the review app's
//! pass view, whose parser (review `src/github/passview.ts`) rejects the
//! whole view past its bounds. It is built from facts the pass already read,
//! with no read of its own, and names no broker or inference address and no
//! spend amount: pools give percentages alone, and holds of every new run
//! their kind, not their reason.

use super::{holds::Entry, *};

pub const SCHEMA: &str = "agent-board/pass-view/v1";
/// The passes kept, newest first; the reader takes at most 8.
const MAX_PASSES: usize = 5;
/// A pass's starts and reasons listed, the rest counted in its `omitted`.
const MAX_STARTS: usize = 8;
const MAX_REASONS: usize = 8;
/// The runs in flight listed, the rest counted in the view's `omitted`.
const MAX_FLIGHTS: usize = 32;
const MAX_POOLS: usize = 16;
/// How long an unchanged view may stand before it is written again: the
/// review app calls one older than an hour stale.
pub(super) const REFRESH_MINUTES: i64 = 30;
/// The holds of every new run, which the view counts as `capped`.
const CAPS: &[HoldKind] = &[HoldKind::Wip, HoldKind::Pools, HoldKind::Budget];

/// An agent run on an item: one the pass started, or one in flight.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Run {
    item: String,
    kind: String,
    pool: String,
    /// Where it runs, `OWNER/REPO`, and its id there.
    repository: String,
    id: u64,
    started_at: Option<String>,
}

impl Run {
    /// The run `id` in `caller` of `kind` on `repository#number`, where each
    /// is one the reader accepts; a pool it would not is `unknown`.
    pub(super) fn new(
        (repository, number): (&str, u64),
        kind: &str,
        pool: Option<&str>,
        (caller, id): (&str, u64),
        started_at: Option<&str>,
    ) -> Option<Self> {
        let named = |r: &str| {
            crate::supervise::word_repo(r)
                && r.len() <= 201
                && r.split('/').all(|s| s != "." && s != "..")
        };
        if !named(repository) || !named(caller) || number == 0 || id == 0 {
            return None;
        }
        let time = started_at.filter(|t| OffsetDateTime::parse(t, &Rfc3339).is_ok());
        Some(Self {
            item: format!("{repository}#{number}"),
            kind: token(&kind.replace(' ', "-"))?,
            pool: pool.and_then(token).unwrap_or_else(|| "unknown".into()),
            repository: caller.into(),
            id,
            started_at: time.map(str::to_owned),
        })
    }

    fn json(&self) -> Value {
        let mut out = json!({"item": self.item, "kind": self.kind, "pool": self.pool,
            "run": {"repository": self.repository, "id": self.id}});
        if let Some(at) = &self.started_at {
            out["started_at"] = at.as_str().into();
        }
        out
    }
}

/// `name` where the reader takes it as a kind or pool:
/// `[a-z][a-z0-9-]{0,31}`.
fn token(name: &str) -> Option<String> {
    let mut chars = name.chars();
    let first = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    let rest = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    (first && rest && name.len() <= 32).then(|| name.to_owned())
}

/// This pass's record at `at`: what it `started`, and where it started
/// nothing, its `holds` counted by kind, most first.
pub(super) fn record(at: &str, failures: usize, started: &[Run], holds: &[Entry]) -> Value {
    let mut counts = BTreeMap::<(&str, &str), usize>::new();
    for entry in holds.iter().filter(|_| started.is_empty()) {
        let kind = entry.hold.kind;
        let capped = if CAPS.contains(&kind) {
            "capped"
        } else {
            "held"
        };
        *counts.entry((capped, kind.name())).or_default() += 1;
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_by_key(|(key, n)| (std::cmp::Reverse(*n), *key));
    let reasons: Vec<Value> = (counts.iter().take(MAX_REASONS))
        .map(|((kind, name), n)| {
            let reason = match *kind {
                "capped" => format!("new runs held: {name}"),
                _ => format!("{n} held as {name}"),
            };
            json!({"item": null, "kind": kind, "reason": reason, "until": null})
        })
        .collect();
    let omitted = started.len().saturating_sub(MAX_STARTS) + counts.len() - reasons.len();
    json!({
        "at": at,
        "result": if failures == 0 { "success" } else { "partial" },
        "sections": [],
        "started": started.iter().take(MAX_STARTS).map(Run::json).collect::<Vec<_>>(),
        "reasons": reasons,
        "omitted": omitted,
    })
}

/// Each pool of the usage `snapshot` the pass paced by: its use and
/// allowance in whole percent, and its reset. A pool is under pace where
/// it has used less than it is allowed. Which work is ready for a pool is
/// not known by pool, so `ready` is null.
fn pools(snapshot: Option<&Value>) -> Vec<Value> {
    let percent = |v: &Value| {
        let p = v.as_f64().filter(|p| (0.0..=100.0).contains(p))?;
        Some(p.round() as u64)
    };
    let pools = snapshot.and_then(|s| s["pools"].as_object());
    (pools.into_iter().flatten())
        .filter_map(|(name, entry)| {
            let (used, allowed) = (
                percent(&entry["used_percent"]),
                percent(&entry["allowed_percent"]),
            );
            let resets = entry["resets_at"].as_str();
            let resets = resets.filter(|t| OffsetDateTime::parse(t, &Rfc3339).is_ok());
            Some(json!({
                "name": token(name)?, "used_percent": used, "allowed_percent": allowed,
                "resets_at": resets, "ready": null,
                "under_pace": used.zip(allowed).map(|(u, a)| u < a),
            }))
        })
        .take(MAX_POOLS)
        .collect()
}

/// The view at `at`: `pass`, this pass's [`record`], ahead of the
/// published view's passes in `previous`, the holds' JSON; the pools of the
/// usage `snapshot`; and the runs in flight where they were listed, which
/// alone makes the view complete.
pub(super) fn snapshot(
    at: &str,
    pass: Value,
    previous: Option<&Value>,
    usage: Option<&Value>,
    flights: Option<&[Run]>,
) -> Value {
    let earlier = (previous.map(|p| &p["pass_view"]))
        .filter(|v| v["schema"] == SCHEMA)
        .and_then(|v| v["passes"].as_array())
        .into_iter()
        .flatten()
        .filter(|p| p.is_object())
        .take(MAX_PASSES - 1)
        .cloned();
    let passes: Vec<Value> = std::iter::once(pass).chain(earlier).collect();
    let listed = flights.unwrap_or_default();
    let in_flight: Vec<Value> = (listed.iter().take(MAX_FLIGHTS))
        .map(|run| {
            let mut out = run.json();
            out["state"] = "running".into();
            out
        })
        .collect();
    json!({
        "schema": SCHEMA, "observed_at": at, "complete": flights.is_some(),
        "omitted": listed.len() - in_flight.len(),
        "passes": passes, "pools": pools(usage), "in_flight": in_flight,
    })
}

/// Cut `view` by one entry for a comment it does not fit in, the least
/// telling first: the oldest pass, down to the newest, then that pass's last
/// start or reason, counted in its `omitted`, then the last run in flight,
/// counted in the view's. Whether anything was cut: once nothing is, the
/// view must go whole. What is left stays within the reader's bounds.
pub(super) fn compact(view: &mut Value) -> bool {
    let Some(passes) = view.get_mut("passes").and_then(Value::as_array_mut) else {
        return false;
    };
    if passes.len() > 1 {
        passes.pop();
        return true;
    }
    if let Some(newest) = passes.first_mut().filter(|p| p.is_object()) {
        let cut = ["started", "reasons"]
            .into_iter()
            .any(|list| newest[list].as_array_mut().and_then(Vec::pop).is_some());
        if cut {
            newest["omitted"] = (newest["omitted"].as_u64().unwrap_or_default() + 1).into();
            return true;
        }
    }
    if view["in_flight"]
        .as_array_mut()
        .and_then(Vec::pop)
        .is_some()
    {
        view["omitted"] = (view["omitted"].as_u64().unwrap_or_default() + 1).into();
        return true;
    }
    false
}

/// Whether `now`'s view, unchanged, is due to be written over the
/// `published` one: [`REFRESH_MINUTES`] after it was observed.
pub(super) fn due(published: Option<&Value>, now: Option<&Value>) -> bool {
    let at = |v: Option<&Value>| {
        let at = v?["observed_at"].as_str()?;
        OffsetDateTime::parse(at, &Rfc3339).ok()
    };
    match (at(published), at(now)) {
        (Some(published), Some(now)) => now - published >= time::Duration::minutes(REFRESH_MINUTES),
        (None, Some(_)) => published.is_some(),
        _ => false,
    }
}

/// `view` as [`holds::changed`] compares it: without its time, and with
/// only the newest pass, timeless, as a pass like the last is no change.
pub(super) fn comparable(view: &mut Value) {
    let Some(fields) = view.as_object_mut() else {
        return;
    };
    fields.remove("observed_at");
    if let Some(passes) = fields.get_mut("passes").and_then(Value::as_array_mut) {
        passes.truncate(1);
        if let Some(newest) = passes.first_mut().and_then(Value::as_object_mut) {
            newest.remove("at");
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::pass::holds::{Hold, Scope};

    /// The review app's `parsePassView` (review `src/github/passview.ts`),
    /// as this view's contract: whether it takes `view` whole.
    pub(in crate::pass) fn accepted(view: &Value) -> bool {
        let time = |v: &Value| {
            v.as_str()
                .is_some_and(|t| OffsetDateTime::parse(t, &Rfc3339).is_ok())
        };
        let token = |v: &Value| v.as_str().is_some_and(|t| token(t).is_some());
        let text = |v: &Value| {
            v.as_str().is_some_and(|t| {
                !t.is_empty() && t.chars().count() <= 240 && !t.chars().any(|c| c.is_control())
            })
        };
        let repository = |v: &Value| {
            v.as_str().is_some_and(|r| {
                r.len() <= 201
                    && r.split('/').count() == 2
                    && r.split('/').all(|s| {
                        !s.is_empty()
                            && s != "."
                            && s != ".."
                            && s.bytes()
                                .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
                    })
            })
        };
        let positive = |v: &Value| v.as_u64().is_some_and(|n| n > 0);
        let count = |v: &Value| v.as_u64().is_some_and(|n| n <= 1_000_000);
        let item = |v: &Value| {
            v.as_str().is_some_and(|i| {
                i.split_once('#').is_some_and(|(r, n)| {
                    repository(&r.into())
                        && !n.starts_with('0')
                        && (1..=10).contains(&n.len())
                        && n.bytes().all(|c| c.is_ascii_digit())
                })
            })
        };
        let run = |v: &Value| v.is_object() && repository(&v["repository"]) && positive(&v["id"]);
        let list = |v: &Value, max: usize, check: &dyn Fn(&Value) -> bool| {
            v.as_array()
                .is_some_and(|l| l.len() <= max && l.iter().all(check))
        };
        let percent =
            |v: &Value| v.is_null() || v.as_f64().is_some_and(|p| (0.0..=100.0).contains(&p));
        let start = |v: &Value| {
            v.is_object()
                && item(&v["item"])
                && token(&v["kind"])
                && token(&v["pool"])
                && run(&v["run"])
        };
        let one_of = |v: &Value, of: &[&str]| v.as_str().is_some_and(|s| of.contains(&s));
        let section = |s: &Value| {
            token(&s["name"])
                && one_of(&s["result"], &["success", "skipped", "failure"])
                && match s["result"] == "failure" {
                    true => text(&s["error"]),
                    false => s["error"].is_null(),
                }
        };
        let reason = |r: &Value| {
            (r["item"].is_null() || item(&r["item"]))
                && one_of(
                    &r["kind"],
                    &["no-ready-work", "replan-delay", "capped", "held"],
                )
                && text(&r["reason"])
                && match r["kind"] == "replan-delay" {
                    true => time(&r["until"]),
                    false => r["until"].is_null(),
                }
        };
        let pass = |p: &Value| {
            time(&p["at"])
                && one_of(&p["result"], &["success", "partial", "failure"])
                && count(&p["omitted"])
                && list(&p["sections"], 12, &section)
                && list(&p["started"], 32, &start)
                && list(&p["reasons"], 32, &reason)
        };
        let pool = |p: &Value| {
            token(&p["name"])
                && percent(&p["used_percent"])
                && percent(&p["allowed_percent"])
                && (p["resets_at"].is_null() || time(&p["resets_at"]))
                && (p["ready"].is_null() || count(&p["ready"]))
                && (p["under_pace"].is_null() || p["under_pace"].is_boolean())
        };
        let flight = |f: &Value| match f["state"] == "running" {
            true => start(f),
            false => {
                one_of(&f["state"], &["merge-queue", "waiting-review"])
                    && repository(&f["repository"])
                    && positive(&f["number"])
            }
        };
        view["schema"] == SCHEMA
            && time(&view["observed_at"])
            && view["complete"].is_boolean()
            && count(&view["omitted"])
            && list(&view["passes"], 8, &pass)
            && list(&view["pools"], 16, &pool)
            && list(&view["in_flight"], 100, &flight)
    }

    /// The review app's fixture (review `test/fixtures/pass-view.json`).
    fn fixture() -> Value {
        json!({
            "schema": SCHEMA, "observed_at": "2026-11-05T12:00:00Z", "complete": true, "omitted": 0,
            "passes": [{
                "at": "2026-11-05T12:00:00Z", "result": "partial",
                "sections": [{"name": "reviews", "result": "failure", "error": "API rate limit exceeded"},
                    {"name": "schedule", "result": "success", "error": null}],
                "started": [],
                "reasons": [
                    {"item": null, "kind": "no-ready-work", "reason": "No ready work in openai", "until": null},
                    {"item": "example/work#12", "kind": "replan-delay", "reason": "Waiting out re-plan delay", "until": "2026-11-05T12:30:00Z"},
                    {"item": null, "kind": "capped", "reason": "Dispatch cap reached", "until": null},
                    {"item": "example/work#13", "kind": "held", "reason": "Needs operator", "until": null}],
                "omitted": 0,
            }, {
                "at": "2026-11-05T11:55:00Z", "result": "success",
                "sections": [{"name": "schedule", "result": "success", "error": null}],
                "started": [{"item": "example/work#11", "kind": "implement", "pool": "claude", "run": {"repository": "example/runner", "id": 42}}],
                "reasons": [], "omitted": 0,
            }],
            "pools": [
                {"name": "openai", "used_percent": 10, "allowed_percent": 60, "resets_at": "2026-11-05T14:00:00Z", "ready": 0, "under_pace": true},
                {"name": "other", "used_percent": null, "allowed_percent": null, "resets_at": null, "ready": null, "under_pace": null}],
            "in_flight": [
                {"state": "running", "item": "example/work#11", "kind": "implement", "pool": "claude", "run": {"repository": "example/runner", "id": 42}},
                {"state": "merge-queue", "repository": "example/work", "number": 14}],
        })
    }

    fn run(n: u64, kind: &str, pool: Option<&str>) -> Run {
        let started = Some("2026-11-05T11:00:00Z");
        Run::new(("o/work", n), kind, pool, ("o/runner", 100 + n), started).unwrap()
    }

    #[test]
    fn checker_takes_the_fixture_and_refuses_what_the_reader_refuses() {
        assert!(accepted(&fixture()));
        let cases: &[(&str, Value)] = &[
            ("/schema", json!("agent-board/pass-view/v2")),
            (
                "/passes/0/started",
                json!([{"item": "o/r#1", "kind": "Implement", "pool": "p", "run": {"repository": "o/r", "id": 1}}]),
            ),
            ("/passes/0/reasons/0/until", json!("2026-11-05T12:30:00Z")),
            ("/passes/0/reasons/0/reason", json!("two\nlines")),
            ("/pools/0/used_percent", json!(101)),
            ("/in_flight/0/run/id", json!(0)),
        ];
        for (at, value) in cases {
            let mut view = fixture();
            *view.pointer_mut(at).unwrap() = value.clone();
            assert!(!accepted(&view), "{at}");
        }
    }

    /// A run the reader would refuse is left out: a repository it takes
    /// for a path, a kind that is no token, or no run at all; a pool that
    /// is no token is `unknown`, and a kind's space a dash.
    #[test]
    fn runs_are_only_those_the_reader_takes() {
        let new = |repo: &str, kind: &str, pool: Option<&str>, id: u64| {
            Run::new((repo, 1), kind, pool, ("o/runner", id), None).map(|r| r.json())
        };
        for (repo, kind, id) in [
            ("o/..", "fix", 1),
            ("o", "fix", 1),
            ("o/r", "Fix", 1),
            ("o/r", "fix", 0),
        ] {
            assert_eq!(new(repo, kind, None, id), None, "{repo} {kind} {id}");
        }
        let fix = new("o/r", "review rerun", Some("Big Pool"), 7).unwrap();
        assert_eq!(
            fix,
            json!({"item": "o/r#1", "kind": "review-rerun", "pool": "unknown",
            "run": {"repository": "o/runner", "id": 7}})
        );
    }

    #[test]
    fn view_is_bounded_carries_passes_and_is_accepted() {
        let at = "2026-11-05T12:00:00Z";
        let entry = |kind, scope| Entry {
            scope,
            hold: Hold::new(kind, "$1003 of $1000 spent"),
            priority: None,
            why: None,
        };
        let item = |n| {
            Scope::Item(ItemRef {
                host: DEFAULT_HOST.into(),
                repository: "o/work".into(),
                number: n,
            })
        };
        let mut holds: Vec<Entry> = (1..=3)
            .map(|n| entry(HoldKind::NeedsHuman, item(n)))
            .collect();
        holds.push(entry(HoldKind::Budget, Scope::Pass));
        holds.push(entry(HoldKind::Blocked, item(9)));
        // Nothing started: the holds by kind, most first, without reasons.
        let idle = record(at, 0, &[], &holds);
        assert_eq!(
            idle["reasons"],
            json!([
            {"item": null, "kind": "held", "reason": "3 held as needs-human", "until": null},
            {"item": null, "kind": "capped", "reason": "new runs held: budget", "until": null},
            {"item": null, "kind": "held", "reason": "1 held as blocked", "until": null}])
        );
        // Started: the starts, and no reasons.
        let many: Vec<Run> = (1..=MAX_STARTS as u64 + 2)
            .map(|n| run(n, "implement", Some("claude")))
            .collect();
        let busy = record(at, 1, &many, &holds);
        assert_eq!(
            (busy["result"].as_str(), &busy["reasons"], &busy["omitted"]),
            (Some("partial"), &json!([]), &json!(2))
        );
        assert_eq!(busy["started"].as_array().unwrap().len(), MAX_STARTS);
        let usage = json!({"pools": {
            "claude": {"used_percent": 45.4, "allowed_percent": 50.0, "resets_at": "2026-11-05T13:00:00Z", "hold": false},
            "openai": {"reason": "no-reading", "hold": true},
            "Bad Name": {"used_percent": 1.0}}});
        let mut previous = None;
        let flights: Vec<Run> = (1..=MAX_FLIGHTS as u64 + 3)
            .map(|n| run(n, "fix", None))
            .collect();
        for n in 0..MAX_PASSES + 2 {
            let pass = if n % 2 == 0 {
                busy.clone()
            } else {
                idle.clone()
            };
            let view = snapshot(at, pass, previous.as_ref(), Some(&usage), Some(&flights));
            assert!(accepted(&view), "{view}");
            assert_eq!(
                view["passes"].as_array().unwrap().len(),
                (n + 1).min(MAX_PASSES)
            );
            previous = Some(json!({"pass_view": view}));
        }
        let view = &previous.unwrap()["pass_view"];
        assert_eq!(view["passes"][0]["result"], "partial");
        assert_eq!(
            (&view["complete"], &view["omitted"]),
            (&json!(true), &json!(3))
        );
        assert_eq!(view["in_flight"][0]["state"], "running");
        assert_eq!(
            view["pools"],
            json!([
            {"name": "claude", "used_percent": 45, "allowed_percent": 50, "resets_at": "2026-11-05T13:00:00Z", "ready": null, "under_pace": true},
            {"name": "openai", "used_percent": null, "allowed_percent": null, "resets_at": null, "ready": null, "under_pace": null}])
        );
        // No spend amount, and unlisted runs are no complete sample.
        assert!(!view.to_string().contains('$'), "{view}");
        let unlisted = snapshot(at, idle, None, None, None);
        assert!(accepted(&unlisted));
        assert_eq!(
            (&unlisted["complete"], &unlisted["pools"]),
            (&json!(false), &json!([]))
        );
    }

    /// Published with the holds; the same pass again is no change until
    /// [`REFRESH_MINUTES`] later, and another pass, or a view the size of
    /// the comment would not fit, is.
    #[test]
    fn view_is_published_with_the_holds() {
        let at = "2026-11-05T12:00:00Z";
        let mut document = holds::snapshot(&[], at.into(), Default::default());
        let pass = record(at, 0, &[run(1, "implement", Some("claude"))], &[]);
        document.pass_view = Some(snapshot(at, pass, None, None, Some(&[])));
        let (fitted, body) = holds::fit(document.clone()).unwrap();
        let published = holds::published(&body).unwrap();
        assert_eq!(Some(&published["pass_view"]), fitted.pass_view.as_ref());
        assert!(accepted(&published["pass_view"]));
        let again = |later: &str, started: &[Run]| {
            let mut document = holds::snapshot(&[], later.into(), Default::default());
            // As [`holds::carry`] keeps it where no pass made progress.
            document.pass.progress_at = at.into();
            let pass = record(later, 0, started, &[]);
            document.pass_view = Some(snapshot(later, pass, Some(&published), None, Some(&[])));
            holds::changed(Some(&body), &document).unwrap()
        };
        let same = [run(1, "implement", Some("claude"))];
        assert!(!again("2026-11-05T12:29:00Z", &same));
        assert!(again("2026-11-05T12:30:00Z", &same));
        assert!(again("2026-11-05T12:05:00Z", &[]));
        let mut oversized = document;
        oversized.pass_view = Some(json!({"oversized": "x".repeat(holds::MAX_COMMENT_CHARS)}));
        assert!(holds::fit(oversized).unwrap().0.pass_view.is_none());
    }
}
