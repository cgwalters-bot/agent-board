//! The read-only half of `agent-board prune` (agent-board#280,
//! docs/prune.md): the open issues of the pass configuration's
//! `workRepositories` that are candidates to close, each with its reasons and
//! the links that are their evidence. It writes nothing to the forge: closing
//! is a later, separate change, and the operator reads this list first.
//!
//! An issue labelled [`KEEP_LABEL`] is skipped whole. Of the rest, an issue is
//! a candidate when it is:
//!
//! - **overtaken**: GitHub takes a merged pull request of a work repository
//!   as closing it (read only for an issue a merged pull request
//!   cross-referenced);
//! - **duplicate**: an older open issue of its repository has the same title
//!   stem ([`title_stem`]); the oldest of them stays;
//! - **stale**: its board Priority is neither P0 nor P1 (or it has none), it
//!   holds no open sub-issue and blocks no issue, and nothing happened for
//!   `stale_days`: idle time starts at its creation, a person's timeline event
//!   (not one that only names them, [`PASSIVE_EVENTS`]) or a pull request's
//!   cross-reference, so the bot's comments, labels and board passes never
//!   make it look active;
//! - **triage-done**: the newest triage verdict the bot posted on it is `done`
//!   ([`crate::plan::TRIAGE_FENCE`]), and no person commented after it.
//!
//! Each REST GET is conditional, as the poll's are ([`crate::poll`]): with the
//! cache of the last read, an unchanged list answers 304 and costs no rate
//! budget. The board's Priority comes from one GraphQL read, which cannot be
//! revalidated. Every read, 304s included, is spent from a budget; running
//! out refuses the read rather than list part of the backlog. The transport
//! retries a read refused under a rate limit at most
//! [`crate::github::RATE_LIMIT_RETRIES`] times, so the HTTP requests are at
//! most that many more per read.
use crate::github::{API_ROOT, Transport};
use crate::pass::Config;
use crate::plan::triage_verdict;
use crate::poll::{Rest, next_link};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub const PRUNE_CACHE_SCHEMA: &str = "agent-board/prune-cache/v1";
/// Every read of one prune, 304s included: a page per hundred open issues
/// of each repository, one timeline page per issue, a closing-reference read
/// per issue a merged pull request referenced, and the board's pages. Rate
/// limit retries are not counted ([`crate::github::RATE_LIMIT_RETRIES`]).
pub const PRUNE_REQUEST_LIMIT: usize = 1000;
pub const DEFAULT_STALE_DAYS: u64 = 21;
pub const STALE_DAYS_LIMIT: u64 = 365;
/// The label that opts an issue out of pruning.
pub const KEEP_LABEL: &str = "keep";
/// Board priorities whose issues are never stale.
const URGENT_PRIORITIES: &[&str] = &["P0", "P1"];
/// Timeline events whose actor is the person they name, not the writer of
/// what named them: a bot's comment that @-mentions a person adds them.
const PASSIVE_EVENTS: &[&str] = &["mentioned", "subscribed"];
/// Pages of the board's open issues, as the pass reads at most.
const BOARD_PAGES: usize = 50;

/// `projectV2` is not on the `RepositoryOwner` interface, only on its types,
/// as in the pass's board query.
const BOARD_QUERY: &str = r#"query($owner: String!, $number: Int!, $field: String!, $cursor: String) {
  repositoryOwner(login: $owner) {
    ... on Organization { projectV2(number: $number) { ...Board } }
    ... on User { projectV2(number: $number) { ...Board } }
  }
}
fragment Board on ProjectV2 {
  field(name: $field) { ... on ProjectV2SingleSelectField { name } }
  items(first: 100, after: $cursor, query: "is:issue is:open") {
    nodes {
      priority: fieldValueByName(name: $field) { ... on ProjectV2ItemFieldSingleSelectValue { name } }
      content { __typename ... on Issue { number repository { nameWithOwner } } }
    }
    pageInfo { hasNextPage endCursor }
  }
}"#;

/// The pull requests GitHub takes as closing an issue, merged ones included:
/// a cross-reference alone may say anything, "does not fix" included.
const CLOSING_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) { issue(number: $number) {
    closedByPullRequestsReferences(first: 50, includeClosedPrs: true) {
      nodes { url merged repository { nameWithOwner } } totalCount
    }
  } }
}"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    Overtaken,
    Duplicate,
    Stale,
    TriageDone,
}

impl Reason {
    fn name(self) -> &'static str {
        match self {
            Self::Overtaken => "overtaken",
            Self::Duplicate => "duplicate",
            Self::Stale => "stale",
            Self::TriageDone => "triage-done",
        }
    }
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Finding {
    pub reason: Reason,
    pub detail: String,
    /// Links to what shows it, sorted.
    pub evidence: Vec<String>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Candidate {
    pub repository: String,
    pub number: u64,
    pub url: String,
    pub title: String,
    pub priority: Option<String>,
    pub reasons: Vec<Finding>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Report {
    pub now: String,
    pub stale_days: u64,
    pub repositories: Vec<String>,
    /// Open issues read, pull requests left out.
    pub open: usize,
    /// Open issues labelled [`KEEP_LABEL`], not judged.
    pub kept: usize,
    pub requests: usize,
    pub not_modified: usize,
    /// By repository and number.
    pub candidates: Vec<Candidate>,
}

impl Report {
    /// The list for a person: a summary line, then each candidate's link and
    /// title with a line per reason.
    pub fn text(&self) -> String {
        let mut out = format!(
            "prune: {} candidates among {} open issues ({} kept) in {} repositories; nothing written\n",
            self.candidates.len(),
            self.open,
            self.kept,
            self.repositories.len()
        );
        for c in &self.candidates {
            let _ = writeln!(out, "\n{} {}", c.url, c.title);
            for f in &c.reasons {
                let _ = writeln!(out, "  {}: {}", f.reason.name(), f.detail);
                for link in &f.evidence {
                    let _ = writeln!(out, "    {link}");
                }
            }
        }
        let _ = writeln!(
            out,
            "\n{} requests, {} not modified",
            self.requests, self.not_modified
        );
        out
    }
}

/// Responses by URL, as the poll keeps them: `etag`, `last_modified`, `next`
/// and a slimmed `body`. Losing it costs requests, not correctness.
#[derive(Debug, Default, PartialEq)]
pub struct Cache(pub BTreeMap<String, Value>);

impl Cache {
    pub fn from_json(value: &Value) -> Result<Self> {
        let object = value.as_object().context("prune cache is not an object")?;
        ensure!(
            object.get("schema").and_then(Value::as_str) == Some(PRUNE_CACHE_SCHEMA),
            "unsupported prune cache schema (expected {PRUNE_CACHE_SCHEMA}); the file is a cache and may be deleted"
        );
        for key in object.keys() {
            ensure!(
                ["schema", "responses"].contains(&key.as_str()),
                "unknown prune cache field {key}"
            );
        }
        let responses = object
            .get("responses")
            .and_then(Value::as_object)
            .context("prune cache responses is not an object")?
            .iter()
            .map(|(url, entry)| {
                ensure!(url.starts_with(API_ROOT), "prune cache URL outside the API");
                ensure!(
                    entry.get("body").is_some(),
                    "prune cache response has no body"
                );
                Ok((url.clone(), entry.clone()))
            })
            .collect::<Result<_>>()?;
        Ok(Self(responses))
    }

    pub fn to_json(&self) -> Value {
        json!({"schema": PRUNE_CACHE_SCHEMA, "responses": self.0})
    }
}

/// Reads through one request budget and the cache, keeping what it used.
struct Reader<'a, C> {
    client: &'a C,
    previous: Cache,
    used: Cache,
    limit: usize,
    requests: usize,
    not_modified: usize,
}

impl<C: Rest + Transport> Reader<'_, C> {
    fn spend(&mut self) -> Result<()> {
        ensure!(
            self.requests < self.limit,
            "prune request budget ({}) exhausted; nothing was listed",
            self.limit
        );
        self.requests += 1;
        Ok(())
    }

    /// One conditional GET; `slim` is applied to each element of a fresh list.
    fn page(&mut self, url: &str, slim: fn(&Value) -> Value) -> Result<(Value, Option<String>)> {
        ensure!(url.starts_with(API_ROOT), "pagination left the API: {url}");
        self.spend()?;
        let cached = self.previous.0.get(url).cloned();
        let validator = cached.as_ref().and_then(|c| {
            c["etag"]
                .as_str()
                .map(|v| ("If-None-Match", v))
                .or(c["last_modified"]
                    .as_str()
                    .map(|v| ("If-Modified-Since", v)))
        });
        let response = self.client.get(url, validator)?;
        let entry = match (response.status, cached) {
            // The ETag covers the body only: a list that grew past this page
            // answers 304 with a new `Link`.
            (304, Some(mut cached)) => {
                self.not_modified += 1;
                if let Some(link) = response.link {
                    cached["next"] = next_link(&link).into();
                }
                cached
            }
            (304, None) => bail!("GET {url}: 304 for a request that was not conditional"),
            (200, _) => {
                let list = response
                    .body
                    .as_array()
                    .with_context(|| format!("GET {url}: not a list"))?;
                json!({
                    "etag": response.etag,
                    "last_modified": response.last_modified,
                    "next": response.link.as_deref().and_then(next_link),
                    "body": list.iter().map(slim).collect::<Vec<_>>(),
                })
            }
            (status, _) => bail!("GET {url}: GitHub HTTP status {status}"),
        };
        self.used.0.insert(url.to_owned(), entry.clone());
        Ok((
            entry["body"].clone(),
            entry["next"].as_str().map(str::to_owned),
        ))
    }

    /// Every page of a list, following `Link` within the API only.
    fn list(&mut self, path: &str, slim: fn(&Value) -> Value) -> Result<Vec<Value>> {
        let mut url = Some(format!("{API_ROOT}{path}"));
        let mut all = Vec::new();
        while let Some(page) = url {
            let (body, next) = self.page(&page, slim)?;
            all.extend(body.as_array().cloned().unwrap_or_default());
            url = next;
        }
        Ok(all)
    }

    /// The board's open issues' Priority, by [`key`]; null for none.
    fn priorities(&mut self, config: &Config) -> Result<BTreeMap<String, Option<String>>> {
        let board = config.board.as_ref().context("no board")?;
        let mut cursor = Value::Null;
        let mut priorities = BTreeMap::new();
        for _ in 0..BOARD_PAGES {
            self.spend()?;
            let data = self.client.graphql(
                BOARD_QUERY,
                json!({"owner": board.owner, "number": board.project,
                    "field": board.priority_field, "cursor": cursor}),
            )?;
            let project = &data["repositoryOwner"]["projectV2"];
            ensure!(project.is_object(), "the board could not be read");
            // Without the field every issue would read as unprioritized,
            // and the urgent ones as stale.
            ensure!(
                project["field"]["name"].is_string(),
                "the board has no {} single-select field",
                board.priority_field
            );
            let items = &project["items"];
            for node in items["nodes"]
                .as_array()
                .context("the board could not be read")?
            {
                let content = &node["content"];
                match content["__typename"].as_str() {
                    Some("Issue") => {}
                    Some(_) => continue,
                    None => bail!("a board item could not be read; the board is incomplete"),
                }
                let repository = content["repository"]["nameWithOwner"]
                    .as_str()
                    .context("a board issue has no repository")?;
                let number = content["number"]
                    .as_u64()
                    .context("a board issue has no number")?;
                let priority = node["priority"]["name"].as_str().map(str::to_owned);
                priorities.insert(key(repository, number), priority);
            }
            match items["pageInfo"]["hasNextPage"].as_bool() {
                Some(false) => return Ok(priorities),
                Some(true) => cursor = items["pageInfo"]["endCursor"].clone(),
                None => bail!("board items have no pageInfo"),
            }
        }
        bail!("the board has more than {} open issues", BOARD_PAGES * 100)
    }

    /// The merged pull requests of `scope` that GitHub takes as closing
    /// `repository#number`, sorted.
    fn closing(
        &mut self,
        repository: &str,
        number: u64,
        scope: &BTreeSet<String>,
    ) -> Result<Vec<String>> {
        let (owner, name) = repository
            .split_once('/')
            .with_context(|| format!("invalid repository {repository}"))?;
        self.spend()?;
        let data = self.client.graphql(
            CLOSING_QUERY,
            json!({"owner": owner, "name": name, "number": number}),
        )?;
        let refs = &data["repository"]["issue"]["closedByPullRequestsReferences"];
        let nodes = refs["nodes"]
            .as_array()
            .with_context(|| format!("{repository}#{number}: closing references not read"))?;
        ensure!(
            refs["totalCount"].as_u64() == Some(nodes.len() as u64),
            "{repository}#{number}: more closing references than one page"
        );
        let mut closing = BTreeSet::new();
        for node in nodes {
            let source = node["repository"]["nameWithOwner"].as_str();
            let in_scope = source.is_some_and(|s| scope.contains(&s.to_ascii_lowercase()));
            if node["merged"] == true
                && in_scope
                && let Some(url) = node["url"].as_str()
            {
                closing.insert(url.to_owned());
            }
        }
        Ok(closing.into_iter().collect())
    }
}

fn key(repository: &str, number: u64) -> String {
    format!("{repository}#{number}").to_ascii_lowercase()
}

/// Copies the `keys` that `value` has, so a missing fact stays missing.
fn pick(value: &Value, keys: &[&str]) -> Value {
    let picked: Map<String, Value> = keys
        .iter()
        .filter_map(|k| Some(((*k).to_owned(), value.get(*k)?.clone())))
        .collect();
    Value::Object(picked)
}

fn slim_user(user: &Value) -> Value {
    pick(user, &["login", "type"])
}

/// What an open-issue list keeps of an issue.
fn slim_issue(issue: &Value) -> Value {
    let mut slim = pick(
        issue,
        &[
            "number",
            "title",
            "html_url",
            "created_at",
            "sub_issues_summary",
            "issue_dependencies_summary",
        ],
    );
    slim["user"] = slim_user(&issue["user"]);
    slim["labels"] = (issue["labels"].as_array().into_iter().flatten())
        .map(|l| l["name"].clone())
        .collect();
    slim["pull"] = issue.get("pull_request").is_some().into();
    slim
}

/// What a timeline keeps of an event: a comment's body only where it may
/// hold a triage verdict.
fn slim_event(event: &Value) -> Value {
    let mut slim = pick(event, &["event", "created_at", "html_url"]);
    for who in ["user", "actor"] {
        if event[who].is_object() {
            slim[who] = slim_user(&event[who]);
        }
    }
    if let Some(body) = event["body"].as_str()
        && body.contains(crate::plan::TRIAGE_FENCE)
    {
        slim["body"] = body.into();
    }
    let source = &event["source"]["issue"];
    if source.is_object() {
        let mut issue = pick(source, &["html_url"]);
        if let Some(pull) = source.get("pull_request") {
            issue["pull_request"] = pick(pull, &["merged_at"]);
        }
        slim["source"] = json!({"issue": issue});
    }
    slim
}

/// A title as duplicates are matched: lowercase words of letters and digits,
/// one space apart.
pub fn title_stem(title: &str) -> String {
    title
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `user` is a person: not an app, nor the configured bot.
fn person(user: &Value, bot: &str) -> bool {
    let login = user["login"].as_str().unwrap_or_default();
    !login.is_empty()
        && user["type"].as_str() != Some("Bot")
        && !login.ends_with("[bot]")
        && !login.eq_ignore_ascii_case(bot)
}

fn parse_time(value: &Value, what: &str) -> Result<OffsetDateTime> {
    let text = value
        .as_str()
        .with_context(|| format!("{what} has no time"))?;
    OffsetDateTime::parse(text, &Rfc3339).with_context(|| format!("{what}: invalid time {text:?}"))
}

/// What the read may say about an issue, beside duplicates.
struct Judge<'a> {
    bot: &'a str,
    now: OffsetDateTime,
    stale_days: u64,
}

impl Judge<'_> {
    /// The findings beside overtaken and duplicate, and whether a merged
    /// pull request cross-referenced the issue, so it may be overtaken.
    fn judge(
        &self,
        issue: &Value,
        priority: Option<&str>,
        timeline: &[Value],
    ) -> Result<(Vec<Finding>, bool)> {
        let url = issue["html_url"].as_str().unwrap_or_default();
        let mut findings = Vec::new();
        let mut merged_reference = false;
        let mut verdict: Option<(String, Option<String>)> = None;
        let created = parse_time(&issue["created_at"], url)?;
        let mut active = (created, url.to_owned());
        for event in timeline {
            let link = event["html_url"].as_str().unwrap_or(url).to_owned();
            let source = &event["source"]["issue"];
            let from_pull = source.get("pull_request").is_some();
            let passive = PASSIVE_EVENTS.contains(&event["event"].as_str().unwrap_or_default());
            let by_person =
                !passive && (person(&event["user"], self.bot) || person(&event["actor"], self.bot));
            if by_person || (event["event"] == "cross-referenced" && from_pull) {
                let at = parse_time(&event["created_at"], &link)?;
                let link = source["html_url"].as_str().unwrap_or(&link).to_owned();
                if at > active.0 {
                    active = (at, link);
                }
            }
            match event["event"].as_str() {
                Some("cross-referenced") => {
                    merged_reference |= source["pull_request"]["merged_at"].is_string();
                }
                Some("commented") if by_person => {
                    if let Some((_, after)) = &mut verdict {
                        after.get_or_insert(link);
                    }
                }
                Some("commented")
                    if event["user"]["login"]
                        .as_str()
                        .is_some_and(|l| l.eq_ignore_ascii_case(self.bot)) =>
                {
                    if let Some(said) = event["body"].as_str().and_then(triage_verdict) {
                        verdict = (said == "done").then_some((link, None));
                    }
                }
                _ => {}
            }
        }
        let idle = (self.now - active.0).whole_days();
        let urgent = priority.is_some_and(|p| URGENT_PRIORITIES.contains(&p));
        if !urgent && !holds(issue) && idle >= self.stale_days as i64 {
            findings.push(Finding {
                reason: Reason::Stale,
                detail: format!(
                    "{} with no activity by a person or a pull request for {idle} days",
                    priority.unwrap_or("no priority")
                ),
                evidence: vec![active.1],
            });
        }
        if let Some((link, None)) = verdict {
            findings.push(Finding {
                reason: Reason::TriageDone,
                detail: "the bot's newest triage verdict is done".into(),
                evidence: vec![link],
            });
        }
        Ok((findings, merged_reference))
    }
}

/// Whether anything is held behind an issue: an open sub-issue, an issue it
/// blocks, or a summary not read, which could hide either.
fn holds(issue: &Value) -> bool {
    let subs = &issue["sub_issues_summary"];
    let deps = &issue["issue_dependencies_summary"];
    let open_subs = subs["total"]
        .as_u64()
        .zip(subs["completed"].as_u64())
        .map(|(total, done)| total > done);
    let blocking = deps["blocking"].as_u64().map(|n| n > 0);
    open_subs.unwrap_or(true) || blocking.unwrap_or(true)
}

/// Whether `config` and `stale_days` can be read with, before any request.
pub fn check(config: &Config, stale_days: u64) -> Result<()> {
    ensure!(
        !config.work_repositories.is_empty(),
        "prune reads the configuration's workRepositories, and it names none"
    );
    ensure!(
        config.board.is_some(),
        "prune needs the configuration's board for Priority"
    );
    ensure!(
        (1..=STALE_DAYS_LIMIT).contains(&stale_days),
        "stale days must be 1 to {STALE_DAYS_LIMIT}"
    );
    Ok(())
}

/// The candidates among the open issues of `config`'s `workRepositories` at
/// `now`, read within `limit` requests through `cache`; with the responses
/// used, the next read's cache.
pub fn read<C: Rest + Transport>(
    client: &C,
    config: &Config,
    cache: Cache,
    (limit, stale_days): (usize, u64),
    now: OffsetDateTime,
) -> Result<(Report, Cache)> {
    check(config, stale_days)?;
    let mut reader = Reader {
        client,
        previous: cache,
        used: Cache::default(),
        limit,
        requests: 0,
        not_modified: 0,
    };
    let priorities = reader.priorities(config)?;
    let judge = Judge {
        bot: &config.bot,
        now,
        stale_days,
    };
    let repositories: BTreeSet<String> = config.work_repositories.iter().cloned().collect();
    let scope: BTreeSet<String> = (repositories.iter())
        .map(|r| r.to_ascii_lowercase())
        .collect();
    let (mut open, mut kept) = (0, 0);
    let mut candidates = Vec::new();
    for repository in &repositories {
        let path =
            format!("repos/{repository}/issues?state=open&sort=created&direction=asc&per_page=100");
        let mut issues: Vec<Value> = reader
            .list(&path, slim_issue)?
            .into_iter()
            .filter(|i| i["pull"] != true)
            .collect();
        open += issues.len();
        issues.sort_by_key(|i| i["number"].as_u64());
        let mut oldest: BTreeMap<String, (OffsetDateTime, &Value)> = BTreeMap::new();
        for issue in &issues {
            let created = parse_time(&issue["created_at"], &format!("{repository} issue"))?;
            let stem = title_stem(issue["title"].as_str().unwrap_or_default());
            // Titles of only punctuation or emoji share no words.
            if stem.is_empty() {
                continue;
            }
            let entry = oldest.entry(stem).or_insert((created, issue));
            if (created, issue["number"].as_u64()) < (entry.0, entry.1["number"].as_u64()) {
                *entry = (created, issue);
            }
        }
        for issue in &issues {
            let number = issue["number"]
                .as_u64()
                .with_context(|| format!("an open issue of {repository} has no number"))?;
            let (Some(url), Some(title)) = (issue["html_url"].as_str(), issue["title"].as_str())
            else {
                bail!("{repository}#{number} has no URL or title");
            };
            let labels = issue["labels"].as_array().into_iter().flatten();
            if labels
                .filter_map(Value::as_str)
                .any(|l| l.eq_ignore_ascii_case(KEEP_LABEL))
            {
                kept += 1;
                continue;
            }
            let priority = priorities.get(&key(repository, number)).cloned().flatten();
            let timeline = reader.list(
                &format!("repos/{repository}/issues/{number}/timeline?per_page=100"),
                slim_event,
            )?;
            let (mut reasons, merged_reference) =
                judge.judge(issue, priority.as_deref(), &timeline)?;
            if merged_reference {
                let closing = reader.closing(repository, number, &scope)?;
                if !closing.is_empty() {
                    reasons.push(Finding {
                        reason: Reason::Overtaken,
                        detail: "closed by a merged pull request of a work repository".into(),
                        evidence: closing,
                    });
                }
            }
            if let Some((_, original)) = oldest.get(&title_stem(title))
                && original["number"].as_u64() != Some(number)
            {
                reasons.push(Finding {
                    reason: Reason::Duplicate,
                    detail: format!(
                        "same title as the older #{}",
                        original["number"].as_u64().unwrap_or_default()
                    ),
                    evidence: vec![original["html_url"].as_str().unwrap_or_default().into()],
                });
            }
            if reasons.is_empty() {
                continue;
            }
            reasons.sort_by_key(|f| f.reason);
            candidates.push(Candidate {
                repository: repository.clone(),
                number,
                url: url.into(),
                title: title.into(),
                priority,
                reasons,
            });
        }
    }
    let report = Report {
        now: now.format(&Rfc3339)?,
        stale_days,
        repositories: repositories.into_iter().collect(),
        open,
        kept,
        requests: reader.requests,
        not_modified: reader.not_modified,
        candidates,
    };
    Ok((report, reader.used))
}

#[cfg(test)]
mod tests;
