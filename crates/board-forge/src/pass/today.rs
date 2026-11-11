//! Aggregate-only private activity (`holds.today`), never a public cost report.
use super::*;

pub(super) const SCHEMA: &str = "agent-board/today/v1";
const READ_LIMIT: usize = 100;
const READ_SECONDS: u64 = 30;
const POOLS: usize = 16;
const POOL_CHARS: usize = 64;
const UNKNOWN: &str = "unknown";

fn pool_name(pool: &str) -> bool {
    !pool.is_empty()
        && pool.len() <= POOL_CHARS
        && pool
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}

pub(super) fn valid_pools(pools: Option<&BTreeMap<String, u64>>) -> bool {
    pools.is_none_or(|p| p.len() <= POOLS && p.keys().all(|k| pool_name(k)))
}

pub(super) fn add_pool(pools: &mut Option<BTreeMap<String, u64>>, pool: &str, count: u64) {
    add_pool_map(pools.get_or_insert_with(BTreeMap::new), pool, count);
}

pub(super) fn add_pool_map(pools: &mut BTreeMap<String, u64>, pool: &str, count: u64) {
    // Reserve a bucket for reruns and missing/invalid pools; arbitrary names
    // cannot grow carried snapshots beyond this fixed vocabulary size.
    let pool = if pool_name(pool) && (pools.contains_key(pool) || pools.len() < POOLS - 1) {
        pool
    } else {
        UNKNOWN
    };
    let n = pools.entry(pool.into()).or_default();
    *n = n.saturating_add(count);
}

/// Optional reads must not consume the requests reserved for publishing holds,
/// or let a busy day's artifact listings monopolize the pass. This same small
/// budget covers both changes and cost, on top of the pass's existing bounds.
struct Reads<'a, C> {
    client: &'a Budget<'a, C>,
    left: Cell<usize>,
    deadline: Instant,
}

impl<C> Reads<'_, C> {
    fn spend(&self) -> Result<()> {
        ensure!(
            self.left.get() > 0,
            "today request budget ({READ_LIMIT}) is spent"
        );
        ensure!(Instant::now() <= self.deadline, "today time bound is spent");
        ensure!(
            self.client.bounds.left.get() > WRITE_RESERVE + HOLDS_READS,
            "today leaves the holds publication reserve"
        );
        self.left.set(self.left.get() - 1);
        Ok(())
    }
}

impl<C: Rest + Transport> Transport for &Reads<'_, C> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        ensure!(!is_mutation(query), "today is read-only");
        self.spend()?;
        self.client.graphql(query, variables)
    }
}

impl<C: Rest> Rest for Reads<'_, C> {
    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
        self.spend()?;
        self.client.get(url, validator)
    }

    fn download(&self, url: &str, limit: u64) -> Result<Vec<u8>> {
        self.spend()?;
        self.client.download(url, limit)
    }

    fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
        bail!("today is read-only")
    }
}

fn counts(changes: &crate::changes::Changes) -> Value {
    use crate::changes::Section;
    let count = |section| {
        changes.sections.get(&section).map_or(0, Vec::len)
            + changes.bots.get(&section).map_or(0, |b| b.count)
    };
    json!({"landed": count(Section::Landed), "closed": count(Section::Closed),
        "opened": count(Section::Opened)})
}

fn cost(date: time::Date, day: crate::cost::Day, config: &Config) -> Value {
    let report = crate::cost::report(date, day, &config.prices);
    let totals = &report["totals"];
    let unread = report["unread"].as_array().map_or(0, Vec::len);
    let unpriced = totals["unpriced_runs"].as_u64().unwrap_or_default();
    let complete = unread == 0 && unpriced == 0;
    let priced = totals["usd"].as_f64().unwrap_or(0.0);
    json!({"day": crate::cost::date_text(date), "complete": complete,
        "usd": complete.then_some(priced), "priced_usd": priced,
        "unpriced_runs": unpriced, "unread_runs": unread,
        "weekly_budget_usd": config.budget.as_ref().and_then(|b| b.weekly_usd)})
}

fn publication(now: OffsetDateTime, overhead: Option<&holds::Overhead>) -> Value {
    let now = now.to_offset(time::UtcOffset::UTC);
    json!({"schema": SCHEMA, "day": now.date().to_string(),
        "observed_at": now.format(&Rfc3339).unwrap_or_default(),
        "changes": null, "runs": overhead.map(|o| json!({
            "complete": o.today.complete, "by_kind": o.today.effects.starts,
            "by_pool": o.today.effects.starts_by_pool}))})
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    pub(super) fn today(&mut self, overhead: Option<&holds::Overhead>) -> Value {
        let now = self.now.to_offset(time::UtcOffset::UTC);
        let day = now.date();
        let reads = Reads {
            client: &self.client,
            left: Cell::new(READ_LIMIT),
            deadline: (Instant::now() + Duration::from_secs(READ_SECONDS))
                .min(self.client.bounds.admit_by),
        };
        let changes = (|| {
            let board = self.config.github_board().context("no GitHub board")?;
            let github = crate::github::GitHub {
                transport: &reads,
                owner: board.owner.clone(),
                number: board.project,
            };
            // changes has an exclusive lower bound; GitHub's event times are
            // whole seconds. Include events exactly at midnight, not yesterday.
            github
                .changes(
                    day.midnight().assume_utc() - time::Duration::seconds(1),
                    Some(now),
                    Some(&self.config.operator),
                    false,
                )
                .map(|c| counts(&c))
        })();
        let last_cost = if self.config.prices.is_empty() {
            None
        } else {
            day.previous_day().map(|date| {
                let reader = crate::cost::Reader::new(&reads, None).until(reads.deadline);
                crate::cost::read_day(&reader, &crate::cost::repositories(self.config), date)
                    .map(|d| cost(date, d, self.config))
            })
        };
        // Release the immutable client borrow before recording diagnostics.
        let mut result = publication(now, overhead);
        match changes {
            Ok(counts) => result["changes"] = counts,
            Err(error) => self.line(format!("today: changes unavailable: {error:#}")),
        }
        if let Some(cost) = last_cost {
            match cost {
                Ok(cost) => result["cost"] = cost,
                Err(error) => self.line(format!("today: cost unavailable: {error:#}")),
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{Bots, Changes, Line, Link, Section};

    fn at() -> OffsetDateTime {
        OffsetDateTime::parse("2026-11-07T12:00:00Z", &Rfc3339).unwrap()
    }

    #[test]
    fn today_fixture_and_utc_boundary() {
        let line = Line {
            link: Link {
                reference: "o/r#1".into(),
                url: "https://github.com/o/r/issues/1".into(),
            },
            title: "not published".into(),
            closes: vec![],
            released: false,
        };
        let changes = Changes {
            since: "2026-11-07T00:00:00Z".into(),
            until: Some(at().format(&Rfc3339).unwrap()),
            sections: BTreeMap::from([
                (Section::Landed, vec![line.clone()]),
                (Section::Closed, vec![line.clone()]),
                (Section::Opened, vec![line.clone(), line]),
            ]),
            bots: BTreeMap::from([(
                Section::Landed,
                Bots {
                    count: 2,
                    repositories: BTreeSet::new(),
                },
            )]),
            repositories: BTreeSet::new(),
        };
        let mut effects = Effects::default();
        effects.starts.insert("implement".into(), 2);
        effects.starts.insert("review".into(), 1);
        add_pool(&mut effects.starts_by_pool, "claude", 2);
        add_pool(&mut effects.starts_by_pool, UNKNOWN, 1);
        let overhead = holds::overhead(None, at(), &effects);
        let mut result = publication(at(), Some(&overhead));
        result["changes"] = counts(&changes);
        let config = super::super::tests::config(json!({"prices": {
            "model": {"input": 3.0, "cached_input": 0.3, "output": 15.0}},
            "budget": {"weeklyUsd": 100.0}}));
        let run = crate::cost::Run {
            id: 1,
            url: "https://github.com/o/r/actions/runs/1".into(),
            kind: "implement",
            repo: "o/r".into(),
            item: None,
            model: "model".into(),
            tokens: crate::cost::Tokens {
                input: 1_000_000,
                ..Default::default()
            },
            usd: None,
            outcome: None,
            duration_s: None,
        };
        let date = at().date().previous_day().unwrap();
        let day = crate::cost::Day {
            runs: vec![run.clone()],
            ..Default::default()
        };
        result["cost"] = cost(date, day, &config);
        let fixture: Value =
            serde_json::from_str(include_str!("../../../../fixtures/today.json")).unwrap();
        assert_eq!(result, fixture);
        assert!(!result.to_string().contains("not published"));

        let mut unpriced = run.clone();
        unpriced.model = "unpriced".into();
        for (runs, unread, priced) in [
            (vec![], vec![], Some(0.0)),
            (vec![unpriced], vec![], None),
            (
                vec![run],
                vec![crate::cost::Unread {
                    id: 2,
                    url: String::new(),
                    reason: "expired".into(),
                }],
                None,
            ),
        ] {
            let got = cost(
                date,
                crate::cost::Day {
                    runs,
                    unread,
                    ..Default::default()
                },
                &config,
            );
            assert_eq!(got["usd"].as_f64(), priced);
            assert_eq!(got["complete"], priced.is_some());
        }
        let local = OffsetDateTime::parse("2026-11-08T01:00:00+03:00", &Rfc3339).unwrap();
        assert_eq!(publication(local, None)["day"], "2026-11-07");
    }

    #[test]
    fn midnight_events_are_included_but_yesterday_is_not() {
        let now = at().date().midnight().assume_utc();
        let start = now - time::Duration::seconds(1);
        let item = |number, created| {
            json!({"content": {
                "__typename": "Issue", "number": number, "title": "event",
                "url": format!("https://github.com/o/r/issues/{number}"),
                "repository": {"nameWithOwner": "o/r"}, "state": "OPEN",
                "createdAt": created, "closedAt": null,
                "labels": {"nodes": [], "pageInfo": {"hasNextPage": false}},
                "timelineItems": {"nodes": [], "pageInfo": {"hasNextPage": false}}
            }})
        };
        let changes = crate::changes::classify_window(
            &[
                item(1, "2026-11-07T00:00:00Z"),
                item(2, "2026-11-06T23:59:59Z"),
                item(3, "2026-11-07T00:00:01Z"),
            ],
            &[],
            &start.format(&Rfc3339).unwrap(),
            Some(&now.format(&Rfc3339).unwrap()),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(
            counts(&changes),
            json!({"landed": 0, "closed": 0, "opened": 1})
        );
    }

    #[test]
    fn pools_are_bounded_and_old_history_stays_unknown() {
        let mut effects = Effects::default();
        effects.starts.insert("implement".into(), 100);
        for n in 0..100 {
            add_pool(&mut effects.starts_by_pool, &format!("pool-{n}"), 1);
        }
        let pools = effects.starts_by_pool.as_ref().unwrap();
        assert_eq!(pools.len(), POOLS);
        assert_eq!(pools.values().sum::<u64>(), 100);
        assert!(valid_pools(Some(pools)));
        assert!(!valid_pools(Some(&BTreeMap::from([(
            "x".repeat(POOL_CHARS + 1),
            1
        )]))));
        let old = holds::overhead(None, at(), &effects);
        let mut previous = json!({"schema": holds::HOLDS_SCHEMA, "at": at().format(&Rfc3339).unwrap(), "pass": {"overhead": old}});
        previous["pass"]["overhead"]["today"]
            .as_object_mut()
            .unwrap()
            .remove("starts_by_pool");
        let carried = holds::overhead(Some(&previous), at(), &effects);
        assert!(carried.today.effects.starts_by_pool.is_none());
        let rolled = holds::overhead(Some(&previous), at() + time::Duration::days(1), &effects);
        assert_eq!(rolled.today.effects.starts_by_pool, effects.starts_by_pool);
    }

    #[test]
    fn reads_stop_at_request_time_and_publication_reserve() {
        let forge = super::super::tests::Forge::new(None, false);
        let bounds = Bounds::new(1000, Instant::now() + Duration::from_secs(60));
        let client = Budget {
            inner: &forge,
            bounds: &bounds,
        };
        let mut reads = Reads {
            client: &client,
            left: Cell::new(READ_LIMIT),
            deadline: Instant::now() + Duration::from_secs(60),
        };
        for _ in 0..READ_LIMIT {
            reads.spend().unwrap();
        }
        assert!(
            reads
                .spend()
                .unwrap_err()
                .to_string()
                .contains("request budget")
        );
        reads.left.set(READ_LIMIT);
        reads.deadline = Instant::now() - Duration::from_secs(1);
        assert!(
            reads
                .spend()
                .unwrap_err()
                .to_string()
                .contains("time bound")
        );
        reads.deadline = Instant::now() + Duration::from_secs(60);
        bounds.left.set(WRITE_RESERVE + HOLDS_READS);
        assert!(
            reads
                .spend()
                .unwrap_err()
                .to_string()
                .contains("publication reserve")
        );
        assert!(reads.send("POST", "", &Value::Null).is_err());
    }

    #[test]
    fn today_fits_without_evicting_holds_and_clock_alone_is_unchanged() {
        let mut document = holds::snapshot(&[], at().format(&Rfc3339).unwrap(), Default::default());
        document.today = Some(publication(at(), None));
        let body = holds::render(&document).unwrap();
        assert!(holds::published(&body).unwrap().get("today").is_some());
        document.today = Some(publication(at() + time::Duration::minutes(1), None));
        assert!(!holds::changed(Some(&body), &document).unwrap());
        document.today.as_mut().unwrap()["changes"] =
            json!({"landed": 1, "closed": 0, "opened": 0});
        assert!(holds::changed(Some(&body), &document).unwrap());
        document.today = Some(json!({"oversized": "x".repeat(holds::MAX_COMMENT_CHARS)}));
        let (fit, _) = holds::fit(document).unwrap();
        assert!(fit.today.is_none());
        assert_eq!(fit.truncated, 0);
    }
}
