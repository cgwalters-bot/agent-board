//! Bounded intake facts; standalone serialization exposes metadata only.
use super::{Config as BoardConfig, Transport, listing, validate_repository};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use url::Url;

pub const REPOSITORY_LIMIT: usize = 100;
pub const PAGE_CAP: usize = 10;
pub const INITIAL_SINCE: &str = "1970-01-01T00:00:00Z";

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub repositories: Vec<String>,
    pub operator_id: u64,
}

impl Config {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(self.operator_id > 0, "intake operatorId must be positive");
        ensure!(
            !self.repositories.is_empty() && self.repositories.len() <= REPOSITORY_LIMIT,
            "intake requires 1..={REPOSITORY_LIMIT} repository selectors"
        );
        for repository in &self.repositories {
            if let Some(owner) = repository.strip_suffix("/*") {
                ensure!(super::is_login(owner), "invalid intake owner");
            } else {
                validate_repository(repository)?;
            }
        }
        Ok(())
    }
}

/// One since cursor per repository. Keep a separate file for each instance.
pub type State = BTreeMap<String, String>;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub repository: String,
    pub kind: &'static str,
    pub number: u64,
    pub url: String,
    /// Signed: Forgejo's Ghost (deleted) and Actions users have negative ids.
    pub author_id: i64,
    pub author_login: String,
    pub is_operator: bool,
    pub generated_by: bool,
    #[serde(skip)]
    pub source: Source,
}

/// Controller-only source facts. Never included in standalone intake output.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub id: u64,
    pub author: i64,
    pub created: String,
    pub updated: Option<String>,
    pub body: String,
    pub pull_request: bool,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub repository: String,
    pub items: Vec<Item>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn timestamp(value: &str) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339).context("intake timestamp must be RFC3339")
}

/// Reads an entire listing or refuses it. Empty-page termination supports
/// servers without totals and servers with a smaller maximum page size.
fn complete(transport: &impl Transport, mut url: Url) -> Result<Vec<serde_json::Value>> {
    let base: Vec<_> = url.query_pairs().into_owned().collect();
    let mut all = Vec::new();
    let mut expected = None;
    for page in 1..=PAGE_CAP {
        let (items, total) = listing(transport, super::page_url(&mut url, &base, page))?;
        if page == 1 {
            expected = total;
        }
        ensure!(expected == total, "intake listing total changed");
        let empty = items.is_empty();
        all.extend(items);
        if empty || total == Some(all.len() as u64) {
            ensure!(
                total.is_none_or(|n| n == all.len() as u64),
                "intake listing incomplete"
            );
            return Ok(all);
        }
    }
    anyhow::bail!("intake listing truncated at {PAGE_CAP} pages; cursor unchanged")
}

/// A final attribution line must be prose, not an example in a fenced or
/// indented code block, or in a blockquote. Quote state is only tracked
/// outside fences; a blank line, an ATX heading or an opening fence ends a
/// quote, as they cannot be lazy continuation lines. This is only metadata,
/// never authorization to act.
fn generated_by(body: &str, footer: &crate::Footer) -> bool {
    let mut fence = None;
    let mut quote = false;
    let mut matched = false;
    for line in body.trim_end().lines() {
        let trimmed = line.trim_start();
        if fence.is_none() {
            let hashes = trimmed.len() - trimmed.trim_start_matches('#').len();
            let heading = (1..=6).contains(&hashes)
                && trimmed[hashes..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace);
            if trimmed.is_empty() || heading {
                quote = false;
            } else if trimmed.starts_with('>') {
                quote = true;
            }
        }
        matched = fence.is_none() && !quote && line == footer.as_str();
        if let Some((mark, count)) = crate::plan::code_fence(line) {
            match fence {
                None => {
                    fence = Some((mark, count));
                    quote = false;
                }
                Some((open, length))
                    if mark == open
                        && count >= length
                        && line.trim().chars().all(|c| c == mark) =>
                {
                    fence = None
                }
                _ => {}
            }
        }
    }
    matched
}

fn error_line(error: &anyhow::Error) -> String {
    if error.downcast_ref::<super::NotFound>().is_some() {
        "404: Forgejo repository not found".into()
    } else if error
        .downcast_ref::<super::AuthenticationRequired>()
        .is_some()
    {
        "401/403: Forgejo refused repository access".into()
    } else {
        format!("{error:#}")
    }
}

/// Response links are data: inspect them locally, never follow them. Forgejo
/// repository lookup is case insensitive, while HTML links preserve casing.
fn target_number(board: &BoardConfig, repository: &str, url: &Url) -> Result<u64> {
    let base = board.base();
    ensure!(
        url.origin() == base.origin()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none(),
        "item URL outside intake instance"
    );
    let path = url
        .path()
        .strip_prefix(base.path())
        .context("item URL outside intake base path")?;
    let parts: Vec<_> = path.split('/').collect();
    ensure!(
        parts.len() == 4
            && format!("{}/{}", parts[0], parts[1]).eq_ignore_ascii_case(repository)
            && matches!(parts[2], "issues" | "pulls"),
        "item URL outside intake repository"
    );
    let number = parts[3]
        .parse::<u64>()
        .context("invalid intake issue number")?;
    ensure!(number > 0, "invalid intake issue number");
    Ok(number)
}

fn read_repository(
    board: &BoardConfig,
    config: &Config,
    transport: &impl Transport,
    repository: &str,
    since: OffsetDateTime,
    until: OffsetDateTime,
    footer: &crate::Footer,
) -> Result<Vec<Item>> {
    let api = board.api_root()?;
    // Forgejo stores whole-second timestamps. Overlap the cursor second even
    // if the API's since comparison is strict; local filtering is inclusive.
    let query_since = since
        .checked_sub(time::Duration::seconds(1))
        .context("intake since cannot be overlapped")?
        .format(&Rfc3339)?;
    let mut items = Vec::new();
    for (kind, endpoint) in [("issue", "issues"), ("comment", "issues/comments")] {
        let mut url = api.join(&format!("repos/{repository}/{endpoint}"))?;
        url.query_pairs_mut().append_pair("since", &query_since);
        if kind == "issue" {
            url.query_pairs_mut()
                .append_pair("type", "issues")
                .append_pair("state", "all");
        }
        let mut ids = BTreeSet::new();
        for value in complete(transport, url)? {
            let id = value["id"].as_u64().context("missing intake item id")?;
            ensure!(id > 0, "invalid intake item id");
            ensure!(ids.insert(id), "duplicate intake item; listing shifted");
            if kind == "issue" && !value["pull_request"].is_null() {
                continue;
            }
            let created = timestamp(super::text(&value, "created_at")?)?;
            if created < since || created > until {
                continue;
            }
            let number = if kind == "issue" {
                value["number"]
                    .as_u64()
                    .context("missing intake issue number")?
            } else {
                // Forgejo's Comment has HTML issue_url or pull_request_url,
                // not an API link. A PR comment's issue_url is empty.
                let target = ["issue_url", "pull_request_url"]
                    .iter()
                    .filter_map(|key| value[key].as_str().filter(|s| !s.is_empty()))
                    .collect::<Vec<_>>();
                ensure!(
                    target.len() == 1,
                    "comment must name one issue or pull request"
                );
                target_number(board, repository, &Url::parse(target[0])?)?
            };
            ensure!(number > 0, "invalid intake issue number");
            let url = Url::parse(super::text(&value, "html_url")?)?;
            ensure!(
                target_number(board, repository, &url)? == number,
                "item URL number does not match"
            );
            let author_id = value["user"]["id"]
                .as_i64()
                .context("missing intake author id")?;
            let login = super::text(&value["user"], "login")?;
            ensure!(super::is_login(login), "invalid intake author login");
            items.push(Item {
                repository: repository.into(),
                kind,
                number,
                url: url.into(),
                author_id,
                author_login: login.into(),
                is_operator: u64::try_from(author_id).is_ok_and(|id| id == config.operator_id),
                generated_by: generated_by(super::text(&value, "body")?, footer),
                source: Source {
                    id,
                    author: author_id,
                    created: super::text(&value, "created_at")?.into(),
                    updated: value["updated_at"].as_str().map(str::to_owned),
                    body: super::text(&value, "body")?.into(),
                    pull_request: !value["pull_request"].is_null()
                        || value["pull_request_url"]
                            .as_str()
                            .is_some_and(|s| !s.is_empty()),
                },
            });
        }
    }
    Ok(items)
}

/// Only successful complete repository reads advance state, to the invocation's
/// start time rather than its finish (items arriving during paging are retried).
pub fn read(
    board: &BoardConfig,
    transport: &impl Transport,
    state: &mut State,
    since: Option<&str>,
    clock: &str,
    footer: &crate::Footer,
) -> Result<Vec<Report>> {
    board.validate()?;
    let config = board
        .intake
        .as_ref()
        .context("board.intake is not configured")?;
    let until = timestamp(clock)?;
    if let Some(since) = since {
        ensure!(timestamp(since)? <= until, "intake since is in the future");
    }
    for cursor in state.values() {
        ensure!(
            timestamp(cursor)? <= until,
            "intake state cursor is in the future"
        );
    }
    let mut repositories = BTreeMap::new();
    let mut reports = Vec::new();
    for selector in &config.repositories {
        let expanded = (|| -> Result<BTreeMap<String, String>> {
            let mut expanded = BTreeMap::new();
            if let Some(owner) = selector.strip_suffix("/*") {
                // The user endpoint includes organization owners on Forgejo.
                let url = board.api_root()?.join(&format!("users/{owner}/repos"))?;
                for value in complete(transport, url)? {
                    let name = super::text(&value, "full_name")?;
                    validate_repository(name)?;
                    if name
                        .split_once('/')
                        .is_some_and(|(o, _)| o.eq_ignore_ascii_case(owner))
                    {
                        expanded.insert(name.to_ascii_lowercase(), name.into());
                    }
                }
            } else {
                expanded.insert(selector.to_ascii_lowercase(), selector.clone());
            }
            ensure!(
                repositories
                    .keys()
                    .chain(expanded.keys())
                    .collect::<BTreeSet<_>>()
                    .len()
                    <= REPOSITORY_LIMIT,
                "intake expansion exceeds {REPOSITORY_LIMIT} repositories"
            );
            Ok(expanded)
        })();
        match expanded {
            Ok(expanded) => repositories.extend(expanded),
            Err(error) => reports.push(Report {
                repository: selector.clone(),
                items: vec![],
                error: Some(error_line(&error)),
            }),
        }
    }
    for (key, repository) in repositories {
        let cursor = since
            .or_else(|| state.get(&key).map(String::as_str))
            .unwrap_or(INITIAL_SINCE);
        match read_repository(
            board,
            config,
            transport,
            &repository,
            timestamp(cursor)?,
            until,
            footer,
        ) {
            Ok(items) => {
                state.insert(key, clock.into());
                reports.push(Report {
                    repository,
                    items,
                    error: None,
                });
            }
            Err(error) => reports.push(Report {
                repository,
                items: vec![],
                error: Some(error_line(&error)),
            }),
        }
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::cell::RefCell;

    struct Fixture {
        responses: Vec<Value>,
        requests: RefCell<Vec<Url>>,
    }

    impl Transport for Fixture {
        fn get(&self, url: &Url) -> Result<super::super::Response> {
            self.requests.borrow_mut().push(url.clone());
            let path = url.path().strip_prefix("/api/v1/").unwrap();
            let page = url
                .query_pairs()
                .find(|(key, _)| key == "page")
                .unwrap()
                .1
                .parse::<u64>()
                .unwrap();
            let response = self
                .responses
                .iter()
                .find(|v| {
                    v["path"]
                        .as_str()
                        .is_some_and(|p| p.eq_ignore_ascii_case(path))
                        && (v["page"] == page || v["repeat"] == true)
                })
                .with_context(|| format!("unrecorded GET {url}"))?;
            match response["error"].as_u64() {
                Some(404) => return Err(super::super::NotFound.into()),
                Some(401) => {
                    return Err(super::super::AuthenticationRequired("TEST_TOKEN".into()).into());
                }
                _ => {}
            }
            Ok(super::super::Response {
                body: response["body"].clone(),
                total: response["total"].as_u64(),
            })
        }

        fn send(&self, method: &str, _: &Url, _: &Value) -> Result<(u16, Value)> {
            panic!("intake must never issue {method}")
        }

        fn write(&self, method: &str, _: &Url, _: &Value) -> Result<Value> {
            panic!("intake must never issue {method}")
        }
    }

    #[test]
    fn fixture_table() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../../fixtures/forgejo/intake.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let board: BoardConfig = serde_json::from_value(json!({
                "url":"https://forge.example/", "repositories":["board/only"], "membership":"open",
                "intake":{"repositories":case["repositories"], "operatorId":42}
            }))
            .unwrap();
            let mut transport = Fixture {
                responses: fixture["responses"].as_array().unwrap().clone(),
                requests: RefCell::new(vec![]),
            };
            let since = fixture["since"].as_str().unwrap();
            let query_since = (timestamp(since).unwrap() - time::Duration::seconds(1))
                .format(&Rfc3339)
                .unwrap();
            let clock = fixture["clock"].as_str().unwrap();
            let mut state = State::from([
                ("example/work".into(), since.into()),
                ("example/missing".into(), since.into()),
                ("example/denied".into(), since.into()),
                ("example/long".into(), since.into()),
                ("outside/ignored".into(), since.into()),
            ]);
            let original = state.clone();
            let reports = read(
                &board,
                &transport,
                &mut state,
                None,
                clock,
                &crate::Footer::default(),
            )
            .unwrap();
            let success: Vec<_> = reports
                .iter()
                .filter(|r| r.error.is_none())
                .map(|r| r.repository.as_str())
                .collect();
            let errors: Vec<_> = reports
                .iter()
                .filter(|r| r.error.is_some())
                .map(|r| r.repository.as_str())
                .collect();
            assert_eq!(json!(success), case["success"], "{}", case["name"]);
            assert_eq!(json!(errors), case["errors"], "{}", case["name"]);
            for (repository, cursor) in &state {
                assert_eq!(
                    cursor,
                    if success.iter().any(|r| r.eq_ignore_ascii_case(repository)) {
                        clock
                    } else {
                        &original[repository]
                    }
                );
            }
            let items = &reports
                .iter()
                .find(|r| r.repository.eq_ignore_ascii_case("example/work"))
                .unwrap()
                .items;
            assert_eq!(items.len(), 4);
            assert_eq!(
                items.iter().map(|i| i.source.id).collect::<Vec<_>>(),
                [10, 20, 21, 22]
            );
            assert_eq!(
                items[1].source.body,
                "Agent response\n\nGenerated-by: https://github.com/cgwalters/#llms"
            );
            assert_eq!(items[1].source.created, "2026-10-30T02:00:00Z");
            assert!(items[2].source.pull_request);
            assert_eq!(items[0].repository, success[0]);
            assert_eq!(
                items
                    .iter()
                    .map(|i| (i.is_operator, i.generated_by))
                    .collect::<Vec<_>>(),
                [(true, false), (true, true), (false, false), (false, false)]
            );
            let output = serde_json::to_string(&reports).unwrap();
            assert!(!output.contains("Agent response"));
            assert!(!output.contains("Quoted example"));
            let requests = transport.requests.borrow();
            assert!(
                !requests.iter().any(
                    |u| u.path().contains("outside/ignored") || u.path().contains("board/only")
                )
            );
            for request in requests.iter().filter(|u| u.path().contains("/issues")) {
                assert!(
                    request
                        .query_pairs()
                        .any(|(k, v)| k == "since" && v == query_since)
                );
                if request.path().ends_with("/issues") {
                    assert!(
                        request
                            .query_pairs()
                            .any(|(k, v)| k == "type" && v == "issues")
                    );
                }
            }
            assert!(requests.len() <= PAGE_CAP + 5);
            drop(requests);
            // A second complete read uses the advanced cursor and emits no old items.
            if case["name"] == "paging and cursor advance" {
                transport.requests.borrow_mut().clear();
                let reports = read(
                    &board,
                    &transport,
                    &mut state,
                    None,
                    clock,
                    &crate::Footer::default(),
                )
                .unwrap();
                assert!(
                    reports
                        .iter()
                        .all(|r| r.items.is_empty() && r.error.is_none())
                );
                assert!(
                    transport
                        .requests
                        .borrow()
                        .iter()
                        .all(|u| u.query_pairs().any(|(k, v)| k == "since"
                            && v == (timestamp(clock).unwrap() - time::Duration::seconds(1))
                                .format(&Rfc3339)
                                .unwrap()))
                );
                // An issue appears after the prior issues endpoint was read,
                // but its server timestamp is in the saved cursor's second.
                let mut late = transport.responses[1]["body"][0].clone();
                late["id"] = json!(12);
                late["number"] = json!(3);
                late["html_url"] = json!("https://forge.example/example/work/issues/3");
                late["created_at"] = json!(clock);
                transport.responses[1]["body"]
                    .as_array_mut()
                    .unwrap()
                    .push(late);
                transport.responses[1]["total"] = json!(3);
                transport.responses[2]["total"] = json!(3);
                let reports = read(
                    &board,
                    &transport,
                    &mut state,
                    None,
                    "2026-10-30T12:01:00Z",
                    &crate::Footer::default(),
                )
                .unwrap();
                assert!(reports[0].error.is_none());
                assert_eq!(reports[0].items.len(), 1);
                assert_eq!(reports[0].items[0].number, 3);
            }
        }
    }

    #[test]
    fn configuration_table() {
        for (selectors, id, valid) in [
            (json!(["owner/work"]), 42, true),
            (json!(["owner/*"]), 42, true),
            (json!(["owner/work"]), 0, false),
            (json!([]), 42, false),
            (json!(["*/work"]), 42, false),
            (json!(["owner/../work"]), 42, false),
            (json!(["owner/work/*"]), 42, false),
            (json!(vec!["owner/work"; REPOSITORY_LIMIT + 1]), 42, false),
        ] {
            let config: Config =
                serde_json::from_value(json!({"repositories":selectors,"operatorId":id})).unwrap();
            assert_eq!(config.validate().is_ok(), valid, "{selectors}");
        }
    }

    #[test]
    fn trailer_table() {
        let footer = crate::Footer::default();
        for (prefix, suffix, expected) in [
            ("prose\n\n", "\n", true),
            ("```\n", "", false),
            ("~~~text\n", "", false),
            ("```\n", "\n```", false),
            ("    ", "", false),
            ("> ", "", false),
            ("> quoted example\n", "", false),
            ("> quoted example\n\n", "", true),
            ("```\nexample\n```\n", "", true),
            ("```\n> quoted example\n```\n", "", true),
            ("> quoted example\n# Heading\n", "", true),
            ("> quoted example\n#lazy\n", "", false),
            ("> quoted example\n```\nexample\n```\n", "", true),
            ("", "\nmore prose", false),
        ] {
            let body = format!("{prefix}{footer}{suffix}");
            assert_eq!(generated_by(&body, &footer), expected, "{body}");
        }
        let custom = crate::Footer::from_json(Some(&json!("Generated-by: custom"))).unwrap();
        assert!(generated_by("reply\n\nGenerated-by: custom", &custom));
        assert!(!generated_by(crate::FOOTER, &custom));
    }
}
