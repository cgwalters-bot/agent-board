//! `agent-board cost` (docs/cost.md, agent-board#441): what one UTC day's
//! agent runs spent, by kind, model, repository and item, as
//! [`COST_DAY_SCHEMA`] JSON. Read-only; the pass publishes it
//! ([`crate::pass`]'s `cost` stage).
//!
//! Each completed workflow run created that day in the pass configuration's
//! work, followed and dispatch repositories that uploaded an [`ARTIFACT`]
//! artifact, or a dispatched run's `PREFIX-agent-run`, counts once, by the
//! `summary.json` in its zip: its tokens, model, repository, item, outcome
//! and duration. The runs and artifacts are listed a page of 100 at a time;
//! each artifact is downloaded, at most [`ARTIFACT_BYTES`], and only its
//! [`SUMMARY_FILE`], at most [`SUMMARY_BYTES`], is read. With a cache directory, a run's summary is
//! kept there by run id and never downloaded again. A run whose artifact
//! expired, is too large or holds no readable summary is listed as unread,
//! not counted; a failed request fails the day's read instead, which is
//! never published short of a run for it.
//!
//! A run is priced at list price from the configuration's `prices`, in
//! dollars per million tokens; input and cache-write tokens at `input`,
//! cache-read tokens at `cached_input`, output tokens at `output`, the four
//! counts taken as disjoint. A run whose model has no price is unpriced: its
//! tokens are counted and its cost is null, never $0.
use crate::github::API_ROOT;
use crate::poll::{Rest, RestResponse, text, validate_repository};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};
use time::{Date, OffsetDateTime, format_description::well_known::Rfc3339};

/// The schema of a day's report.
pub const COST_DAY_SCHEMA: &str = "agent-board/cost-day/v1";
/// The artifact every agentic-job run uploads: so named by a review, with a
/// prefix by a dispatched run ([`is_agent_artifact`]).
pub const ARTIFACT: &str = "agent-run";
/// The one file read from it.
pub const SUMMARY_FILE: &str = "summary.json";
/// The most of an artifact's zip downloaded.
pub const ARTIFACT_BYTES: u64 = 4 * 1024 * 1024;
/// The most of its summary read, unpacked.
pub const SUMMARY_BYTES: u64 = 64 * 1024;
/// The most requests one day's read, or one pass's publishing, sends.
pub const COST_REQUEST_LIMIT: usize = 1000;
/// How long one day's read, or one pass's publishing, may take.
pub const COST_SECONDS: u64 = 10 * 60;
/// The most runs GitHub lists for one filter, and the pages of them read.
const RUN_LIST_LIMIT: u64 = 1000;
const PER_PAGE: usize = 100;
/// Pages of a repository's artifacts read back to the day.
const ARTIFACT_PAGES: usize = 20;
/// The items the report ranks by cost.
const TOP_ITEMS: usize = 10;
/// The most characters of a model, item or outcome kept from a summary.
const TEXT_LIMIT: usize = 200;
/// The most a price may be, in dollars per million tokens: a typo of a few
/// zeros is no price.
const PRICE_LIMIT: f64 = 10_000.0;
/// The kinds a run is counted under ([`kind`]), [`OTHER`] last.
pub const KINDS: &[&str] = &[
    "review",
    "implement",
    "triage",
    "fix",
    "planning",
    "supervisor",
];
const OTHER: &str = "other";
/// What a summary that names no model counts under.
const UNKNOWN_MODEL: &str = "unknown";

/// One model's list prices, in US dollars per million tokens.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Price {
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
}

/// The configuration's `prices`, by model.
pub type Prices = BTreeMap<String, Price>;

pub fn check_prices(prices: &Prices) -> Result<()> {
    for (model, price) in prices {
        ensure!(!model.trim().is_empty(), "prices names an empty model");
        for (what, usd) in [
            ("input", price.input),
            ("cached_input", price.cached_input),
            ("output", price.output),
        ] {
            ensure!(
                usd.is_finite() && (0.0..=PRICE_LIMIT).contains(&usd),
                "prices.{model}.{what} is not 0 to {PRICE_LIMIT}"
            );
        }
    }
    Ok(())
}

/// A day, `YYYY-MM-DD`.
pub fn parse_date(text: &str) -> Result<Date> {
    let parts: Vec<&str> = text.split('-').collect();
    let date = match parts[..] {
        [year, month, day]
            if [(year, 4), (month, 2), (day, 2)]
                .iter()
                .all(|(p, n)| p.len() == *n && p.bytes().all(|c| c.is_ascii_digit())) =>
        {
            let month = time::Month::try_from(month.parse::<u8>()?).ok();
            month.and_then(|m| {
                Date::from_calendar_date(year.parse().ok()?, m, day.parse().ok()?).ok()
            })
        }
        _ => None,
    };
    date.with_context(|| format!("{text:?} is not a date YYYY-MM-DD"))
}

/// `date` as `YYYY-MM-DD`.
pub fn date_text(date: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// The repositories whose runs count: `work`, `follow` and `dispatch`, each
/// once, in order.
pub fn repositories(config: &crate::pass::Config) -> Vec<String> {
    let mut seen = BTreeSet::new();
    (config.work_repositories.iter())
        .chain(&config.follow)
        .chain([&config.dispatch_repo])
        .filter(|r| seen.insert(r.to_ascii_lowercase()))
        .cloned()
        .collect()
}

/// A run's four token counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Tokens {
    fn add(&mut self, other: &Tokens) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
    }

    /// Their cost at `price`.
    fn usd(&self, price: &Price) -> f64 {
        ((self.input + self.cache_write) as f64 * price.input
            + self.cache_read as f64 * price.cached_input
            + self.output as f64 * price.output)
            / 1e6
    }
}

/// One counted run, as the report lists it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Run {
    pub id: u64,
    pub url: String,
    pub kind: &'static str,
    pub repo: String,
    pub item: Option<String>,
    pub model: String,
    pub tokens: Tokens,
    /// None: unpriced, filled in by [`report`].
    pub usd: Option<f64>,
    pub outcome: Option<String>,
    pub duration_s: Option<f64>,
}

/// A run with an artifact that was not counted, and why.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Unread {
    pub id: u64,
    pub url: String,
    pub reason: String,
}

/// What one day's read found.
#[derive(Debug, Default, PartialEq)]
pub struct Day {
    pub repositories: Vec<String>,
    pub runs: Vec<Run>,
    pub unread: Vec<Unread>,
}

/// Whether `name` is an agentic-job run's artifact: [`ARTIFACT`], as a
/// review uploads it, or `PREFIX-agent-run`, as a dispatched run does
/// (`dispatch-implement-agent-run`).
fn is_agent_artifact(name: &str) -> bool {
    name.strip_suffix(ARTIFACT)
        .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('-'))
}

/// A summary's `workflow`, the agentic-job workflow a run used, and the kind
/// counted for it where nothing else names one: of the pair each serves
/// (implement or fix, triage or planning), the first.
const WORKFLOW_KINDS: &[(&str, &str)] = &[("branch", "implement"), ("analysis", "triage")];

/// The kind a run counts under, from its artifact's name, its workflow
/// file name, the first word of its title and then its summary's
/// `workflow`: the first of [`KINDS`] that a word of the artifact's prefix
/// (`dispatch-fix` of `dispatch-fix-agent-run`) or else of the file name,
/// each split at `-`, `_` and `.`, starts with (`plan` for planning,
/// `supervis` for supervisor), else the title's first word where it starts
/// with one of them, else [`WORKFLOW_KINDS`]'s, else [`OTHER`].
pub fn kind(artifact: &str, path: &str, title: &str, workflow: &str) -> &'static str {
    fn stem(kind: &'static str) -> &'static str {
        match kind {
            "planning" => "plan",
            "supervisor" => "supervis",
            kind => kind,
        }
    }
    let of = |words: &mut dyn Iterator<Item = &str>| -> Option<&'static str> {
        let words: Vec<String> = words.map(str::to_ascii_lowercase).collect();
        (KINDS.iter())
            .find(|k| words.iter().any(|w| w.starts_with(stem(k))))
            .copied()
    };
    let prefix = artifact.strip_suffix(ARTIFACT).unwrap_or_default();
    let file = path.rsplit('/').next().unwrap_or_default();
    (of(&mut prefix.split(['-', '_', '.'])))
        .or_else(|| of(&mut file.split(['-', '_', '.'])))
        .or_else(|| of(&mut title.split_whitespace().take(1)))
        .or_else(|| {
            (WORKFLOW_KINDS.iter())
                .find(|(w, _)| *w == workflow)
                .map(|(_, kind)| *kind)
        })
        .unwrap_or(OTHER)
}

/// Reads within [`COST_REQUEST_LIMIT`] and [`COST_SECONDS`], keeping each
/// summary in `cache`, a directory, by run id. Reads leave
/// [`COMMIT_REQUESTS`] of the requests to [`commit`], and nothing is sent
/// after a refusal under a rate limit.
pub struct Reader<'a, C> {
    client: &'a C,
    left: Cell<usize>,
    deadline: Instant,
    cache: Option<&'a Path>,
}

/// The requests of one [`commit`]: the branch, its commit, the tree, the
/// new commit and the branch's move.
const COMMIT_REQUESTS: usize = 5;
/// How long past the deadline those requests may still be sent, so that
/// the days read before it are published, as the pass's
/// [`FINISH_SECONDS`](crate::pass::FINISH_SECONDS) lets its writes finish.
const COMMIT_SECONDS: u64 = crate::pass::FINISH_SECONDS;
/// The shortest window of a day's runs listed apart ([`Reader::runs`]).
const MIN_WINDOW: time::Duration = time::Duration::hours(1);

impl<'a, C: Rest> Reader<'a, C> {
    pub fn new(client: &'a C, cache: Option<&'a Path>) -> Self {
        Self {
            client,
            left: Cell::new(COST_REQUEST_LIMIT),
            deadline: Instant::now() + Duration::from_secs(COST_SECONDS),
            cache,
        }
    }

    /// The reader sending nothing after `deadline` either.
    pub fn until(mut self, deadline: Instant) -> Self {
        self.deadline = self.deadline.min(deadline);
        self
    }

    /// The requests sent so far.
    pub fn spent(&self) -> usize {
        COST_REQUEST_LIMIT - self.left.get()
    }

    /// Whether a read can send no more.
    pub fn exhausted(&self) -> bool {
        self.left.get() <= COMMIT_REQUESTS || Instant::now() > self.deadline
    }

    fn spend(&self, read: bool) -> Result<()> {
        let left = self.left.get();
        let (floor, grace) = match read {
            true => (COMMIT_REQUESTS, Duration::ZERO),
            false => (0, Duration::from_secs(COMMIT_SECONDS)),
        };
        ensure!(
            left > floor,
            "the cost request budget ({COST_REQUEST_LIMIT}) is spent"
        );
        ensure!(
            Instant::now() <= self.deadline + grace,
            "the cost time bound is spent"
        );
        self.left.set(left - 1);
        Ok(())
    }

    /// `result`, sending nothing more after a rate limit refused it.
    fn observe<T>(&self, result: Result<T>) -> Result<T> {
        if let Err(error) = &result
            && error.is::<crate::github::RateLimited>()
        {
            self.left.set(0);
        }
        result
    }

    fn get(&self, url: &str) -> Result<RestResponse> {
        self.spend(true)?;
        let response = self.observe(self.client.get(url, None))?;
        ensure!(
            response.status == 200,
            "GET {}: GitHub HTTP status {}",
            url.trim_start_matches(API_ROOT),
            response.status
        );
        Ok(response)
    }

    /// A write, or a GET whose 404 is an answer, as [`Rest::send`]; it may
    /// spend the requests reads leave.
    pub(crate) fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        self.spend(false)?;
        self.observe(self.client.send(method, url, body))
    }

    /// The day's completed runs in `repository`: id, workflow path, title
    /// and URL. GitHub lists at most [`RUN_LIST_LIMIT`] runs for one
    /// filter, so a window with more is listed as two halves, down to
    /// [`MIN_WINDOW`].
    fn runs(&self, repository: &str, date: Date) -> Result<Vec<Value>> {
        let start = date.midnight().assume_utc();
        let day = (
            start,
            start + time::Duration::days(1) - time::Duration::seconds(1),
        );
        let mut windows = vec![day];
        let mut runs = Vec::new();
        while let Some((from, to)) = windows.pop() {
            let created = match (from, to) == day {
                true => date_text(date),
                false => format!("{}..{}", from.format(&Rfc3339)?, to.format(&Rfc3339)?),
            };
            match self.window(repository, &created)? {
                Some(listed) => runs.extend(listed),
                None => {
                    ensure!(
                        to - from >= MIN_WINDOW,
                        "{repository} has more than {RUN_LIST_LIMIT} runs created {created}"
                    );
                    let half = (to - from).whole_seconds() / 2;
                    let middle = from + time::Duration::seconds(half);
                    windows.push((middle + time::Duration::seconds(1), to));
                    windows.push((from, middle));
                }
            }
        }
        Ok(runs)
    }

    /// The completed runs in `repository` `created` in a window, or None
    /// where they are more than GitHub lists.
    fn window(&self, repository: &str, created: &str) -> Result<Option<Vec<Value>>> {
        let mut runs = Vec::new();
        for page in 1..=RUN_LIST_LIMIT as usize / PER_PAGE {
            let url = format!(
                "{API_ROOT}repos/{repository}/actions/runs?created={created}&status=completed&per_page={PER_PAGE}&page={page}"
            );
            let body = self.get(&url)?.body;
            let total = body["total_count"].as_u64().context("runs have no total")?;
            if total > RUN_LIST_LIMIT {
                return Ok(None);
            }
            let listed = body["workflow_runs"]
                .as_array()
                .context("workflow_runs is not a list")?;
            runs.extend(listed.iter().cloned());
            if listed.len() < PER_PAGE || runs.len() as u64 >= total {
                return Ok(Some(runs));
            }
        }
        bail!("{repository}'s runs created {created} are more than GitHub lists")
    }

    /// `repository`'s agent run artifacts ([`is_agent_artifact`]) created
    /// since `start`, by run id: GitHub lists them newest first, so the
    /// listing stops at the first older one. The listing is of every
    /// artifact, as dispatched runs name theirs apart; a run with several
    /// keeps the shortest name, and of the same name the newest.
    fn artifacts(&self, repository: &str, start: OffsetDateTime) -> Result<BTreeMap<u64, Value>> {
        let mut found: BTreeMap<u64, Value> = BTreeMap::new();
        for page in 1..=ARTIFACT_PAGES {
            let url = format!(
                "{API_ROOT}repos/{repository}/actions/artifacts?per_page={PER_PAGE}&page={page}"
            );
            let body = self.get(&url)?.body;
            let listed = body["artifacts"]
                .as_array()
                .context("artifacts is not a list")?;
            for artifact in listed {
                let created = OffsetDateTime::parse(text(artifact, "/created_at"), &Rfc3339)
                    .context("artifact has no created_at")?;
                if created < start {
                    return Ok(found);
                }
                let name = text(artifact, "/name");
                let Some(run) = artifact["workflow_run"]["id"].as_u64() else {
                    continue;
                };
                if !is_agent_artifact(name) {
                    continue;
                }
                // Of a run's, the shortest name counts, and of one name the
                // newest (a re-run's), listed first.
                let shorter = |kept: &Value| name.len() < text(kept, "/name").len();
                if found.get(&run).is_none_or(shorter) {
                    found.insert(run, artifact.clone());
                }
            }
            if listed.len() < PER_PAGE {
                return Ok(found);
            }
        }
        bail!(
            "{repository} has more than {} artifacts since the day began",
            ARTIFACT_PAGES * PER_PAGE
        )
    }

    /// `run`'s summary, from the cache or else downloaded from `artifact`,
    /// or why the artifact holds none to read. An error is the read's
    /// (a request, the budget, the cache), not the artifact's: the day is
    /// not read whole.
    fn summary(&self, repository: &str, run: u64, artifact: &Value) -> Result<Result<Vec<u8>>> {
        let cached = self.cache.map(|dir| dir.join(format!("{run}.json")));
        if let Some(path) = cached.as_ref().filter(|p| p.exists()) {
            let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
            ensure!(
                bytes.len() as u64 <= SUMMARY_BYTES,
                "{} is over {SUMMARY_BYTES} bytes",
                path.display()
            );
            return Ok(Ok(bytes));
        }
        if artifact["expired"] == true {
            return Ok(Err(anyhow::anyhow!("its artifact expired")));
        }
        let size = artifact["size_in_bytes"].as_u64().unwrap_or(u64::MAX);
        if size > ARTIFACT_BYTES {
            return Ok(Err(anyhow::anyhow!(
                "its artifact is over {ARTIFACT_BYTES} bytes"
            )));
        }
        let id = artifact["id"].as_u64().context("artifact has no id")?;
        self.spend(true)?;
        let url = format!("{API_ROOT}repos/{repository}/actions/artifacts/{id}/zip");
        let zip = self.observe(self.client.download(&url, ARTIFACT_BYTES))?;
        let bytes = match unzip(&zip, SUMMARY_FILE, SUMMARY_BYTES) {
            Ok(bytes) => bytes,
            unreadable => return Ok(unreadable),
        };
        if let Some(path) = cached {
            // Renamed into place, so that no half-written summary is read.
            let partial = path.with_extension("json.partial");
            std::fs::write(&partial, &bytes)
                .and_then(|()| std::fs::rename(&partial, &path))
                .with_context(|| format!("write {}", path.display()))?;
        }
        Ok(Ok(bytes))
    }
}

/// Every counted run of `date` in `repositories`, and those not counted.
pub fn read_day<C: Rest>(reader: &Reader<C>, repositories: &[String], date: Date) -> Result<Day> {
    let start = date.midnight().assume_utc();
    let mut day = Day {
        repositories: repositories.to_vec(),
        ..Day::default()
    };
    for repository in repositories {
        validate_repository(repository)?;
        let runs = reader.runs(repository, date)?;
        if runs.is_empty() {
            continue;
        }
        let artifacts = reader.artifacts(repository, start)?;
        for run in &runs {
            let Some(id) = run["id"].as_u64() else {
                continue;
            };
            let Some(artifact) = artifacts.get(&id) else {
                continue;
            };
            let url = format!("https://github.com/{repository}/actions/runs/{id}");
            let read = (reader.summary(repository, id, artifact)?)
                .and_then(|bytes| counted(&bytes, repository, run, artifact, &url));
            match read {
                Ok(counted) => day.runs.push(counted),
                Err(error) => day.unread.push(Unread {
                    id,
                    url,
                    reason: format!("{error:#}"),
                }),
            }
        }
    }
    day.runs.sort_by_key(|r| r.id);
    day.unread.sort_by_key(|r| r.id);
    Ok(day)
}

/// At most [`TEXT_LIMIT`] characters of `text`, without control
/// characters, which `--human` would print to a terminal.
fn clip(text: &str) -> String {
    (text.chars())
        .filter(|c| !c.is_control())
        .take(TEXT_LIMIT)
        .collect()
}

/// The run `summary` describes, `run` in `repository` as listed, with its
/// `artifact`.
fn counted(
    summary: &[u8],
    repository: &str,
    run: &Value,
    artifact: &Value,
    url: &str,
) -> Result<Run> {
    let summary: Value = serde_json::from_slice(summary).context("summary.json is not JSON")?;
    let tokens = summary["tokens"]
        .as_object()
        .context("summary.json has no tokens")?;
    let count = |key: &str| -> Result<u64> {
        match tokens.get(key) {
            None | Some(Value::Null) => Ok(0),
            Some(n) => n
                .as_u64()
                .with_context(|| format!("summary.json tokens.{key} is not a count")),
        }
    };
    let repo = (summary["repo"].as_str())
        .filter(|r| validate_repository(r).is_ok())
        .unwrap_or(repository)
        .to_owned();
    let item = match &summary["item"] {
        Value::Number(n) if n.as_u64().is_some() => Some(format!("{repo}#{n}")),
        Value::String(s) if !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()) => {
            Some(format!("{repo}#{s}"))
        }
        Value::String(s) if !s.trim().is_empty() => Some(clip(s.trim())),
        _ => None,
    };
    Ok(Run {
        id: run["id"].as_u64().context("run has no id")?,
        url: url.to_owned(),
        kind: kind(
            text(artifact, "/name"),
            text(run, "/path"),
            text(run, "/display_title"),
            summary["workflow"].as_str().unwrap_or_default(),
        ),
        repo,
        item,
        model: (summary["model"].as_str())
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map_or(UNKNOWN_MODEL.to_owned(), clip),
        tokens: Tokens {
            input: count("input")?,
            output: count("output")?,
            cache_read: count("cache_read")?,
            cache_write: count("cache_write")?,
        },
        usd: None,
        outcome: summary["outcome"].as_str().map(clip),
        duration_s: summary["duration_s"].as_f64().filter(|d| d.is_finite()),
    })
}

/// `usd` to a hundredth of a cent, so the report reads the same each time.
fn cents(usd: f64) -> f64 {
    (usd * 1e4).round() / 1e4
}

/// The runs, tokens and priced cost of one group.
#[derive(Default, Serialize)]
struct Group {
    runs: u64,
    tokens: Tokens,
    usd: f64,
    unpriced_runs: u64,
}

impl Group {
    fn add(&mut self, run: &Run) {
        self.runs += 1;
        self.tokens.add(&run.tokens);
        match run.usd {
            Some(usd) => self.usd += usd,
            None => self.unpriced_runs += 1,
        }
    }

    /// Its `usd` is null where none of its runs is priced, never $0.
    fn json(&self) -> Value {
        let usd = (self.runs > self.unpriced_runs).then(|| cents(self.usd));
        json!({"runs": self.runs, "tokens": self.tokens, "usd": usd,
            "unpriced_runs": self.unpriced_runs})
    }
}

fn groups(map: &BTreeMap<String, Group>) -> Value {
    Value::Object(map.iter().map(|(k, g)| (k.clone(), g.json())).collect())
}

/// The day's [`COST_DAY_SCHEMA`] report, its runs priced by `prices`.
pub fn report(date: Date, mut day: Day, prices: &Prices) -> Value {
    let mut total = Group::default();
    let mut by: [BTreeMap<String, Group>; 4] = Default::default();
    let mut unpriced: BTreeMap<String, Group> = BTreeMap::new();
    for run in &mut day.runs {
        run.usd = prices.get(&run.model).map(|p| run.tokens.usd(p));
        total.add(run);
        let item = run.item.clone();
        let keys = [
            Some(run.kind.to_owned()),
            Some(run.model.clone()),
            Some(run.repo.clone()),
            item,
        ];
        for (map, key) in by.iter_mut().zip(keys) {
            if let Some(key) = key {
                map.entry(key).or_default().add(run);
            }
        }
        if run.usd.is_none() {
            unpriced.entry(run.model.clone()).or_default().add(run);
        }
    }
    let [by_kind, by_model, by_repository, by_item] = by;
    let mut top: Vec<(&String, &Group)> = by_item.iter().filter(|(_, g)| g.usd > 0.0).collect();
    top.sort_by(|a, b| b.1.usd.total_cmp(&a.1.usd).then_with(|| a.0.cmp(b.0)));
    let top: Vec<Value> = (top.into_iter().take(TOP_ITEMS))
        .map(|(item, g)| {
            let mut entry = g.json();
            entry["item"] = json!(item);
            entry
        })
        .collect();
    let unpriced: BTreeMap<&String, Value> = (unpriced.iter())
        .map(|(model, g)| (model, json!({"runs": g.runs, "tokens": g.tokens})))
        .collect();
    let runs: Vec<Value> = (day.runs.iter())
        .map(|run| {
            let mut entry = json!(run);
            entry["usd"] = json!(run.usd.map(cents));
            entry
        })
        .collect();
    json!({
        "schema": COST_DAY_SCHEMA,
        "date": date_text(date),
        "repositories": day.repositories,
        "totals": total.json(),
        "by_kind": groups(&by_kind),
        "by_model": groups(&by_model),
        "by_repository": groups(&by_repository),
        "top_items": top,
        "unpriced": unpriced,
        "runs": runs,
        "unread": day.unread,
    })
}

/// A report's short table, for `--human`.
pub fn human(report: &Value) -> String {
    let totals = &report["totals"];
    let usd = (totals["usd"].as_f64()).map_or("none priced".into(), |usd| {
        format!("${usd:.2} at list prices")
    });
    let mut out = format!(
        "cost {}: {} run(s), {usd}; {} unpriced, {} unread\n",
        text(report, "/date"),
        totals["runs"],
        totals["unpriced_runs"],
        report["unread"].as_array().map_or(0, Vec::len),
    );
    let row = |name: &str, g: &Value| {
        let tokens: u64 = ["input", "output", "cache_read", "cache_write"]
            .iter()
            .filter_map(|k| g["tokens"][k].as_u64())
            .sum();
        // An unpriced group has no dollar amount, not $0.
        let usd = g["usd"]
            .as_f64()
            .map_or("-".into(), |usd| format!("${usd:.2}"));
        format!(
            "  {name:<40} {:>5} {:>8} {:>12} {usd:>10}\n",
            g["runs"].as_u64().unwrap_or_default(),
            g["unpriced_runs"].as_u64().unwrap_or_default(),
            tokens
        )
    };
    for (title, key) in [
        ("kind", "by_kind"),
        ("model", "by_model"),
        ("repository", "by_repository"),
    ] {
        out.push_str(&format!(
            "{title:<42} {:>5} {:>8} {:>12} {:>10}\n",
            "runs", "unpriced", "tokens", "usd"
        ));
        for (name, g) in report[key].as_object().into_iter().flatten() {
            out.push_str(&row(name, g));
        }
    }
    if let Some(top) = report["top_items"].as_array().filter(|t| !t.is_empty()) {
        out.push_str("top items\n");
        for g in top {
            out.push_str(&row(text(g, "/item"), g));
        }
    }
    for (model, g) in report["unpriced"].as_object().into_iter().flatten() {
        let tokens = &g["tokens"];
        out.push_str(&format!(
            "unpriced {model}: {} run(s), {} input, {} cache-read, {} cache-write, {} output tokens\n",
            g["runs"], tokens["input"], tokens["cache_read"], tokens["cache_write"], tokens["output"]
        ));
    }
    out
}

fn u16_at(bytes: &[u8], at: usize) -> Result<usize> {
    let b = bytes.get(at..at + 2).context("zip truncated")?;
    Ok(u16::from_le_bytes([b[0], b[1]]) as usize)
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u64> {
    let b = bytes.get(at..at + 4).context("zip truncated")?;
    Ok(u64::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]])))
}

const END_SIGNATURE: [u8; 4] = *b"PK\x05\x06";
const CENTRAL_SIGNATURE: [u8; 4] = *b"PK\x01\x02";
const LOCAL_SIGNATURE: [u8; 4] = *b"PK\x03\x04";
const STORED: usize = 0;
const DEFLATED: usize = 8;

/// The file `name` at the top of `zip`, at most `limit` bytes unpacked:
/// stored or deflated, not encrypted, not ZIP64.
pub fn unzip(zip: &[u8], name: &str, limit: u64) -> Result<Vec<u8>> {
    // The end record is 22 bytes and a comment of at most 65535.
    let floor = zip.len().saturating_sub(22 + 0xFFFF);
    let end = (floor..=zip.len().saturating_sub(22))
        .rev()
        .find(|&at| zip[at..].starts_with(&END_SIGNATURE))
        .context("not a zip: no end of central directory")?;
    let entries = u16_at(zip, end + 10)?;
    let mut at = u32_at(zip, end + 16)? as usize;
    for _ in 0..entries {
        ensure!(
            zip.get(at..)
                .is_some_and(|b| b.starts_with(&CENTRAL_SIGNATURE)),
            "zip central directory is malformed"
        );
        let (flags, method) = (u16_at(zip, at + 8)?, u16_at(zip, at + 10)?);
        let (packed, size) = (u32_at(zip, at + 20)?, u32_at(zip, at + 24)?);
        let (name_len, extra, comment) = (
            u16_at(zip, at + 28)?,
            u16_at(zip, at + 30)?,
            u16_at(zip, at + 32)?,
        );
        let local = u32_at(zip, at + 42)? as usize;
        let entry = zip
            .get(at + 46..at + 46 + name_len)
            .context("zip truncated")?;
        at += 46 + name_len + extra + comment;
        if entry != name.as_bytes() {
            continue;
        }
        ensure!(flags & 1 == 0, "{name} is encrypted");
        ensure!(
            packed != 0xFFFF_FFFF && size != 0xFFFF_FFFF,
            "{name} is in ZIP64, which is not read"
        );
        ensure!(size <= limit, "{name} is over {limit} bytes");
        ensure!(
            zip.get(local..)
                .is_some_and(|b| b.starts_with(&LOCAL_SIGNATURE)),
            "zip local header of {name} is malformed"
        );
        let start = local + 30 + u16_at(zip, local + 26)? + u16_at(zip, local + 28)?;
        let data =
            (zip.get(start..start.saturating_add(packed as usize))).context("zip truncated")?;
        let mut out = Vec::new();
        match method {
            STORED => out.extend_from_slice(data),
            DEFLATED => {
                flate2::read::DeflateDecoder::new(data)
                    .take(limit + 1)
                    .read_to_end(&mut out)
                    .with_context(|| format!("inflate {name}"))?;
            }
            other => bail!("{name} is packed by method {other}, not stored or deflated"),
        }
        ensure!(
            out.len() as u64 == size,
            "{name} unpacked to {} bytes, not the {size} its entry says",
            out.len()
        );
        return Ok(out);
    }
    bail!("the zip has no {name}")
}

/// The repository branch the pass publishes to: never the default branch.
pub const DATA_BRANCH: &str = "data";
/// The directory of the published days on it.
pub const DATA_DIR: &str = "cost";
/// The days before today a publishing pass makes sure are published.
pub const PUBLISH_DAYS: i64 = 7;

/// `cost` in the pass configuration: from `hourUtc` each day, the pass
/// publishes the reports of the last [`PUBLISH_DAYS`] days that are not on
/// [`DATA_BRANCH`] yet.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Publish {
    pub hour_utc: u8,
    /// A directory that keeps each read summary by run id, as `state` is
    /// kept; None: no cache.
    pub cache: Option<String>,
}

impl Publish {
    pub fn check(&self, config: &crate::pass::Config) -> Result<()> {
        ensure!(self.hour_utc < 24, "cost hourUtc is not 0 to 23");
        ensure!(
            config.epics.is_some(),
            "cost publishes to epics.repository, which is not set"
        );
        Ok(())
    }

    /// The days due at `now`, oldest first: none before `hourUtc`, else the
    /// last [`PUBLISH_DAYS`] before today.
    pub fn days(&self, now: OffsetDateTime) -> Vec<Date> {
        if now.hour() < self.hour_utc {
            return Vec::new();
        }
        (1..=PUBLISH_DAYS)
            .rev()
            .filter_map(|back| now.date().checked_sub(time::Duration::days(back)))
            .collect()
    }
}

/// The days already on `repository`'s [`DATA_BRANCH`], by file name: none
/// where the branch or its [`DATA_DIR`] is missing.
pub fn published<C: Rest>(reader: &Reader<C>, repository: &str) -> Result<BTreeSet<String>> {
    let url = format!("{API_ROOT}repos/{repository}/contents/{DATA_DIR}?ref={DATA_BRANCH}");
    let (status, body) = reader.send("GET", &url, &Value::Null)?;
    match status {
        404 => Ok(BTreeSet::new()),
        200 => Ok((body.as_array().context("cost directory is not a list")?)
            .iter()
            .map(|entry| text(entry, "/name").to_owned())
            .collect()),
        status => {
            bail!("listing {DATA_DIR} on {repository}@{DATA_BRANCH}: GitHub HTTP status {status}")
        }
    }
}

/// Commits `files` (path, text) to `repository`'s [`DATA_BRANCH`] by the
/// git data API, creating the branch where it is missing; the new commit.
pub fn commit<C: Rest>(
    reader: &Reader<C>,
    repository: &str,
    files: &[(String, String)],
    message: &str,
) -> Result<String> {
    let api = format!("{API_ROOT}repos/{repository}/git");
    let ok = |what: &str, (status, body): (u16, Value)| -> Result<Value> {
        ensure!(
            (200..300).contains(&status),
            "{what} on {repository}: GitHub HTTP status {status}: {}",
            body["message"].as_str().unwrap_or("no message")
        );
        Ok(body)
    };
    let branch = format!("heads/{DATA_BRANCH}");
    let parent = match reader.send("GET", &format!("{api}/ref/{branch}"), &Value::Null)? {
        (404, _) => None,
        answer => Some(text(&ok("reading the data branch", answer)?, "/object/sha").to_owned()),
    };
    let base = match &parent {
        Some(sha) => {
            let read = reader.send("GET", &format!("{api}/commits/{sha}"), &Value::Null)?;
            Some(text(&ok("reading the data commit", read)?, "/tree/sha").to_owned())
        }
        None => None,
    };
    let entries: Vec<Value> = (files.iter())
        .map(|(path, content)| {
            json!({"path": path, "mode": "100644", "type": "blob", "content": content})
        })
        .collect();
    let mut tree = json!({"tree": entries});
    if let Some(base) = &base {
        tree["base_tree"] = json!(base);
    }
    let tree = ok(
        "writing the tree",
        reader.send("POST", &format!("{api}/trees"), &tree)?,
    )?;
    let commit = json!({"message": message, "tree": text(&tree, "/sha"),
        "parents": parent.iter().collect::<Vec<_>>()});
    let commit = ok(
        "writing the commit",
        reader.send("POST", &format!("{api}/commits"), &commit)?,
    )?;
    let sha = text(&commit, "/sha").to_owned();
    ensure!(!sha.is_empty(), "the new commit has no sha");
    let moved = match parent {
        Some(_) => reader.send(
            "PATCH",
            &format!("{api}/refs/{branch}"),
            &json!({"sha": sha, "force": false}),
        )?,
        None => reader.send(
            "POST",
            &format!("{api}/refs"),
            &json!({"ref": format!("refs/{branch}"), "sha": sha}),
        )?,
    };
    ok("moving the data branch", moved)?;
    Ok(sha)
}

#[cfg(test)]
pub(crate) mod tests;
