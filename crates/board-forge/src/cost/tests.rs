use super::*;
use std::cell::RefCell;
use std::io::Write;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/cost/day-2026-10-28.json"
);
const REPORT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/cost/report-2026-10-28.json"
);
pub(crate) const REPOSITORIES: &[&str] =
    &["cgwalters-forge/agent-board", "cgwalters-forge/tracker"];

/// A zip of `entries` (name, contents, deflated), as `actions/upload-artifact` packs one.
pub(crate) fn zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
    let (mut out, mut central) = (Vec::new(), Vec::new());
    for (name, contents, deflate) in entries {
        let packed = match deflate {
            true => {
                let mut encoder =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(contents).unwrap();
                encoder.finish().unwrap()
            }
            false => contents.to_vec(),
        };
        let mut crc = flate2::Crc::new();
        crc.update(contents);
        let method: u16 = if *deflate { 8 } else { 0 };
        // Versions made by and needed, flags, method, time and date; the
        // local header leaves out the first.
        let mut fields = Vec::new();
        for n in [20u16, 20, 0, method, 0, 0] {
            fields.extend(n.to_le_bytes());
        }
        for n in [crc.sum(), packed.len() as u32, contents.len() as u32] {
            fields.extend(n.to_le_bytes());
        }
        let offset = out.len() as u32;
        out.extend(LOCAL_SIGNATURE);
        out.extend(&fields[2..]);
        out.extend((name.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(name.as_bytes());
        out.extend(&packed);
        central.extend(CENTRAL_SIGNATURE);
        central.extend(&fields);
        for n in [name.len() as u16, 0, 0, 0, 0] {
            central.extend(n.to_le_bytes());
        }
        central.extend(0u32.to_le_bytes());
        central.extend(offset.to_le_bytes());
        central.extend(name.as_bytes());
    }
    let start = out.len() as u32;
    out.extend(&central);
    out.extend(END_SIGNATURE);
    let n = entries.len() as u16;
    for v in [0u16, 0, n, n] {
        out.extend(v.to_le_bytes());
    }
    out.extend((central.len() as u32).to_le_bytes());
    out.extend(start.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

/// The recorded day's GitHub, and a data branch at `branch` holding
/// `published` (file names under `cost/`); every write is recorded. A GET
/// whose path contains `refuse`'s text fails, under a rate limit where it
/// says so.
pub(crate) struct Forge {
    fixture: Value,
    pub branch: Option<&'static str>,
    pub published: Vec<String>,
    pub refuse: Option<(&'static str, bool)>,
    pub downloads: Cell<usize>,
    pub sent: RefCell<Vec<(String, String, Value)>>,
}

impl Forge {
    pub(crate) fn new() -> Self {
        let text = std::fs::read_to_string(FIXTURE).unwrap();
        Self {
            fixture: serde_json::from_str(&text).unwrap(),
            branch: None,
            published: Vec::new(),
            refuse: None,
            downloads: Cell::new(0),
            sent: RefCell::default(),
        }
    }

    /// Each request sent, as `METHOD path`.
    pub(crate) fn requests(&self) -> Vec<String> {
        (self.sent.borrow().iter())
            .map(|(method, url, _)| format!("{method} {}", url.trim_start_matches(API_ROOT)))
            .collect()
    }
}

impl Rest for Forge {
    fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
        let path = url.trim_start_matches(API_ROOT);
        if let Some((text, limited)) = self.refuse.filter(|(text, _)| path.contains(text)) {
            let error = anyhow::anyhow!("GET {path}: GitHub HTTP status 502");
            return Err(match limited {
                true => error.context(crate::github::RateLimited(format!("refused {text}"))),
                false => error,
            });
        }
        // Every other day had no runs.
        let none = path
            .contains("/actions/runs?")
            .then(|| json!({"total_count": 0, "workflow_runs": []}));
        let body = (self.fixture["responses"].get(path).cloned())
            .or(none)
            .with_context(|| format!("no recorded answer for GET {path}"))?;
        Ok(RestResponse {
            status: 200,
            etag: None,
            last_modified: None,
            link: None,
            body,
        })
    }

    fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        (self.sent.borrow_mut()).push((method.into(), url.into(), body.clone()));
        let path = url.split("/git/").nth(1).unwrap_or(url);
        Ok(match (method, path) {
            ("GET", _) if url.contains("/contents/cost?ref=data") => match self.branch {
                None => (404, json!({"message": "Not Found"})),
                Some(_) => (
                    200,
                    json!(
                        self.published
                            .iter()
                            .map(|n| json!({"name": n}))
                            .collect::<Vec<_>>()
                    ),
                ),
            },
            ("GET", "ref/heads/data") => match self.branch {
                None => (404, json!({"message": "Not Found"})),
                Some(sha) => (200, json!({"object": {"sha": sha}})),
            },
            ("GET", _) if path.starts_with("commits/") => (200, json!({"tree": {"sha": "tree0"}})),
            ("POST", "trees") => (201, json!({"sha": "tree1"})),
            ("POST", "commits") => (201, json!({"sha": "commit1"})),
            ("PATCH", "refs/heads/data") | ("POST", "refs") => (200, json!({})),
            _ => bail!("unexpected {method} {url}"),
        })
    }

    fn download(&self, url: &str, limit: u64) -> Result<Vec<u8>> {
        assert_eq!(limit, ARTIFACT_BYTES);
        self.downloads.set(self.downloads.get() + 1);
        if let Some((text, _)) = self.refuse.filter(|(text, _)| url.contains(text)) {
            bail!("GET {url}: the download of {text} timed out");
        }
        let id = (url.strip_suffix("/zip"))
            .and_then(|u| u.rsplit('/').next())
            .context("not an artifact's zip")?;
        let summary = serde_json::to_vec(&self.fixture["artifacts"][id]).unwrap();
        Ok(zip(&[
            ("logs/agent.log", b"...", true),
            (SUMMARY_FILE, &summary, true),
        ]))
    }
}

pub(crate) fn prices() -> Prices {
    let price = Price {
        input: 3.0,
        cached_input: 0.3,
        output: 15.0,
    };
    BTreeMap::from([("anthropic/claude-x".to_owned(), price)])
}

fn day() -> Date {
    parse_date("2026-10-28").unwrap()
}

fn repositories() -> Vec<String> {
    REPOSITORIES.iter().map(|r| r.to_string()).collect()
}

#[test]
fn reads_and_prices_a_recorded_day() {
    let forge = Forge::new();
    let reader = Reader::new(&forge, None);
    let read = read_day(&reader, &repositories(), day()).unwrap();
    // Two run listings, two artifact listings and four downloads: the CI
    // run has no agent artifact, the fix run's expired, the review's
    // `agent-run` is read rather than its longer-named one, the dispatched
    // runs' `dispatch-KIND-agent-run` are, and the listing stops at the
    // first artifact of the day before.
    assert_eq!((reader.spent(), forge.downloads.get()), (8, 4));
    let report = report(day(), read, &prices());
    let expected: Value = serde_json::from_str(&std::fs::read_to_string(REPORT).unwrap()).unwrap();
    assert_eq!(report, expected, "{report:#}");
    assert_eq!(
        human(&report),
        std::fs::read_to_string(REPORT.replace(".json", ".txt")).unwrap()
    );
}

#[test]
fn a_cached_summary_is_not_downloaded_again() {
    let dir = std::env::temp_dir().join(format!("cost-cache-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let forge = Forge::new();
    let first = read_day(&Reader::new(&forge, Some(&dir)), &repositories(), day()).unwrap();
    assert!(dir.join("40812885249.json").exists());
    let again = Forge::new();
    let reader = Reader::new(&again, Some(&dir));
    let second = read_day(&reader, &repositories(), day()).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!((again.downloads.get(), reader.spent()), (0, 4));
    assert_eq!(first, second);
}

#[test]
fn the_request_budget_bounds_a_read_and_leaves_the_commit_its_share() {
    let forge = Forge::new();
    let reader = Reader::new(&forge, None);
    reader.left.set(COMMIT_REQUESTS + 3);
    let error = read_day(&reader, &repositories(), day()).unwrap_err();
    // The fourth read, the tracker's runs, is refused.
    assert!(
        format!("{error:#}").contains("cost request budget"),
        "{error:#}"
    );
    assert!(reader.exhausted());
    let files = [("cost/a.json".to_owned(), "{}\n".to_owned())];
    let repository = REPOSITORIES[1];
    assert_eq!(commit(&reader, repository, &files, "m").unwrap(), "commit1");
    assert_eq!(reader.left.get(), 1);

    // Past the deadline, reads stop and the commit still goes out.
    let forge = Forge::new();
    let reader = Reader::new(&forge, None).until(Instant::now());
    std::thread::sleep(Duration::from_millis(2));
    let error = read_day(&reader, &repositories(), day()).unwrap_err();
    assert!(format!("{error:#}").contains("time bound"), "{error:#}");
    assert!(reader.exhausted());
    assert_eq!(commit(&reader, repository, &files, "m").unwrap(), "commit1");
}

#[test]
fn a_failed_request_fails_the_day_rather_than_leave_a_run_unread() {
    for (refuse, expected) in [
        (
            ("artifacts/8001/zip", false),
            "the download of artifacts/8001/zip timed out",
        ),
        (
            ("tracker/actions/artifacts", false),
            "GitHub HTTP status 502",
        ),
    ] {
        let mut forge = Forge::new();
        forge.refuse = Some(refuse);
        let error = read_day(&Reader::new(&forge, None), &repositories(), day()).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
    // After a rate limit, nothing more is sent, the commit included.
    let mut forge = Forge::new();
    forge.refuse = Some(("tracker/actions/runs", true));
    let reader = Reader::new(&forge, None);
    assert!(read_day(&reader, &repositories(), day()).is_err());
    assert!(reader.exhausted());
    let error = commit(&reader, REPOSITORIES[1], &[], "m").unwrap_err();
    assert!(format!("{error:#}").contains("budget"), "{error:#}");
    assert!(forge.sent.borrow().is_empty());
}

/// Answers a day's runs as more than GitHub lists, and each window of
/// half a day or less with one run named by its window.
struct Busy(RefCell<Vec<String>>);

impl Rest for Busy {
    fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
        let created = url
            .split("created=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap();
        self.0.borrow_mut().push(created.to_owned());
        let body = match created.contains("..") {
            true => json!({"total_count": 1, "workflow_runs": [{"id": self.0.borrow().len()}]}),
            false => json!({"total_count": 1001, "workflow_runs": []}),
        };
        Ok(RestResponse {
            status: 200,
            etag: None,
            last_modified: None,
            link: None,
            body,
        })
    }

    fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
        bail!("no writes")
    }
}

#[test]
fn a_day_past_the_listing_cap_is_listed_in_halves() {
    let busy = Busy(RefCell::default());
    let runs = Reader::new(&busy, None).runs("o/r", day()).unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(
        *busy.0.borrow(),
        [
            "2026-10-28",
            "2026-10-28T00:00:00Z..2026-10-28T11:59:59Z",
            "2026-10-28T12:00:00Z..2026-10-28T23:59:59Z",
        ]
    );
}

#[test]
fn kinds_come_from_the_workflow_then_the_title() {
    for (path, title, expected) in [
        (".github/workflows/review.yml", "Review o/r#1", "review"),
        (".github/workflows/agent-review.yaml", "fix o/r#1", "review"),
        (".github/workflows/supervisor.yml", "", "supervisor"),
        (".github/workflows/epic-planning.yml", "", "planning"),
        (
            ".github/workflows/dispatch.yml",
            "implement o/r#1 (claude)",
            "implement",
        ),
        (
            ".github/workflows/dispatch.yml",
            "triage o/r#2 (claude)",
            "triage",
        ),
        (".github/workflows/agent.yml", "fix o/r#3 (openai)", "fix"),
        (".github/workflows/agent.yml", "Plan o/r#4", "planning"),
        (".github/workflows/dispatch.yml", "research o/r#5", "other"),
        (".github/workflows/ci.yml", "fix the build", "fix"),
        (".github/workflows/ci.yml", "", "other"),
        (".github/workflows/deploy-preview.yml", "", "other"),
        (
            ".github/workflows/prefix-check.yml",
            "Reviews o/r#6",
            "review",
        ),
    ] {
        assert_eq!(
            kind(ARTIFACT, path, title, ""),
            expected,
            "{path} {title:?}"
        );
    }
    // A dispatched run's artifact prefix comes first, then the file name and
    // title, then the summary's workflow.
    let dispatch = ".github/workflows/dispatch.yml";
    for (artifact, title, workflow, expected) in [
        ("dispatch-implement-agent-run", "", "", "implement"),
        ("dispatch-fix-agent-run", "implement", "branch", "fix"),
        ("dispatch-triage-agent-run", "", "analysis", "triage"),
        ("dispatch-planning-agent-run", "", "", "planning"),
        ("dispatch-agent-run", "fix o/r#1", "branch", "fix"),
        ("dispatch-agent-run", "", "branch", "implement"),
        ("dispatch-agent-run", "", "analysis", "triage"),
        ("dispatch-agent-run", "", "dispatch", "other"),
        (ARTIFACT, "", "", "other"),
    ] {
        assert_eq!(
            kind(artifact, dispatch, title, workflow),
            expected,
            "{artifact} {title:?} {workflow}"
        );
    }
    assert_eq!(
        kind(ARTIFACT, ".github/workflows/review.yml", "", "branch"),
        "review"
    );
}

#[test]
fn agent_artifacts_are_named_agent_run_with_or_without_a_prefix() {
    for (name, expected) in [
        ("agent-run", true),
        ("dispatch-implement-agent-run", true),
        ("dispatch-fix-agent-run", true),
        // Any prefix: a summary that is no agentic-job run's is unread.
        ("logs-agent-run", true),
        ("agent-run-logs", false),
        ("coverage", false),
        ("wagent-run", false),
        ("", false),
    ] {
        assert_eq!(is_agent_artifact(name), expected, "{name}");
    }
}

#[test]
fn unzips_only_a_bounded_stored_or_deflated_file() {
    let summary = br#"{"tokens": {}}"#;
    for deflate in [false, true] {
        let zip = zip(&[("a.txt", b"a", deflate), (SUMMARY_FILE, summary, deflate)]);
        assert_eq!(unzip(&zip, SUMMARY_FILE, 64).unwrap(), summary);
        let error = unzip(&zip, SUMMARY_FILE, 4).unwrap_err();
        assert!(format!("{error:#}").contains("over 4 bytes"), "{error:#}");
    }
    let nested = zip(&[("run/summary.json", summary, true)]);
    let refused: [(&[u8], &str); 3] = [
        (&nested, "has no summary.json"),
        (b"PK\x03\x04 not a zip", "no end of central directory"),
        (&nested[..nested.len() - 30], "no end of central directory"),
    ];
    for (bytes, expected) in refused {
        let error = unzip(bytes, SUMMARY_FILE, 64).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
    // An entry whose size lies is refused, not read past its bound.
    let mut lying = zip(&[(SUMMARY_FILE, summary, true)]);
    let central = lying
        .windows(4)
        .position(|w| w == CENTRAL_SIGNATURE)
        .unwrap();
    lying[central + 24] = 2;
    let error = unzip(&lying, SUMMARY_FILE, 64).unwrap_err();
    assert!(format!("{error:#}").contains("not the 2"), "{error:#}");
}

#[test]
fn summaries_without_tokens_are_unread_and_prices_are_checked() {
    let run = json!({"id": 1, "path": ".github/workflows/review.yml", "display_title": ""});
    let run = &run;
    let artifact = &json!({"name": ARTIFACT});
    for (summary, expected) in [
        (&b"{"[..], "not JSON"),
        (br#"{"model": "m"}"#, "has no tokens"),
        (
            br#"{"tokens": {"input": -1}}"#,
            "tokens.input is not a count",
        ),
    ] {
        let error = counted(summary, "o/r", run, artifact, "u").unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
    let read = counted(
        br#"{"tokens": {"input": 1}, "repo": "../x", "item": "7"}"#,
        "o/r",
        run,
        artifact,
        "u",
    );
    let read = read.unwrap();
    assert_eq!(
        (
            read.repo.as_str(),
            read.item.as_deref(),
            read.model.as_str()
        ),
        ("o/r", Some("o/r#7"), UNKNOWN_MODEL)
    );
    // Items and models are text, without control characters.
    for (item, expected) in [
        (json!(-1), None),
        (json!(1.5), None),
        (json!("+5"), Some("+5")),
        (json!("o/r#9 \u{1b}[2J"), Some("o/r#9 [2J")),
    ] {
        let summary = json!({"tokens": {}, "item": item, "model": "m\u{7}"}).to_string();
        let read = counted(summary.as_bytes(), "o/r", run, artifact, "u").unwrap();
        assert_eq!((read.item.as_deref(), read.model.as_str()), (expected, "m"));
    }
    for (price, ok) in [
        (1.0, true),
        (-1.0, false),
        (f64::NAN, false),
        (PRICE_LIMIT + 1.0, false),
    ] {
        let prices = BTreeMap::from([(
            "m".to_owned(),
            Price {
                input: 1.0,
                cached_input: price,
                output: 1.0,
            },
        )]);
        assert_eq!(check_prices(&prices).is_ok(), ok, "{price}");
    }
    for (text, ok) in [
        ("2026-10-28", true),
        ("2026-02-30", false),
        ("2026-1-28", false),
        ("2026-10-28T00:00:00Z", false),
        ("+2026-10-28", false),
    ] {
        assert_eq!(parse_date(text).is_ok(), ok, "{text}");
    }
}
