//! Read-only, opt-in rollout measurement, not dispatch or spending authority.
use super::*;

const PAGES: usize = 3;
const PAGE_SIZE: usize = 100;
const READ_SECONDS: u64 = 10;

pub(super) fn activation(value: &str) -> Result<OffsetDateTime> {
    let at = OffsetDateTime::parse(value, &Rfc3339).context("sessions compareAfter timestamp")?;
    ensure!(
        value.ends_with('Z') && at.nanosecond() == 0,
        "sessions compareAfter must be whole-second UTC (ending Z)"
    );
    ensure!(at.year() >= 1970, "sessions compareAfter is before 1970");
    ensure!(
        at.checked_sub(time::Duration::days(1)).is_some()
            && at.checked_add(time::Duration::days(1)).is_some(),
        "sessions compareAfter cannot bound two days"
    );
    Ok(at)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Counts {
    triage: usize,
    session: usize,
}

/// Count unique initial run creations, never attempts. A partial listing is
/// unknown, including when its known subtotal happens to be zero.
fn window(
    client: &impl Rest,
    config: &Config,
    start: OffsetDateTime,
    end: OffsetDateTime,
    deadline: Instant,
) -> Result<BTreeMap<String, Counts>> {
    let mut counts: BTreeMap<_, _> = config
        .sessions
        .as_ref()
        .into_iter()
        .flat_map(Sessions::anchors)
        .map(|a| (a.repository.to_ascii_lowercase(), Counts::default()))
        .collect();
    // GitHub's created range includes both ends, at whole-second precision.
    let range = format!(
        "{}..{}",
        sweep::stamp(start),
        sweep::stamp(end - time::Duration::seconds(1))
    );
    let range: String = url::form_urlencoded::byte_serialize(range.as_bytes()).collect();
    let mut seen = BTreeMap::new();
    let mut listed = 0;
    let mut total = None;
    for page in 1..=PAGES {
        ensure!(Instant::now() <= deadline, "comparison time bound spent");
        let response = client.get(&format!(
            "{API_ROOT}repos/{}/actions/workflows/{}/runs?event=workflow_dispatch&created={range}&per_page={PAGE_SIZE}&page={page}",
            config.dispatch_repo, config.dispatch_workflow
        ), None)?;
        ensure!(
            response.status == 200,
            "dispatch history HTTP {}",
            response.status
        );
        let n = response.body["total_count"]
            .as_u64()
            .context("history has no total_count")?;
        ensure!(
            n <= (PAGES * PAGE_SIZE) as u64,
            "dispatch history exceeds {PAGES} pages"
        );
        ensure!(
            total.is_none_or(|old| old == n),
            "dispatch history changed while reading"
        );
        total = Some(n);
        let runs = response.body["workflow_runs"]
            .as_array()
            .context("history has no workflow_runs")?;
        ensure!(runs.len() <= PAGE_SIZE, "dispatch history page too large");
        listed += runs.len() as u64;
        for run in runs {
            let id = run["id"]
                .as_u64()
                .filter(|id| *id > 0)
                .context("history run has no id")?;
            let created = run["created_at"]
                .as_str()
                .context("history run has no created_at")?;
            let at = OffsetDateTime::parse(created, &Rfc3339).context("history run created_at")?;
            ensure!(
                at >= start && at < end,
                "history run outside requested window"
            );
            let title = run["display_title"]
                .as_str()
                .context("history run has no title")?;
            let identity = (at, title.to_owned());
            if let Some(old) = seen.insert(id, identity.clone()) {
                ensure!(old == identity, "history run identity changed");
                continue;
            }
            // Names are correlation evidence only, never authorization. Refuse
            // unknown names rather than hide potentially unclassified triages.
            let (kind, repo, number) =
                crate::supervise::run_title(title).context("unclassified dispatch title")?;
            let item = ItemRef {
                host: DEFAULT_HOST.into(),
                repository: repo.into(),
                number,
            };
            ensure!(
                crate::runs::title_matches(title, &item, kind),
                "unclassified dispatch title suffix"
            );
            if let Some(count) = counts.get_mut(&repo.to_ascii_lowercase()) {
                match kind {
                    "triage" => count.triage += 1,
                    "session" => count.session += 1,
                    _ => {}
                }
            }
        }
        if response.link.as_deref().and_then(next_link).is_none() {
            ensure!(
                listed == n && seen.len() as u64 == n,
                "dispatch history incomplete"
            );
            return Ok(counts);
        }
    }
    bail!("dispatch history exceeds {PAGES} pages")
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    pub(in crate::pass) fn session_comparison(&mut self) {
        let Some(sessions) = &self.config.sessions else {
            return;
        };
        let Some(value) = &sessions.compare_after else {
            return;
        };
        let Ok(at) = activation(value) else { return }; // Config::check validated it.
        let start = at - time::Duration::days(1);
        let end = at + time::Duration::days(1);
        let label = format!(
            "before=[{},{}) after=[{},{})",
            sweep::stamp(start),
            sweep::stamp(at),
            sweep::stamp(at),
            sweep::stamp(end)
        );
        if self.now < end {
            self.line(format!(
                "session comparison: {label} unknown: after window has not ended"
            ));
            return;
        }
        // Six GETs at most, shared ten-second deadline and the pass's remaining
        // request/time budget. Run after publication so it cannot starve writes.
        let deadline =
            (Instant::now() + Duration::from_secs(READ_SECONDS)).min(self.client.bounds.admit_by);
        let read = |a, b| window(&self.client, self.config, a, b, deadline);
        let before = read(start, at);
        let after = read(at, end);
        for anchor in sessions.anchors() {
            let repo = anchor.repository.to_ascii_lowercase();
            let show = |result: &Result<BTreeMap<String, Counts>>| match result {
                Ok(counts) => counts
                    .get(&repo)
                    .map(|c| format!("triage={} session={}", c.triage, c.session))
                    .unwrap_or_else(|| "unknown".into()),
                Err(error) => format!("unknown ({error:#})"),
            };
            self.line(format!("session comparison {repo}: {label} before: {}; after: {}; listed dispatch creations, not inference savings", show(&before), show(&after)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct History(RefCell<VecDeque<RestResponse>>);

    impl Rest for History {
        fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
            assert!(url.contains("event=workflow_dispatch&created=2026-10-22T00%3A00%3A00Z..2026-10-22T23%3A59%3A59Z"), "{url}");
            self.0
                .borrow_mut()
                .pop_front()
                .context("history unavailable")
        }

        fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
            panic!("measurement must not write")
        }
    }

    fn response(runs: Value, total: u64, more: bool) -> RestResponse {
        RestResponse {
            status: 200,
            etag: None,
            last_modified: None,
            link: more.then(|| "<https://api.github.com/next>; rel=\"next\"".into()),
            body: json!({"workflow_runs": runs, "total_count": total}),
        }
    }

    fn run(id: u64, title: &str) -> Value {
        json!({"id": id, "display_title": title, "created_at": "2026-10-22T00:00:00Z", "run_attempt": 1})
    }

    fn config() -> Config {
        let mut settings: Value =
            serde_json::from_str(include_str!("../../../../../fixtures/pass/config.json")).unwrap();
        settings["triagePools"] = json!(["claude"]);
        settings["sessions"] = json!({"hourUtc": 0, "issues": ["cgwalters-forge/agent-board#40"]});
        super::super::super::tests::config(settings)
    }

    fn read(pages: Vec<RestResponse>) -> Result<BTreeMap<String, Counts>> {
        window(
            &History(RefCell::new(pages.into())),
            &config(),
            activation("2026-10-22T00:00:00Z").unwrap(),
            activation("2026-10-23T00:00:00Z").unwrap(),
            Instant::now() + Duration::from_secs(10),
        )
    }

    #[test]
    fn deadline_and_pass_budget_stop_transport_reads() {
        let config = config();
        let start = activation("2026-10-22T00:00:00Z").unwrap();
        let end = start + time::Duration::days(1);
        for (requests, expired) in [(0, false), (100, true)] {
            let history = History(RefCell::new(vec![response(json!([]), 0, false)].into()));
            let deadline = Instant::now() + Duration::from_secs(10);
            let bounds = crate::pass::Bounds::new(requests, deadline);
            let client = Budget {
                inner: &history,
                bounds: &bounds,
            };
            let deadline = if expired {
                Instant::now() - Duration::from_secs(1)
            } else {
                deadline
            };
            assert!(window(&client, &config, start, end, deadline).is_err());
            assert_eq!(history.0.borrow().len(), 1);
        }
        let history = History(RefCell::new(
            (0..=PAGES)
                .map(|_| response(json!([]), 300, true))
                .collect(),
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        let error = window(&history, &config, start, end, deadline).unwrap_err();
        assert!(error.to_string().contains("exceeds 3 pages"));
        assert_eq!(history.0.borrow().len(), 1, "no fourth page was read");
    }

    #[test]
    fn repositories_boundaries_and_reruns() {
        let mut rerun = run(1, "triage CGWALTERS-FORGE/AGENT-BOARD#12 (astra)");
        rerun["run_attempt"] = 2.into();
        let runs = json!([
            rerun,
            run(2, "session cgwalters-forge/agent-board#40 (claude)"),
            run(3, "triage other/repository#1"),
            run(4, "implement cgwalters-forge/agent-board#1")
        ]);
        let counts = read(vec![response(runs, 4, false)]).unwrap();
        assert_eq!(
            counts["cgwalters-forge/agent-board"],
            Counts {
                triage: 1,
                session: 1
            }
        );
        for boundary in ["2026-10-21T23:59:59Z", "2026-10-23T00:00:00Z"] {
            let mut run = run(1, "triage cgwalters-forge/agent-board#1");
            run["created_at"] = boundary.into();
            assert!(read(vec![response(json!([run]), 1, false)]).is_err());
        }
    }

    #[test]
    fn unknown_is_not_zero_and_collection_is_bounded() {
        for pages in [
            vec![],
            vec![response(json!([]), 1, false)],
            vec![response(json!([{"id":1}]), 1, false)],
            vec![response(
                json!([run(1, "triage cgwalters-forge/agent-board#1 extra")]),
                1,
                false,
            )],
            (0..PAGES).map(|_| response(json!([]), 300, true)).collect(),
            vec![response(json!([]), 1, true), response(json!([]), 0, false)],
        ] {
            assert!(read(pages).is_err());
        }
        let zero = read(vec![response(json!([]), 0, false)]).unwrap();
        assert_eq!(zero["cgwalters-forge/agent-board"], Counts::default());
        let paged = read(vec![
            response(
                json!([run(1, "triage cgwalters-forge/agent-board#1")]),
                2,
                true,
            ),
            response(
                json!([run(2, "session cgwalters-forge/agent-board#40")]),
                2,
                false,
            ),
        ])
        .unwrap();
        assert_eq!(
            paged["cgwalters-forge/agent-board"],
            Counts {
                triage: 1,
                session: 1
            }
        );
        let mut denied = response(json!([]), 0, false);
        denied.status = 403;
        assert!(read(vec![denied]).is_err());
    }

    #[test]
    fn activation_is_explicit_utc_and_second_precision() {
        for value in [
            "yesterday",
            "2026-10-22T00:00:00+01:00",
            "2026-10-22T00:00:00.1Z",
            "1969-01-01T00:00:00Z",
        ] {
            assert!(activation(value).is_err(), "{value}");
        }
        assert!(activation("2026-10-22T00:00:00Z").is_ok());
    }
}
