//! GitHub Projects v2 read transport. Connections are exhausted, never inferred complete.
use super::Forge;
use anyhow::{Context, Result, ensure};
use board_core::{Capability, Coverage, Evidence, Item, ItemRef, Snapshot};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

pub const DEFAULT_HOST: &str = "github.com";
/// The only origin a REST request carries the token to.
pub const API_ROOT: &str = "https://api.github.com/";

/// Interpret GitHub syntax only for the explicitly selected GitHub host.
pub fn parse_github_item(input: &str, repository: Option<&str>, host: &str) -> Result<ItemRef> {
    Ok(parse_github_identity(input, repository, host)?.0)
}

/// Parse an issue identity without losing an explicit pull request URL's kind.
pub fn parse_github_issue(input: &str, repository: Option<&str>, host: &str) -> Result<ItemRef> {
    let (item, pull_request) = parse_github_identity(input, repository, host)?;
    ensure!(
        !pull_request,
        "only issues can be added or edited, not pull requests"
    );
    Ok(item)
}

fn parse_github_identity(
    input: &str,
    repository: Option<&str>,
    host: &str,
) -> Result<(ItemRef, bool)> {
    let (repository, number, pull_request) = if input.contains("://") {
        let url = Url::parse(input).context("parse GitHub issue URL")?;
        ensure!(
            url.scheme() == "https" && url.host_str() == Some(host),
            "issue URL must use the selected GitHub host and HTTPS"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "issue URL must be canonical"
        );
        let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
        ensure!(
            parts.len() == 4 && ["issues", "pull"].contains(&parts[2]),
            "expected a GitHub issue or pull request URL"
        );
        (
            format!("{}/{}", parts[0], parts[1]),
            parts[3].parse().context("parse issue number")?,
            parts[2] == "pull",
        )
    } else {
        let (repo, number) = match input.rsplit_once('#') {
            Some(parts) => parts,
            None => (repository.context("bare numbers require --repo")?, input),
        };
        (
            repo.into(),
            number.parse().context("parse issue number")?,
            false,
        )
    };
    let item = ItemRef {
        host: host.into(),
        repository,
        number,
    };
    item.validate()?;
    ensure!(
        item.repository.split('/').count() == 2,
        "GitHub repositories require OWNER/REPO"
    );
    Ok((item, pull_request))
}

/// GitHub login syntax: 1-39 ASCII alphanumerics or hyphens, not starting with a hyphen,
/// plus the `_shortcode` suffix of enterprise managed users. Shell-inert, so a validated
/// login can be printed into a command to copy.
pub fn is_github_login(login: &str) -> bool {
    (1..=39).contains(&login.len())
        && !login.starts_with('-')
        && login
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

pub fn validate_github_project(project: &Url) -> Result<()> {
    ensure!(
        project.scheme() == "https"
            && project.host_str() == Some(DEFAULT_HOST)
            && project.port().is_none()
            && project.username().is_empty()
            && project.password().is_none()
            && project.query().is_none()
            && project.fragment().is_none(),
        "unsupported_capability: noncanonical GitHub project"
    );
    let parts: Vec<_> = project.path().trim_matches('/').split('/').collect();
    ensure!(
        parts.len() == 4
            && ["orgs", "users"].contains(&parts[0])
            && is_github_login(parts[1])
            && parts[2] == "projects"
            && parts[3].parse::<u64>().is_ok_and(|n| n > 0),
        "invalid GitHub project URL"
    );
    Ok(())
}

pub fn github_item_url(item: &ItemRef) -> Result<String> {
    item.validate()?;
    ensure!(
        item.repository.split('/').count() == 2,
        "invalid GitHub repository"
    );
    Ok(format!(
        "https://{}/{}/issues/{}",
        item.host, item.repository, item.number
    ))
}

#[derive(Debug)]
pub struct AuthenticationRequired;

impl std::fmt::Display for AuthenticationRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "authentication required: provide a valid GH_TOKEN or GITHUB_TOKEN (classic token: read:project, or project for writes such as project init; fine-grained: the owner's Projects permission)"
        )
    }
}

impl std::error::Error for AuthenticationRequired {}

/// A GET answered 404 or 410: what it names is not there, or was deleted.
#[derive(Debug)]
pub struct NotFound;

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not found")
    }
}

impl std::error::Error for NotFound {}

pub trait Transport {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value>;
}

/// Attach the operation and its fine-grained permission to live failures.
pub struct DiagnosticTransport<'a, T> {
    pub transport: &'a T,
    pub project: String,
    pub repository: String,
}

fn operation(query: &str) -> (&str, &str, bool) {
    for (marker, step, permission, repository) in [
        (
            "issue(number:",
            "reading issue",
            "repository Issues read",
            true,
        ),
        (
            "createIssue(",
            "opening issue in",
            "repository Issues write",
            true,
        ),
        (
            "closeIssue(",
            "closing issue in",
            "repository Issues write",
            true,
        ),
        (
            "addComment(",
            "commenting on issue in",
            "repository Issues write",
            true,
        ),
        (
            "repository(owner:",
            "reading repository",
            "repository Metadata read",
            true,
        ),
        (
            "createProjectV2(",
            "creating project",
            "organization Projects write",
            false,
        ),
        (
            "deleteProjectV2(",
            "deleting project",
            "organization Projects write",
            false,
        ),
        (
            "addProjectV2ItemById(",
            "adding the issue to project",
            "organization Projects write",
            false,
        ),
        (
            "deleteProjectV2Item(",
            "removing the issue from project",
            "organization Projects write",
            false,
        ),
        (
            "createProjectV2Field(",
            "creating field in project",
            "organization Projects write (for a user-owned project, a classic token's project scope)",
            false,
        ),
        (
            "updateProjectV2ItemFieldValue(",
            "setting field in project",
            "organization Projects write",
            false,
        ),
        (
            "fieldValues(first",
            "reading item field values in project",
            "organization Projects read",
            false,
        ),
        (
            "fields(first",
            "reading field schema in project",
            "organization Projects read",
            false,
        ),
        (
            "items(first",
            "reading membership in project",
            "organization Projects read",
            false,
        ),
        (
            "labels(first",
            "reading labels in project",
            "repository Issues read",
            false,
        ),
        (
            "assignees(first",
            "reading assignees in project",
            "repository Issues read",
            false,
        ),
    ] {
        if query.contains(marker) {
            return (step, permission, repository);
        }
    }
    ("reading project", "organization Projects read", false)
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn step_permissions() {
        for (query, expected) in [
            ("projectV2(number", "organization Projects read"),
            ("fields(first", "organization Projects read"),
            ("items(first", "organization Projects read"),
            ("fieldValues(first", "organization Projects read"),
            ("createProjectV2(", "organization Projects write"),
            ("deleteProjectV2(", "organization Projects write"),
            ("addProjectV2ItemById(", "organization Projects write"),
            ("deleteProjectV2Item(", "organization Projects write"),
            (
                "createProjectV2Field(",
                "organization Projects write (for a user-owned project, a classic token's project scope)",
            ),
            (
                "updateProjectV2ItemFieldValue(",
                "organization Projects write",
            ),
            ("issue(number:", "repository Issues read"),
            ("createIssue(", "repository Issues write"),
            ("closeIssue(", "repository Issues write"),
            ("addComment(", "repository Issues write"),
            ("repository(owner:", "repository Metadata read"),
            ("labels(first", "repository Issues read"),
            ("assignees(first", "repository Issues read"),
        ] {
            assert_eq!(operation(query).1, expected, "{query}");
            for refusal in [
                "FORBIDDEN",
                "GitHub HTTP status 403",
                "GitHub HTTP status 404",
                "network failure",
            ] {
                struct Refused<'a>(&'a str);
                impl Transport for Refused<'_> {
                    fn graphql(&self, _: &str, _: Value) -> Result<Value> {
                        anyhow::bail!("{}", self.0)
                    }
                }
                let transport = DiagnosticTransport {
                    transport: &Refused(refusal),
                    project: "org/2".into(),
                    repository: "org/repo".into(),
                };
                let error = format!("{:#}", transport.graphql(query, json!({})).unwrap_err());
                assert!(error.contains(if operation(query).2 {
                    "org/repo"
                } else {
                    "org/2"
                }));
                assert_eq!(error.contains(expected), refusal != "network failure");
                assert_eq!(
                    error.contains("pending approval"),
                    refusal != "network failure"
                );
            }
        }
    }
}

impl<T: Transport> Transport for DiagnosticTransport<'_, T> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let (step, permission, repository) = operation(query);
        let target = if repository {
            &self.repository
        } else {
            &self.project
        };
        let detail = if query.contains("updateProjectV2ItemFieldValue(") {
            if let Some(name) = variables["name"].as_str() {
                &format!("setting {name} in project")
            } else if variables["value"].get("singleSelectOptionId").is_some() {
                "setting Status in project"
            } else {
                "setting Fixture nonce in project"
            }
        } else {
            step
        };
        let context = format!(
            "{detail} {target} (node {})",
            variables["id"].as_str().unwrap_or("project/repository")
        );
        self.transport.graphql(query, variables).map_err(|error| {
            let message = format!("{error:#}");
            let context = if ["FORBIDDEN", "HTTP status 403", "HTTP status 404"].iter().any(|s| message.contains(s)) {
                format!("{context}\nFine-grained token needs {permission}; an organization can also hold a fine-grained token pending approval.")
            } else {
                context
            };
            error.context(context)
        })
    }
}

// Shared across every connection and per-item read in one snapshot. Exhaustion
// returns an error, never a partial snapshot that could yield proposals.
const SNAPSHOT_REQUEST_LIMIT: usize = 1_000;

struct ReadBudget<'a, T> {
    transport: &'a T,
    remaining: std::cell::Cell<usize>,
}

impl<T: Transport> Transport for ReadBudget<'_, T> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let remaining = self.remaining.get();
        ensure!(
            remaining > 0,
            "GitHub snapshot request budget exhausted; no snapshot produced"
        );
        self.remaining.set(remaining - 1);
        self.transport.graphql(query, variables)
    }
}

pub struct Client {
    token: String,
    agent: ureq::Agent,
}

impl Client {
    pub fn from_env() -> Result<Self> {
        let token = select_token(
            std::env::var("GH_TOKEN").ok(),
            std::env::var("GITHUB_TOKEN").ok(),
        )?;
        Ok(Self::with_token(token))
    }

    /// The client for `name` when that variable holds a token.
    pub fn from_var(name: &str) -> Option<Self> {
        let token = std::env::var(name).ok().filter(|t| !t.is_empty())?;
        Some(Self::with_token(token))
    }

    /// A REST client that sends no token, for endpoints that need none.
    pub fn anonymous() -> Self {
        Self::with_token(String::new())
    }

    fn authorize<B>(&self, request: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        if self.token.is_empty() {
            return request;
        }
        request.header("Authorization", &format!("Bearer {}", self.token))
    }

    /// The body of a GET that the API answers with a redirect off itself, as
    /// it does for job logs, at most `limit` bytes. The redirect is followed
    /// by hand, without the token, so the token goes only to the API.
    pub fn download(&self, url: &str, limit: u64) -> Result<Vec<u8>> {
        ensure!(
            url.starts_with(API_ROOT),
            "refusing to send the token outside {API_ROOT}"
        );
        let path = url.trim_start_matches(API_ROOT);
        let mut response = self
            .agent
            .get(url)
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("User-Agent", "agent-board")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .config()
            .max_redirects(0)
            .build()
            .call()
            .map_err(transport_error)?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            let location = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .filter(|l| l.starts_with("https://"))
                .with_context(|| format!("GET {path}: redirect without an HTTPS location"))?
                .to_owned();
            response = self
                .agent
                .get(&location)
                .header("User-Agent", "agent-board")
                .call()
                .map_err(transport_error)?;
        }
        let status = response.status().as_u16();
        ensure!(
            (200..300).contains(&status),
            "GET {path}: GitHub HTTP status {status}"
        );
        response
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .with_context(|| format!("GET {path}: body unreadable or over {limit} bytes"))
    }

    fn with_token(token: String) -> Self {
        Self {
            token,
            agent: http_agent(),
        }
    }
}

/// The HTTPS agent every live forge client uses.
pub(crate) fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                // Honor locally installed CAs, including sandbox egress proxies.
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into()
}

fn select_token(gh: Option<String>, github: Option<String>) -> Result<String> {
    gh.filter(|s| !s.is_empty())
        .or_else(|| github.filter(|s| !s.is_empty()))
        .ok_or_else(|| AuthenticationRequired.into())
}

fn transport_error(error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::StatusCode(401) => AuthenticationRequired.into(),
        ureq::Error::StatusCode(403) => anyhow::anyhow!(
            "GitHub denied access: check Organization Projects read/write permission, repository Issues read/write permission, organization approval and rate limits"
        ),
        ureq::Error::StatusCode(code) => anyhow::anyhow!("GitHub HTTP status {code}"),
        _ => anyhow::anyhow!("GitHub transport failed"),
    }
}

fn response_data(response: Value) -> Result<Value> {
    let details = response["errors"]
        .as_array()
        .map(|errors| {
            errors
                .iter()
                .map(|error| {
                    format!(
                        "{}: {}",
                        error["type"].as_str().unwrap_or("UNKNOWN"),
                        error["message"].as_str().unwrap_or("message unavailable")
                    )
                })
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default();
    if response["errors"].as_array().is_some_and(|errors| {
        errors
            .iter()
            .any(|e| e["type"] == "UNAUTHORIZED" || e["extensions"]["code"] == "UNAUTHENTICATED")
    }) {
        return Err(anyhow::Error::from(AuthenticationRequired)
            .context(format!("GitHub GraphQL: {details}")));
    }
    ensure!(
        response
            .get("errors")
            .is_none_or(|e| e.is_null() || e.as_array().is_some_and(Vec::is_empty)),
        "GitHub GraphQL: {details}; response may be partial"
    );
    response
        .get("data")
        .filter(|v| v.is_object())
        .cloned()
        .context("missing GraphQL data")
}

impl Transport for Client {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let mut response = self
            .agent
            .post("https://api.github.com/graphql")
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("User-Agent", "agent-board")
            .send_json(json!({"query":query,"variables":variables}))
            .map_err(transport_error)?;
        let status = response.status().as_u16();
        let body: Value = match response.body_mut().read_json() {
            Ok(body) => body,
            Err(_) if !(200..300).contains(&status) => json!({}),
            Err(error) => return Err(error).context("decode GitHub GraphQL response"),
        };
        http_response_data(status, body)
    }
}

impl crate::poll::Rest for Client {
    fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        ensure!(
            url.starts_with(API_ROOT),
            "refusing to send the token outside {API_ROOT}"
        );
        let path = url.trim_start_matches(API_ROOT);
        let request = |request: ureq::RequestBuilder<ureq::typestate::WithBody>| {
            let request = self
                .authorize(request)
                .header("User-Agent", "agent-board")
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28");
            // A DELETE (of a label) or a GET is sent with `null`, which
            // means no body.
            match body {
                Value::Null => request.send_empty(),
                body => request.send_json(body),
            }
        };
        let mut response = match method {
            // Unconditional, with its status for the caller to judge.
            "GET" => request(self.agent.get(url).force_send_body()),
            "POST" => request(self.agent.post(url)),
            "PATCH" => request(self.agent.patch(url)),
            "PUT" => request(self.agent.put(url)),
            "DELETE" => request(self.agent.delete(url).force_send_body()),
            _ => anyhow::bail!("unsupported method {method}"),
        }
        .map_err(transport_error)?;
        let status = response.status().as_u16();
        if status == 401 {
            return Err(anyhow::Error::from(AuthenticationRequired)
                .context(format!("{method} {path}: GitHub HTTP status 401")));
        }
        // 204 (a dispatch without run details) has no body.
        let text = response
            .body_mut()
            .read_to_string()
            .with_context(|| format!("read {method} {path}"))?;
        let body = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        Ok((status, body))
    }

    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<crate::poll::RestResponse> {
        // A `Link` header or a cached URL must not carry the token elsewhere.
        ensure!(
            url.starts_with(API_ROOT),
            "refusing to send the token outside {API_ROOT}"
        );
        let mut request = self
            .authorize(self.agent.get(url))
            .header("User-Agent", "agent-board")
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some((name, value)) = validator {
            request = request.header(name, value);
        }
        let mut response = request.call().map_err(transport_error)?;
        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let (etag, last_modified) = (header("etag"), header("last-modified"));
        let link = header("link");
        let path = url.trim_start_matches(API_ROOT);
        let body = match status {
            304 => Value::Null,
            200..300 => response
                .body_mut()
                .read_json()
                .with_context(|| format!("decode GET {path}"))?,
            401 => {
                return Err(anyhow::Error::from(AuthenticationRequired)
                    .context(format!("GET {path}: GitHub HTTP status 401")));
            }
            404 | 410 => {
                return Err(anyhow::Error::from(NotFound)
                    .context(format!("GET {path}: GitHub HTTP status {status}")));
            }
            _ => anyhow::bail!("GET {path}: GitHub HTTP status {status}"),
        };
        Ok(crate::poll::RestResponse {
            status,
            etag,
            last_modified,
            link,
            body,
        })
    }
}

fn http_response_data(status: u16, body: Value) -> Result<Value> {
    if !(200..300).contains(&status) {
        let message = body["message"].as_str().unwrap_or("message unavailable");
        let diagnostic = format!("GitHub HTTP status {status}: {message}");
        if body["errors"].as_array().is_some_and(|e| !e.is_empty()) {
            return response_data(body).with_context(|| diagnostic);
        }
        if status == 401 {
            return Err(anyhow::Error::from(AuthenticationRequired).context(diagnostic));
        }
        anyhow::bail!("{diagnostic}");
    }
    response_data(body)
}

/// Node IDs for opening an issue, resolved by `GitHub::issue_target`.
pub struct IssueTarget {
    repository: String,
    repository_id: String,
    label: String,
    label_id: String,
    assignee: String,
    assignee_id: String,
}

pub struct GitHub<T> {
    pub transport: T,
    pub owner: String,
    pub number: u64,
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("missing GitHub {key}"))
}

fn connection(
    t: &impl Transport,
    query: &str,
    variables: Value,
    pointer: &str,
) -> Result<(Vec<Value>, bool)> {
    let mut variables = variables;
    let mut nodes = Vec::new();
    let mut cursors = BTreeSet::new();
    let mut complete = true;
    loop {
        let data = t.graphql(query, variables.clone())?;
        let conn = data.pointer(pointer).context("missing GitHub connection")?;
        let page = conn["nodes"]
            .as_array()
            .context("missing connection nodes")?;
        for node in page {
            if node.is_null() {
                complete = false;
            } else {
                nodes.push(node.clone());
            }
        }
        match conn["pageInfo"]["hasNextPage"].as_bool() {
            Some(false) => return Ok((nodes, complete)),
            Some(true) => {
                let cursor = text(&conn["pageInfo"], "endCursor")?;
                ensure!(
                    cursors.insert(cursor.to_owned()),
                    "GitHub pagination cursor repeated"
                );
                variables["cursor"] = cursor.into();
            }
            None => return Ok((nodes, false)),
        }
    }
}

fn typed_field(
    fields: &[Value],
    values: &BTreeMap<String, Value>,
    name: &str,
) -> Result<Option<Value>> {
    let schema = fields.iter().find(|f| f["name"].as_str() == Some(name));
    if schema.is_none() && name != "Status" && !values.contains_key(name) {
        return Ok(None);
    }
    let schema = schema.context("missing typed field schema")?;
    ensure!(
        schema["dataType"] == "SINGLE_SELECT",
        "typed field is not single-select"
    );
    if let Some(value) = values.get(name) {
        ensure!(
            value["__typename"] == "ProjectV2ItemFieldSingleSelectValue",
            "incompatible typed field value"
        );
        let option = text(value, "optionId")?;
        let selected = text(value, "name")?;
        ensure!(
            schema["options"].as_array().is_some_and(|opts| opts
                .iter()
                .any(|o| o["id"] == option && o["name"] == selected)),
            "unknown typed field option"
        );
        Ok(Some(selected.into()))
    } else {
        Ok(None)
    }
}

/// One `NAME=VALUE` edit resolved against a project field schema. Offline snapshot
/// schemas may omit node IDs; only a live write needs them.
#[derive(Debug)]
pub struct FieldWrite {
    pub name: String,
    pub value: String,
    select: bool,
    field_id: Option<String>,
    option_id: Option<String>,
}

/// Resolve every edit before anything is written. A writer that created fields or
/// options, or coerced values, would let a typo change the board's schema or put a
/// value where readers and automations expect an option; so each field must exist
/// exactly once, be single-select (with exactly one matching option) or text, and be
/// named only once. Edits are therefore bounded by the schema's own field count.
pub fn field_writes(fields: &[Value], edits: &[String]) -> Result<Vec<FieldWrite>> {
    let mut writes: Vec<FieldWrite> = Vec::new();
    for edit in edits {
        let (name, value) = edit
            .split_once('=')
            .with_context(|| format!("expected NAME=VALUE, got {edit:?}"))?;
        ensure!(!name.is_empty(), "expected NAME=VALUE, got {edit:?}");
        ensure!(
            !value.is_empty(),
            "{name}: clearing a field is not supported"
        );
        ensure!(
            writes.iter().all(|w| w.name != name),
            "{name} is set more than once"
        );
        let named: Vec<_> = fields.iter().filter(|f| f["name"] == name).collect();
        ensure!(!named.is_empty(), "board has no {name} field");
        ensure!(named.len() == 1, "board has more than one {name} field");
        let field = named[0];
        let (select, option_id) = match field["dataType"].as_str() {
            Some("SINGLE_SELECT") => {
                let options = field["options"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let matching: Vec<_> = options.iter().filter(|o| o["name"] == value).collect();
                ensure!(
                    matching.len() == 1,
                    "{name} has no unambiguous option {value:?}; options: {}",
                    options
                        .iter()
                        .filter_map(|o| o["name"].as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                (true, matching[0]["id"].as_str().map(str::to_owned))
            }
            Some("TEXT") => (false, None),
            other => anyhow::bail!(
                "{name} is a {} field; only single-select and text fields can be set",
                other.unwrap_or("untyped")
            ),
        };
        writes.push(FieldWrite {
            name: name.into(),
            value: value.into(),
            select,
            field_id: field["id"].as_str().map(str::to_owned),
            option_id,
        });
    }
    Ok(writes)
}

// The project (id, url) and its complete field schema; shared by snapshots and `project init`.
fn read_schema(t: &impl Transport, owner: &str, number: u64) -> Result<(Value, Vec<Value>, bool)> {
    let data = t.graphql("query($owner:String!,$number:Int!){repositoryOwner(login:$owner){... on Organization{projectV2(number:$number){id url}} ... on User{projectV2(number:$number){id url}}}}", json!({"owner":owner,"number":number}))?;
    let project = data["repositoryOwner"]["projectV2"].clone();
    let (fields, complete) = connection(
        t,
        "query($id:ID!,$cursor:String){node(id:$id){... on ProjectV2{fields(first:100,after:$cursor){nodes{... on ProjectV2Field{id name dataType} ... on ProjectV2SingleSelectField{id name dataType options{id name}} ... on ProjectV2IterationField{id name dataType configuration{iterations{id title startDate duration} completedIterations{id title startDate duration}}}} pageInfo{hasNextPage endCursor}}}}}",
        json!({"id":text(&project, "id")?}),
        "/node/fields",
    )?;
    Ok((project, fields, complete))
}

impl<T: Transport> Forge for GitHub<T> {
    fn capabilities(&self) -> Result<Vec<Capability>> {
        // Read access never establishes mutation authority.
        Ok(vec![])
    }

    fn snapshot(&self) -> Result<Snapshot> {
        self.snapshot_with_policy(&board_core::AgentPolicy::parse(
            board_core::DEFAULT_AGENT_POLICY,
        )?)
    }
}

pub const STATUS_BODY_LIMIT: usize = 4_000;

fn validate_status_body(body: &str) -> Result<()> {
    ensure!(!body.trim().is_empty(), "status body must not be empty");
    // Bound publication size, including generated summaries, before any mutation.
    ensure!(
        body.chars().count() <= STATUS_BODY_LIMIT,
        "status body exceeds {STATUS_BODY_LIMIT} characters"
    );
    Ok(())
}

fn status_summary(items: &[Value], since: Option<&str>) -> Result<String> {
    let parse = |s: &str| {
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
            .context("invalid status summary timestamp")
    };
    let cutoff = since.map(parse).transpose()?;
    let mut changes = BTreeMap::new();
    for item in items {
        let content = &item["content"];
        ensure!(
            matches!(
                content["__typename"].as_str(),
                Some("Issue" | "PullRequest")
            ),
            "cannot summarize redacted or draft project items"
        );
        let created = parse(text(content, "createdAt")?)?;
        let updated = parse(text(content, "updatedAt")?)?;
        ensure!(
            content
                .get("closedAt")
                .is_some_and(|value| value.is_null() || value.is_string()),
            "missing or invalid closedAt"
        );
        let closed = content["closedAt"].as_str().map(parse).transpose()?;
        if cutoff.is_some_and(|cutoff| updated <= cutoff) {
            continue;
        }
        let change = if closed.is_some_and(|closed| cutoff.is_none_or(|cutoff| closed > cutoff)) {
            "closed"
        } else if cutoff.is_none_or(|cutoff| created > cutoff) {
            "created"
        } else {
            "updated"
        };
        let repo = text(&content["repository"], "nameWithOwner")?;
        let number = content["number"]
            .as_u64()
            .context("missing content number")?;
        let kind = text(content, "__typename")?;
        ensure!(
            changes.insert((repo, number, kind), change).is_none(),
            "duplicate project content"
        );
    }
    let mut body = match since {
        Some(since) => format!("Project activity since {since}:\n"),
        None => "Project activity (first status update):\n".to_owned(),
    };
    if changes.is_empty() {
        body.push_str("\nNo issue or pull request activity.\n");
    }
    for ((repo, number, kind), change) in changes {
        use std::fmt::Write;
        writeln!(body, "- {repo}#{number} ({kind}): {change}")?;
    }
    validate_status_body(&body)?;
    Ok(body)
}

impl<T: Transport> GitHub<T> {
    /// Explicit operator publication, not delegated output application.
    pub fn status_update(&self, status: &str, body: Option<&str>) -> Result<String> {
        let status = match status {
            "on-track" => "ON_TRACK",
            "at-risk" => "AT_RISK",
            "off-track" => "OFF_TRACK",
            _ => anyhow::bail!("invalid project status"),
        };
        if let Some(body) = body {
            validate_status_body(body)?;
        }
        let (id, _, _) = self.schema()?;
        let generated;
        let body = match body {
            Some(body) => body,
            None => {
                let diagnostic = self.diagnostic();
                let transport = ReadBudget {
                    transport: &diagnostic,
                    remaining: std::cell::Cell::new(SNAPSHOT_REQUEST_LIMIT),
                };
                let (updates, complete) = connection(
                    &transport,
                    "query($id:ID!,$cursor:String){node(id:$id){... on ProjectV2{statusUpdates(first:100,after:$cursor){nodes{createdAt} pageInfo{hasNextPage endCursor}}}}}",
                    json!({"id":id}),
                    "/node/statusUpdates",
                )?;
                ensure!(complete, "incomplete status update history");
                let since = updates
                    .iter()
                    .map(|u| {
                        let value = text(u, "createdAt")?;
                        let timestamp = time::OffsetDateTime::parse(
                            value,
                            &time::format_description::well_known::Rfc3339,
                        )
                        .context("invalid status update timestamp")?;
                        Ok((timestamp, value))
                    })
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .max()
                    .map(|(_, value)| value);
                let (items, complete) = connection(
                    &transport,
                    "query($id:ID!,$cursor:String){node(id:$id){... on ProjectV2{items(first:100,after:$cursor){nodes{content{__typename ... on Issue{number createdAt updatedAt closedAt repository{nameWithOwner}} ... on PullRequest{number createdAt updatedAt closedAt repository{nameWithOwner}}}} pageInfo{hasNextPage endCursor}}}}}",
                    json!({"id":id}),
                    "/node/items",
                )?;
                ensure!(complete, "incomplete project items");
                generated = status_summary(&items, since)?;
                &generated
            }
        };
        validate_status_body(body)?;
        let data = self.diagnostic().graphql(
            "mutation($project:ID!,$status:ProjectV2StatusUpdateStatus!,$body:String!){createProjectV2StatusUpdate(input:{projectId:$project,status:$status,body:$body}){statusUpdate{id body status}}}",
            json!({"project":id,"status":status,"body":body}),
        )?;
        let receipt = &data["createProjectV2StatusUpdate"]["statusUpdate"];
        ensure!(
            receipt["body"] == body && receipt["status"] == status,
            "status update receipt mismatch"
        );
        Ok(text(receipt, "id")?.to_owned())
    }

    fn diagnostic(&self) -> DiagnosticTransport<'_, T> {
        DiagnosticTransport {
            transport: &self.transport,
            project: format!("{}/{}", self.owner, self.number),
            repository: String::new(),
        }
    }

    /// The project's node ID, URL and complete field schema, without reading items.
    /// The returned URL must name the requested project before anything writes to its ID.
    pub fn schema(&self) -> Result<(String, Url, Vec<Value>)> {
        ensure!(self.number > 0, "project number must be positive");
        let diagnostic = self.diagnostic();
        let transport = ReadBudget {
            transport: &diagnostic,
            remaining: std::cell::Cell::new(SNAPSHOT_REQUEST_LIMIT),
        };
        let (project, fields, complete) = read_schema(&transport, &self.owner, self.number)?;
        let url: Url = text(&project, "url")?
            .parse()
            .context("parse project URL")?;
        validate_github_project(&url)?;
        let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
        ensure!(
            parts[1].eq_ignore_ascii_case(&self.owner) && parts[3] == self.number.to_string(),
            "GitHub returned project {url}, not {}/{}",
            self.owner,
            self.number
        );
        ensure!(complete, "incomplete project field schema");
        Ok((text(&project, "id")?.to_owned(), url, fields))
    }

    /// The issue's node ID and its item ID on `project`, if it is a member. A pull request
    /// number is refused by GitHub's `issue` lookup, so it can never be enrolled as an issue.
    pub fn issue_item(&self, project: &str, issue: &ItemRef) -> Result<(String, Option<String>)> {
        ensure!(issue.host == DEFAULT_HOST, "unsupported item host");
        let (owner, name) = issue
            .repository
            .split_once('/')
            .context("GitHub repositories require OWNER/REPO")?;
        let transport = DiagnosticTransport {
            transport: &self.transport,
            project: format!("{}/{}", self.owner, self.number),
            repository: issue.repository.clone(),
        };
        let data = transport.graphql(
            "query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,name:$repo){issue(number:$number){id number repository{nameWithOwner} projectItems(first:100){nodes{id project{id}} pageInfo{hasNextPage}}}}}",
            json!({"owner":owner,"repo":name,"number":issue.number}),
        )?;
        let found = &data["repository"]["issue"];
        ensure!(
            found["number"].as_u64() == Some(issue.number)
                && found["repository"]["nameWithOwner"]
                    .as_str()
                    .is_some_and(|r| r.eq_ignore_ascii_case(&issue.repository)),
            "GitHub returned a different issue than {}",
            issue.display()
        );
        let items = &found["projectItems"];
        let mut members = Vec::new();
        for node in items["nodes"].as_array().context("missing project items")? {
            if text(&node["project"], "id")? == project {
                members.push(text(node, "id")?.to_owned());
            }
        }
        ensure!(
            members.len() <= 1,
            "issue has more than one item on the board"
        );
        // Absence on a truncated page is not evidence of absence: an edit could miss
        // the item, and an add could create a second one.
        ensure!(
            !members.is_empty() || items["pageInfo"]["hasNextPage"] == false,
            "issue is on more than 100 projects; board membership unknown"
        );
        Ok((text(found, "id")?.to_owned(), members.pop()))
    }

    /// Add an issue to the project, returning its item ID.
    pub fn add_item(&self, project: &str, issue: &str) -> Result<String> {
        let data = self.diagnostic().graphql(
            "mutation($project:ID!,$issue:ID!){addProjectV2ItemById(input:{projectId:$project,contentId:$issue}){item{id}}}",
            json!({"project":project,"issue":issue}),
        )?;
        Ok(text(&data["addProjectV2ItemById"]["item"], "id")?.to_owned())
    }

    /// Set one validated field and check the value GitHub reports back for it.
    pub fn set_field(&self, project: &str, item: &str, write: &FieldWrite) -> Result<()> {
        let field = write
            .field_id
            .as_deref()
            .context("field schema lacks node IDs")?;
        let value = match &write.option_id {
            Some(option) => json!({"singleSelectOptionId": option}),
            None if write.select => anyhow::bail!("option schema lacks node IDs"),
            None => json!({"text": write.value}),
        };
        let data = self.diagnostic().graphql(
            "mutation($project:ID!,$item:ID!,$field:ID!,$value:ProjectV2FieldValue!,$name:String!){updateProjectV2ItemFieldValue(input:{projectId:$project,itemId:$item,fieldId:$field,value:$value}){projectV2Item{id fieldValueByName(name:$name){... on ProjectV2ItemFieldSingleSelectValue{optionId} ... on ProjectV2ItemFieldTextValue{text}}}}}",
            json!({"project":project,"item":item,"field":field,"value":value,"name":write.name}),
        )?;
        let receipt = &data["updateProjectV2ItemFieldValue"]["projectV2Item"];
        let observed = &receipt["fieldValueByName"];
        ensure!(
            receipt["id"].as_str() == Some(item)
                && match &write.option_id {
                    Some(option) => observed["optionId"].as_str() == Some(option),
                    None => observed["text"].as_str() == Some(&write.value),
                },
            "{} update receipt mismatch",
            write.name
        );
        Ok(())
    }

    /// Create one project field; single-select options are created in the given order.
    pub fn create_field(
        &self,
        project: &str,
        name: &str,
        data_type: &str,
        options: &[&str],
    ) -> Result<String> {
        let options: Option<Vec<_>> = (data_type == "SINGLE_SELECT").then(|| {
            options
                .iter()
                .map(|name| json!({"name":name,"color":"GRAY","description":""}))
                .collect()
        });
        let data = self.diagnostic().graphql(
            "mutation($id:ID!,$name:String!,$dataType:ProjectV2CustomFieldType!,$options:[ProjectV2SingleSelectFieldOptionInput!]){createProjectV2Field(input:{projectId:$id,dataType:$dataType,name:$name,singleSelectOptions:$options}){projectV2Field{... on ProjectV2Field{id} ... on ProjectV2SingleSelectField{id}}}}",
            // Named `id` so a failure names the project node it was writing to.
            json!({"id":project,"name":name,"dataType":data_type,"options":options}),
        )?;
        Ok(text(&data["createProjectV2Field"]["projectV2Field"], "id")?.to_owned())
    }

    fn repository_diagnostic(&self, repository: &str) -> DiagnosticTransport<'_, T> {
        DiagnosticTransport {
            transport: &self.transport,
            project: format!("{}/{}", self.owner, self.number),
            repository: repository.into(),
        }
    }

    /// Resolve where an issue with one label and one assignee can be opened, before
    /// anything is written. A missing label is refused rather than created, as
    /// `field_writes` refuses new fields, and the assignee must be assignable there:
    /// GitHub silently drops anyone else, which would leave an unassigned issue behind.
    pub fn issue_target(
        &self,
        repository: &str,
        label: &str,
        assignee: &str,
    ) -> Result<IssueTarget> {
        let (owner, name) = repository
            .split_once('/')
            .context("GitHub repositories require OWNER/REPO")?;
        ensure!(
            is_github_login(assignee),
            "invalid GitHub login {assignee:?}"
        );
        let data = self.repository_diagnostic(repository).graphql(
            "query($owner:String!,$repo:String!,$label:String!,$login:String!){repository(owner:$owner,name:$repo){id nameWithOwner label(name:$label){id name} assignableUsers(query:$login,first:100){nodes{id login}}}}",
            json!({"owner":owner,"repo":name,"label":label,"login":assignee}),
        )?;
        let repo = &data["repository"];
        ensure!(
            repo["nameWithOwner"]
                .as_str()
                .is_some_and(|r| r.eq_ignore_ascii_case(repository)),
            "GitHub returned a different repository than {repository}"
        );
        // GitHub's label lookup is case-insensitive; the label applied must be this one.
        ensure!(
            repo["label"]["name"] == label,
            "{repository} has no {label:?} label; create it first"
        );
        // The query matches logins and names by prefix; only the exact login counts.
        let user = repo["assignableUsers"]["nodes"]
            .as_array()
            .context("missing assignable users")?
            .iter()
            .find(|u| {
                u["login"]
                    .as_str()
                    .is_some_and(|l| l.eq_ignore_ascii_case(assignee))
            })
            .with_context(|| format!("{assignee} cannot be assigned issues in {repository}"))?;
        Ok(IssueTarget {
            repository: repository.into(),
            repository_id: text(repo, "id")?.to_owned(),
            label: label.into(),
            label_id: text(&repo["label"], "id")?.to_owned(),
            assignee: assignee.into(),
            assignee_id: text(user, "id")?.to_owned(),
        })
    }

    /// Open an issue at a resolved target, returning its node ID and number. The receipt
    /// is checked; on a mismatch the issue exists and the error names it.
    pub fn create_issue(
        &self,
        target: &IssueTarget,
        title: &str,
        body: &str,
    ) -> Result<(String, u64)> {
        let created = self.repository_diagnostic(&target.repository).graphql(
            "mutation($repo:ID!,$title:String!,$body:String!,$label:ID!,$user:ID!){createIssue(input:{repositoryId:$repo,title:$title,body:$body,labelIds:[$label],assigneeIds:[$user]}){issue{id number labels(first:2){nodes{name}} assignees(first:2){nodes{login}}}}}",
            json!({"repo":target.repository_id,"title":title,"body":body,"label":target.label_id,"user":target.assignee_id}),
        )?;
        let issue = &created["createIssue"]["issue"];
        let number = issue["number"]
            .as_u64()
            .context("missing created issue number")?;
        let names = |key: &str, field: &str| -> Vec<String> {
            issue[key]["nodes"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|n| n[field].as_str().map(str::to_lowercase))
                .collect()
        };
        ensure!(
            names("labels", "name") == [target.label.to_lowercase()]
                && names("assignees", "login") == [target.assignee.to_lowercase()],
            "opened {}#{number}, but GitHub did not apply label {} and assignee {}",
            target.repository,
            target.label,
            target.assignee
        );
        Ok((text(issue, "id")?.to_owned(), number))
    }

    /// The issue's node ID, whether it is open, and its label names. A truncated label
    /// list is refused: an unseen page could hold the label a caller requires.
    pub fn issue_labels(&self, issue: &ItemRef) -> Result<(String, bool, Vec<String>)> {
        ensure!(issue.host == DEFAULT_HOST, "unsupported item host");
        let (owner, name) = issue
            .repository
            .split_once('/')
            .context("GitHub repositories require OWNER/REPO")?;
        let data = self.repository_diagnostic(&issue.repository).graphql(
            "query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,name:$repo){issue(number:$number){id number state repository{nameWithOwner} labels(first:100){nodes{name} pageInfo{hasNextPage}}}}}",
            json!({"owner":owner,"repo":name,"number":issue.number}),
        )?;
        let found = &data["repository"]["issue"];
        ensure!(
            found["number"].as_u64() == Some(issue.number)
                && found["repository"]["nameWithOwner"]
                    .as_str()
                    .is_some_and(|r| r.eq_ignore_ascii_case(&issue.repository)),
            "GitHub returned a different issue than {}",
            issue.display()
        );
        let labels = &found["labels"];
        ensure!(
            labels["pageInfo"]["hasNextPage"] == false,
            "issue has more than 100 labels"
        );
        let names = labels["nodes"]
            .as_array()
            .context("missing issue labels")?
            .iter()
            .map(|n| text(n, "name").map(str::to_owned))
            .collect::<Result<_>>()?;
        Ok((
            text(found, "id")?.to_owned(),
            text(found, "state")? == "OPEN",
            names,
        ))
    }

    /// Comment on an issue, returning the comment URL.
    pub fn comment(&self, repository: &str, issue: &str, body: &str) -> Result<String> {
        let data = self.repository_diagnostic(repository).graphql(
            "mutation($id:ID!,$body:String!){addComment(input:{subjectId:$id,body:$body}){commentEdge{node{url}}}}",
            json!({"id":issue,"body":body}),
        )?;
        Ok(text(&data["addComment"]["commentEdge"]["node"], "url")?.to_owned())
    }

    /// Close an issue as completed and check that GitHub reports it closed.
    pub fn close_completed(&self, repository: &str, issue: &str) -> Result<()> {
        let data = self.repository_diagnostic(repository).graphql(
            "mutation($id:ID!){closeIssue(input:{issueId:$id,stateReason:COMPLETED}){issue{id state}}}",
            json!({"id":issue}),
        )?;
        let receipt = &data["closeIssue"]["issue"];
        ensure!(
            receipt["id"].as_str() == Some(issue) && receipt["state"] == "CLOSED",
            "close receipt mismatch"
        );
        Ok(())
    }

    pub fn snapshot_with_policy(&self, policy: &board_core::AgentPolicy) -> Result<Snapshot> {
        ensure!(self.number > 0, "project number must be positive");
        let diagnostic = self.diagnostic();
        let transport = ReadBudget {
            transport: &diagnostic,
            remaining: std::cell::Cell::new(SNAPSHOT_REQUEST_LIMIT),
        };
        let (project, fields, mut complete) = read_schema(&transport, &self.owner, self.number)?;
        let id = text(&project, "id")?;
        let clock = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)?;
        let (members, members_complete) = connection(
            &transport,
            "query($id:ID!,$cursor:String){node(id:$id){... on ProjectV2{items(first:100,after:$cursor){nodes{id content{__typename ... on Issue{id number title state stateReason createdAt updatedAt closedAt repository{nameWithOwner}} ... on PullRequest{id number title state createdAt updatedAt closedAt mergedAt repository{nameWithOwner}}}} pageInfo{hasNextPage endCursor}}}}}",
            json!({"id":id}),
            "/node/items",
        )?;
        complete &= members_complete;
        let mut items = Vec::new();
        for member in members {
            let content = &member["content"];
            let kind = content["__typename"].as_str();
            if !matches!(kind, Some("Issue" | "PullRequest")) {
                complete = false;
                continue;
            }
            let content_id = text(content, "id")?;
            let native_state = text(content, "state")?;
            ensure!(
                matches!(native_state, "OPEN" | "CLOSED")
                    || (kind == Some("PullRequest") && native_state == "MERGED"),
                "unknown GitHub content state"
            );
            let (values, values_complete) = connection(
                &transport,
                "query($id:ID!,$cursor:String){node(id:$id){... on ProjectV2Item{fieldValues(first:100,after:$cursor){nodes{__typename ... on ProjectV2ItemFieldSingleSelectValue{name optionId field{... on ProjectV2SingleSelectField{name}}} ... on ProjectV2ItemFieldTextValue{text field{... on ProjectV2Field{name}}} ... on ProjectV2ItemFieldNumberValue{number field{... on ProjectV2Field{name}}} ... on ProjectV2ItemFieldDateValue{date field{... on ProjectV2Field{name}}} ... on ProjectV2ItemFieldIterationValue{title iterationId field{... on ProjectV2IterationField{name}}} ... on ProjectV2ItemFieldRepositoryValue{repository{id nameWithOwner} field{... on ProjectV2Field{name}}} ... on ProjectV2ItemFieldMilestoneValue{milestone{id title state dueOn} field{... on ProjectV2Field{name}}} ... on ProjectV2ItemFieldLabelValue{labels(first:100){nodes{id name} pageInfo{hasNextPage}} field{... on ProjectV2Field{name}}} ... on ProjectV2ItemFieldUserValue{users(first:100){nodes{id login} pageInfo{hasNextPage}} field{... on ProjectV2Field{name}}} ... on ProjectV2ItemFieldPullRequestValue{pullRequests(first:100){nodes{id number url title state} pageInfo{hasNextPage}} field{... on ProjectV2Field{name}}} ... on ProjectV2ItemFieldReviewerValue{reviewers(first:100){nodes{__typename ... on User{id login} ... on Team{id name}} pageInfo{hasNextPage}} field{... on ProjectV2Field{name}}}} pageInfo{hasNextPage endCursor}}}}}",
                json!({"id":text(&member,"id")?}),
                "/node/fieldValues",
            )?;
            let mut item_fields = BTreeMap::new();
            let mut values_supported = true;
            for mut value in values {
                let nested = match value["__typename"].as_str() {
                    Some("ProjectV2ItemFieldLabelValue") => Some("labels"),
                    Some("ProjectV2ItemFieldUserValue") => Some("users"),
                    Some("ProjectV2ItemFieldPullRequestValue") => Some("pullRequests"),
                    Some("ProjectV2ItemFieldReviewerValue") => Some("reviewers"),
                    _ => None,
                };
                if let Some(field) = nested {
                    // These value types are not nodes: retain the inline page,
                    // but do not treat truncated or redacted facts as complete.
                    let nested = &value[field];
                    let nodes = nested["nodes"].as_array();
                    values_supported &= nested["pageInfo"]["hasNextPage"].as_bool() == Some(false)
                        && nodes.is_some_and(|nodes| {
                            nodes.iter().all(|node| {
                                node["id"].is_string()
                                    && match field {
                                        "labels" => node["name"].is_string(),
                                        "users" => node["login"].is_string(),
                                        "pullRequests" => {
                                            node["number"].as_u64().is_some()
                                                && node["url"].is_string()
                                                && node["title"].is_string()
                                                && matches!(
                                                    node["state"].as_str(),
                                                    Some("OPEN" | "CLOSED" | "MERGED")
                                                )
                                        }
                                        "reviewers" => match node["__typename"].as_str() {
                                            Some("User") => node["login"].is_string(),
                                            Some("Team") => node["name"].is_string(),
                                            _ => false,
                                        },
                                        _ => false,
                                    }
                            })
                        });
                    value[field] = nodes.cloned().unwrap_or_default().into();
                }
                if let Some(name) = value["field"]["name"].as_str() {
                    ensure!(
                        name != "Agent" || !item_fields.contains_key(name),
                        "duplicate Agent value"
                    );
                    item_fields.insert(name.to_owned(), value);
                } else {
                    // An unimplemented or redacted value must not become an
                    // observed null. Keep its type and block this item's repairs.
                    values_supported = false;
                    item_fields.insert(format!("unsupported:{}", item_fields.len()), value);
                }
            }
            let mut facts_complete = values_complete
                && values_supported
                && content["createdAt"].is_string()
                && content["updatedAt"].is_string()
                && content.get("closedAt").is_some()
                && (kind != Some("PullRequest") || content.get("mergedAt").is_some());
            let mut facts = BTreeMap::new();
            for (field, selection) in [("labels", "name"), ("assignees", "login")] {
                let query = format!(
                    "query($id:ID!,$cursor:String){{node(id:$id){{... on Issue{{{field}(first:100,after:$cursor){{nodes{{{selection}}} pageInfo{{hasNextPage endCursor}}}}}} ... on PullRequest{{{field}(first:100,after:$cursor){{nodes{{{selection}}} pageInfo{{hasNextPage endCursor}}}}}}}}}}"
                );
                let (nodes, full) = connection(
                    &transport,
                    &query,
                    json!({"id":content_id}),
                    &format!("/node/{field}"),
                )?;
                facts_complete &= full;
                facts.insert(
                    field,
                    nodes
                        .iter()
                        .map(|n| text(n, selection).map(str::to_owned))
                        .collect::<Result<Vec<_>>>()?,
                );
            }
            let mut typed_complete = facts_complete;
            let status = typed_field(&fields, &item_fields, "Status")
                .and_then(|v| {
                    v.map(serde_json::from_value)
                        .transpose()
                        .map_err(Into::into)
                })
                .unwrap_or_else(|_| {
                    typed_complete = false;
                    None
                });
            let priority = typed_field(&fields, &item_fields, "Priority")
                .and_then(|v| {
                    v.map(serde_json::from_value)
                        .transpose()
                        .map_err(Into::into)
                })
                .unwrap_or_else(|_| {
                    typed_complete = false;
                    None
                });
            let turn = typed_field(&fields, &item_fields, "Turn")
                .and_then(|v| {
                    v.map(serde_json::from_value)
                        .transpose()
                        .map_err(Into::into)
                })
                .unwrap_or_else(|_| {
                    typed_complete = false;
                    None
                });
            complete &= typed_complete;
            let identity = ItemRef {
                host: DEFAULT_HOST.into(),
                repository: text(&content["repository"], "nameWithOwner")?.into(),
                number: content["number"]
                    .as_u64()
                    .context("missing content number")?,
            };
            let url = format!(
                "https://{}/{}/{}/{}",
                DEFAULT_HOST,
                identity.repository,
                if kind == Some("Issue") {
                    "issues"
                } else {
                    "pull"
                },
                identity.number
            )
            .parse()?;
            items.push(Item {
                identity,
                assignment: None,
                title: text(content, "title")?.into(),
                state: if native_state == "OPEN" {
                    "open"
                } else {
                    "closed"
                }
                .into(),
                state_reason: content["stateReason"].as_str().map(str::to_owned),
                status,
                priority,
                turn,
                fields_complete: typed_complete,
                field_errors: vec![],
                evidence: vec![Evidence {
                    url,
                    observed_at: clock.clone(),
                    provenance: "GitHub GraphQL".into(),
                }],
                content_kind: if kind == Some("Issue") {
                    "issue"
                } else {
                    "pull_request"
                }
                .into(),
                labels: facts.remove("labels").unwrap_or_default(),
                assignees: facts.remove("assignees").unwrap_or_default(),
                timestamps: ["createdAt", "updatedAt", "closedAt", "mergedAt"]
                    .into_iter()
                    .filter_map(|k| {
                        content
                            .get(k)
                            .map(|v| (k.into(), v.as_str().map(str::to_owned)))
                    })
                    .collect(),
                fields: item_fields,
            });
        }
        let mut snapshot = Snapshot {
            schema: board_core::SNAPSHOT_SCHEMA.into(),
            project: text(&project, "url")?.parse()?,
            clock: clock.clone(),
            coverage: Coverage {
                complete,
                observed_at: clock,
            },
            items,
            fields,
        };
        validate_github_project(&snapshot.project)?;
        snapshot.validate()?;
        policy.normalize(&mut snapshot)?;
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    #[test]
    fn client_uses_platform_trust_with_verification_enabled() {
        let client = Client::with_token("test-key-not-secret".into());
        let tls = client.agent.config().tls_config();
        assert!(matches!(
            tls.root_certs(),
            ureq::tls::RootCerts::PlatformVerifier
        ));
        assert!(!tls.disable_verification());
    }

    struct Recorded {
        responses: RefCell<VecDeque<Value>>,
        variables: RefCell<Vec<Value>>,
    }

    impl Transport for Recorded {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            if query.contains("fieldValues(first") {
                for (ty, field) in [
                    ("Label", "labels"),
                    ("User", "users"),
                    ("PullRequest", "pullRequests"),
                    ("Reviewer", "reviewers"),
                ] {
                    assert!(query.contains(&format!(
                        "... on ProjectV2ItemField{ty}Value{{{field}(first:100)"
                    )));
                }
            }
            self.variables.borrow_mut().push(variables);
            self.responses
                .borrow_mut()
                .pop_front()
                .context("unexpected request")
        }
    }

    fn page(nodes: Value, next: bool, cursor: Value) -> Value {
        json!({"node":{"connection":{"nodes":nodes,"pageInfo":{"hasNextPage":next,"endCursor":cursor}}}})
    }

    #[test]
    fn status_publication_validates_before_writing() {
        for status in ["on-track", "at-risk", "off-track"] {
            let native = status.replace('-', "_").to_uppercase();
            let github = GitHub {
                owner: "example".into(),
                number: 1,
                transport: Recorded {
                    responses: RefCell::new(VecDeque::from([
                        json!({"repositoryOwner":{"projectV2":{"id":"P","url":"https://github.com/orgs/example/projects/1"}}}),
                        json!({"node":{"fields":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}),
                        json!({"createProjectV2StatusUpdate":{"statusUpdate":{"id":"U","body":"Report","status":native}}}),
                    ])),
                    variables: RefCell::new(vec![]),
                },
            };
            assert_eq!(github.status_update(status, Some("Report")).unwrap(), "U");
            assert_eq!(
                github.transport.variables.borrow()[2],
                json!({"project":"P","body":"Report","status":native})
            );
            for body in [
                "".to_owned(),
                " ".to_owned(),
                "x".repeat(STATUS_BODY_LIMIT + 1),
            ] {
                assert!(github.status_update(status, Some(&body)).is_err());
            }
            assert!(github.status_update("unknown", Some("Report")).is_err());
            assert_eq!(github.transport.variables.borrow().len(), 3);
        }
    }

    #[test]
    fn summary_is_sorted_and_since_last_update() {
        let make = |number, created, updated, closed| json!({"content":{"__typename":"Issue","repository":{"nameWithOwner":"example/repo"},"number":number,"createdAt":created,"updatedAt":updated,"closedAt":closed}});
        let old = "2026-10-01T00:00:00Z";
        let cutoff = "2026-10-02T00:00:00Z";
        let new = "2026-10-03T00:00:00Z";
        let items = vec![
            make(3, old, new, Some(new)),
            make(2, new, new, None),
            make(1, old, new, None),
            make(4, old, cutoff, None),
        ];
        let body = status_summary(&items, Some(cutoff)).unwrap();
        assert_eq!(
            body,
            format!(
                "Project activity since {cutoff}:\n- example/repo#1 (Issue): updated\n- example/repo#2 (Issue): created\n- example/repo#3 (Issue): closed\n"
            )
        );
        let mut reversed = items.clone();
        reversed.reverse();
        assert_eq!(status_summary(&reversed, Some(cutoff)).unwrap(), body);
        assert!(
            status_summary(&[], None)
                .unwrap()
                .contains("first status update")
        );
        assert!(status_summary(&[json!({"content":null})], None).is_err());
        assert!(status_summary(&items, Some("bad date")).is_err());
        assert!(validate_status_body(&"é".repeat(STATUS_BODY_LIMIT)).is_ok());
        let mut pr = make(5, new, new, None);
        pr["content"]["__typename"] = json!("PullRequest");
        assert!(
            status_summary(&[pr], Some(cutoff))
                .unwrap()
                .contains("(PullRequest): created")
        );
        let many: Vec<_> = (1..200)
            .map(|number| make(number, new, new, None))
            .collect();
        assert!(
            status_summary(&many, None)
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
        let mut missing = make(1, old, new, None);
        missing["content"]
            .as_object_mut()
            .unwrap()
            .remove("closedAt");
        assert!(status_summary(&[missing], None).is_err());
    }

    #[test]
    fn generated_status_publication_and_refusals() {
        for case in [
            "valid",
            "wrong-project",
            "incomplete",
            "redacted",
            "receipt",
        ] {
            let url = if case == "wrong-project" {
                "https://github.com/orgs/other/projects/1"
            } else {
                "https://github.com/orgs/example/projects/1"
            };
            let body = "Project activity since 2026-10-02T00:00:00Z:\n\nNo issue or pull request activity.\n";
            let github = GitHub {
                owner: "example".into(),
                number: 1,
                transport: Recorded {
                    responses: RefCell::new(VecDeque::from([
                        json!({"repositoryOwner":{"projectV2":{"id":"P","url":url}}}),
                        json!({"node":{"fields":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}),
                        json!({"node":{"statusUpdates":{"nodes":[{"createdAt":"2026-10-02T00:00:00Z"},{"createdAt":"2026-10-01T00:00:00Z"}],"pageInfo":{"hasNextPage":false}}}}),
                        json!({"node":{"items":{"nodes":if case == "redacted" {json!([{"content":null}])} else {json!([])},"pageInfo":{"hasNextPage":case == "incomplete","endCursor":null}}}}),
                        json!({"createProjectV2StatusUpdate":{"statusUpdate":{"id":"U","body":if case == "receipt" {"wrong"} else {body},"status":"ON_TRACK"}}}),
                    ])),
                    variables: RefCell::new(vec![]),
                },
            };
            let result = github.status_update("on-track", None);
            assert_eq!(result.is_ok(), case == "valid", "{case}: {result:?}");
            let count = github.transport.variables.borrow().len();
            assert_eq!(
                count,
                match case {
                    "wrong-project" => 2,
                    "incomplete" | "redacted" => 4,
                    _ => 5,
                }
            );
            if case == "valid" {
                assert_eq!(github.transport.variables.borrow()[4]["body"], body);
            }
        }
    }

    #[test]
    fn authentication_table() {
        for (gh, github, expected) in [
            (Some("primary"), Some("fallback"), Some("primary")),
            (Some(""), Some("fallback"), Some("fallback")),
            (None, Some("fallback"), Some("fallback")),
            (Some("primary"), None, Some("primary")),
            (Some(""), Some(""), None),
            (None, None, None),
        ] {
            let result = select_token(gh.map(str::to_owned), github.map(str::to_owned));
            match expected {
                Some(s) => assert_eq!(result.unwrap(), s),
                None => assert!(result.unwrap_err().is::<AuthenticationRequired>()),
            }
        }
        for (status, authentication) in [(401, true), (403, false), (429, false), (500, false)] {
            assert_eq!(
                transport_error(ureq::Error::StatusCode(status))
                    .context("request")
                    .is::<AuthenticationRequired>(),
                authentication
            );
        }
        for (response, authentication) in [
            (
                json!({"errors":[{"type":"UNAUTHORIZED","message":"not echoed"}]}),
                true,
            ),
            (
                json!({"errors":[{"extensions":{"code":"UNAUTHENTICATED"}}]}),
                true,
            ),
            (
                json!({"data":{"partial":true},"errors":[{"type":"FORBIDDEN","message":"not echoed"}]}),
                false,
            ),
        ] {
            let has_message = response["errors"][0]["message"].is_string();
            let error = response_data(response).unwrap_err();
            assert_eq!(error.is::<AuthenticationRequired>(), authentication);
            assert_eq!(error.to_string().contains("not echoed"), has_message);
        }
    }

    #[test]
    fn refusal_diagnostics() {
        let error = response_data(json!({"errors":[{"type":"FORBIDDEN","message":"Projects denied","path":["hidden"]},{"type":"NOT_FOUND","message":"Missing project"}],"data":{"hidden":"body"}})).unwrap_err().to_string();
        assert!(error.contains("FORBIDDEN: Projects denied"));
        assert!(error.contains("NOT_FOUND: Missing project"));
        assert!(!error.contains("hidden"));
        for status in [401, 403, 429, 500] {
            let error =
                http_response_data(status, json!({"message":"Access refused","other":"hidden"}))
                    .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("{status}: Access refused"))
            );
            assert!(!error.to_string().contains("hidden"));
            assert_eq!(error.is::<AuthenticationRequired>(), status == 401);
        }
    }

    #[test]
    fn pagination_table() {
        for (responses, expected_complete, count, requests) in [
            (
                vec![page(json!([{"id":"one"}]), false, Value::Null)],
                true,
                1,
                1,
            ),
            (
                vec![
                    page(json!([{"id":"one"}]), true, json!("next")),
                    page(json!([{"id":"two"}]), false, Value::Null),
                ],
                true,
                2,
                2,
            ),
            (
                vec![page(json!([{"id":"one"},null]), false, Value::Null)],
                false,
                1,
                1,
            ),
            (
                vec![
                    page(json!([null]), true, json!("next")),
                    page(json!([{"id":"two"}]), false, Value::Null),
                ],
                false,
                1,
                2,
            ),
            (
                vec![json!({"node":{"connection":{"nodes":[]}}})],
                false,
                0,
                1,
            ),
        ] {
            let t = Recorded {
                responses: RefCell::new(responses.into()),
                variables: RefCell::new(vec![]),
            };
            let (nodes, complete) =
                connection(&t, "recorded", json!({"id":"project"}), "/node/connection").unwrap();
            assert_eq!(complete, expected_complete);
            assert_eq!(nodes.len(), count);
            assert_eq!(t.variables.borrow().len(), requests);
            if requests == 2 {
                assert_eq!(t.variables.borrow()[1]["cursor"], "next");
            }
        }
    }

    #[test]
    fn aggregate_request_budget_table() {
        for (pages, limit, succeeds) in [(2, 2, true), (3, 2, false)] {
            let t = Recorded {
                responses: RefCell::new(
                    (0..pages)
                        .map(|i| page(json!([]), i + 1 < pages, json!(format!("cursor-{i}"))))
                        .collect(),
                ),
                variables: RefCell::new(vec![]),
            };
            let budget = ReadBudget {
                transport: &t,
                remaining: std::cell::Cell::new(limit),
            };
            let result = connection(&budget, "recorded", json!({}), "/node/connection");
            assert_eq!(result.is_ok(), succeeds);
            if !succeeds {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("request budget exhausted")
                );
            }
            assert_eq!(t.variables.borrow().len(), limit);
            // A second connection cannot reset the snapshot's aggregate budget.
            assert!(connection(&budget, "another", json!({}), "/node/connection").is_err());
            assert_eq!(t.variables.borrow().len(), limit);
        }
    }

    #[test]
    fn snapshot_unique_cursor_exhaustion() {
        let mut responses = vec![json!({"repositoryOwner":{"projectV2":{"id":"P"}}})];
        for i in 1..SNAPSHOT_REQUEST_LIMIT {
            responses.push(json!({"node":{"fields":{"nodes":[],"pageInfo":{"hasNextPage":true,"endCursor":format!("cursor-{i}")}}}}));
        }
        let github = GitHub {
            transport: Recorded {
                responses: RefCell::new(responses.into()),
                variables: RefCell::new(vec![]),
            },
            owner: "example".into(),
            number: 1,
        };
        let error = github.snapshot().unwrap_err();
        assert!(error.to_string().contains("request budget exhausted"));
        assert_eq!(
            github.transport.variables.borrow().len(),
            SNAPSHOT_REQUEST_LIMIT
        );
    }

    #[test]
    fn malformed_pagination_table() {
        for responses in [
            vec![page(json!([]), true, Value::Null)],
            vec![
                page(json!([]), true, json!("same")),
                page(json!([]), true, json!("same")),
            ],
            vec![json!({"node":null})],
        ] {
            let t = Recorded {
                responses: RefCell::new(responses.into()),
                variables: RefCell::new(vec![]),
            };
            assert!(connection(&t, "recorded", json!({}), "/node/connection").is_err());
        }
    }

    #[test]
    fn redacted_membership_blocks_reconcile() {
        let t = Recorded { responses: RefCell::new(vec![
            json!({"repositoryOwner":{"projectV2":{"id":"P","url":"https://github.com/orgs/example/projects/1"}}}),
            json!({"node":{"fields":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}),
            json!({"node":{"items":{"nodes":[{"id":"M","content":null}],"pageInfo":{"hasNextPage":false}}}}),
        ].into()),variables:RefCell::new(vec![]) };
        let snapshot = GitHub {
            transport: t,
            owner: "example".into(),
            number: 1,
        }
        .snapshot()
        .unwrap();
        assert!(!snapshot.coverage.complete);
        let policy = board_core::Policy {
            project: snapshot.project.clone(),
            repositories: vec!["example/intake".into()],
            capabilities: vec![Capability::ProjectFieldEdit],
        };
        let plan = board_core::reconcile(&snapshot, &policy, &snapshot.clock).unwrap();
        assert!(plan.actions.is_empty());
        assert!(plan.diagnostics.contains_key("coverage"));
    }

    #[test]
    fn typed_schema_table() {
        let schema = json!({"name":"Status","dataType":"SINGLE_SELECT","options":[{"id":"done","name":"Done"}]});
        let value = json!({"__typename":"ProjectV2ItemFieldSingleSelectValue","name":"Done","optionId":"done"});
        for (field, selected, valid) in [
            (schema.clone(), Some(value.clone()), true),
            (schema.clone(), None, true),
            (
                json!({"name":"Status","dataType":"TEXT"}),
                Some(json!({"__typename":"ProjectV2ItemFieldTextValue","text":"Done"})),
                false,
            ),
            (
                schema.clone(),
                Some(
                    json!({"__typename":"ProjectV2ItemFieldSingleSelectValue","name":"Done","optionId":"unknown"}),
                ),
                false,
            ),
            (
                schema,
                Some(json!({"__typename":"ProjectV2ItemFieldTextValue","text":"Done"})),
                false,
            ),
        ] {
            let values = selected
                .map(|v| BTreeMap::from([("Status".into(), v)]))
                .unwrap_or_default();
            assert_eq!(typed_field(&[field], &values, "Status").is_ok(), valid);
        }
        assert!(typed_field(&[], &BTreeMap::new(), "Status").is_err());
    }

    #[test]
    fn issue_and_pull_request_facts_table() {
        for (kind, state, reason, repairs) in [
            ("Issue", "OPEN", None, 0),
            ("Issue", "CLOSED", None, 0),
            ("Issue", "CLOSED", Some("COMPLETED"), 0),
            ("Issue", "CLOSED", Some("NOT_PLANNED"), 1),
            ("Issue", "CLOSED", Some("OTHER"), 0),
            ("PullRequest", "OPEN", None, 0),
            ("PullRequest", "MERGED", None, 0),
        ] {
            let conn = |name: &str, nodes: Value| json!({"node":{name:{"nodes":nodes,"pageInfo":{"hasNextPage":false}}}});
            let t = Recorded { responses: RefCell::new(vec![
                json!({"repositoryOwner":{"projectV2":{"id":"P","url":"https://github.com/orgs/example/projects/1"}}}),
                conn("fields",json!([{"name":"Status","dataType":"SINGLE_SELECT","options":[{"id":"done","name":"Done"},{"id":"cancelled","name":"Cancelled"}]}])),
                conn("items",json!([{"id":"M","content":{"__typename":kind,"id":"I","number":7,"title":"recorded","state":state,"stateReason":reason,"repository":{"nameWithOwner":"example/intake"},"createdAt":"2026-10-08T00:00:00Z","updatedAt":"2026-10-08T00:00:00Z","closedAt":null,"mergedAt":null}}])),
                conn("fieldValues",json!([{"__typename":"ProjectV2ItemFieldSingleSelectValue","name":"Done","optionId":"done","field":{"name":"Status"}}])),
                conn("labels",json!([{"name":"bug"}])),conn("assignees",json!([{"login":"human"}])),
            ].into()),variables:RefCell::new(vec![]) };
            let snapshot = GitHub {
                transport: t,
                owner: "example".into(),
                number: 1,
            }
            .snapshot()
            .unwrap();
            assert!(snapshot.coverage.complete);
            let item = &snapshot.items[0];
            assert!(item.fields_complete);
            assert_eq!(item.labels, ["bug"]);
            assert_eq!(item.assignees, ["human"]);
            assert_eq!(item.state, if state == "OPEN" { "open" } else { "closed" });
            assert_eq!(item.state_reason.as_deref(), reason);
            let policy = board_core::Policy {
                project: snapshot.project.clone(),
                repositories: vec!["example/intake".into()],
                capabilities: vec![Capability::ProjectFieldEdit, Capability::CloseIssue],
            };
            let plan = board_core::reconcile(&snapshot, &policy, &snapshot.clock).unwrap();
            assert_eq!(plan.actions.len(), repairs);
            if repairs == 1 {
                assert!(matches!(
                    plan.actions[0].intent,
                    board_core::Intent::SetStatus {
                        value: board_core::Status::Cancelled,
                        ..
                    }
                ));
            }
        }
    }

    #[test]
    fn agent_assignment_reader_table() {
        for (name, valid) in [
            (None, true),
            (Some("rust-worker"), true),
            (Some("custom-worker"), true),
            (Some("unknown"), false),
        ] {
            let conn = |name: &str, nodes: Value| json!({"node":{name:{"nodes":nodes,"pageInfo":{"hasNextPage":false}}}});
            let values = name.map(|name| vec![json!({"__typename":"ProjectV2ItemFieldSingleSelectValue", "name":name, "optionId":"agent", "field":{"name":"Agent"}})]).unwrap_or_default();
            let github = GitHub {
                transport: Recorded {
                    responses: RefCell::new(vec![
                        json!({"repositoryOwner":{"projectV2":{"id":"P","url":"https://github.com/orgs/example/projects/1"}}}),
                        conn("fields", json!([
                            {"name":"Status","dataType":"SINGLE_SELECT","options":[]},
                            {"name":"Agent","dataType":"SINGLE_SELECT","options":[{"id":"agent","name":name.unwrap_or("rust-worker")}]}
                        ])),
                        conn("items", json!([{"id":"M","content":{"__typename":"Issue","id":"I","number":7,"title":"recorded","state":"OPEN","repository":{"nameWithOwner":"example/intake"}}}])),
                        conn("fieldValues", json!(values)),
                        conn("labels", json!([])),
                        conn("assignees", json!([])),
                    ].into()),
                    variables: RefCell::new(vec![]),
                },
                owner: "example".into(),
                number: 1,
            };
            let policy = board_core::AgentPolicy::parse(&format!(
                "{}\n[agents.custom-worker]\nenabled = true\nfields = []\noutputs = []",
                board_core::DEFAULT_AGENT_POLICY
            ))
            .unwrap();
            let result = github.snapshot_with_policy(&policy);
            if valid {
                assert_eq!(result.unwrap().items[0].assignment.as_deref(), name);
            } else {
                assert!(format!("{:#}", result.unwrap_err()).contains("unknown Agent value"));
            }
        }
    }

    #[test]
    fn inline_field_connections_table() {
        // API-shaped synthetic responses: these four value types have no id.
        let fixture: Vec<Value> =
            serde_json::from_str(include_str!("../../../fixtures/github-field-values.json"))
                .unwrap();
        for field in ["labels", "users", "pullRequests", "reviewers"] {
            for (case, expected_complete) in [
                ("complete", true),
                ("empty", true),
                ("truncated", false),
                ("redacted", false),
                ("malformed_object", false),
                ("missing_fact", false),
                ("missing_nodes", false),
                ("missing_page_info", false),
            ] {
                let mut values = fixture.clone();
                let value = values.iter_mut().find(|v| v.get(field).is_some()).unwrap();
                match case {
                    "empty" => value[field]["nodes"] = json!([]),
                    "truncated" => value[field]["pageInfo"]["hasNextPage"] = json!(true),
                    "redacted" => value[field]["nodes"] = json!([null]),
                    "malformed_object" => value[field]["nodes"] = json!([{}]),
                    "missing_fact" => value[field]["nodes"] = json!([{"id":"N"}]),
                    "missing_nodes" => {
                        value[field].as_object_mut().unwrap().remove("nodes");
                    }
                    "missing_page_info" => {
                        value[field].as_object_mut().unwrap().remove("pageInfo");
                    }
                    _ => {}
                }
                let conn = |name: &str, nodes: Value| json!({"node":{name:{"nodes":nodes,"pageInfo":{"hasNextPage":false}}}});
                let github = GitHub {
                    transport: Recorded {
                        responses: RefCell::new(vec![
                            json!({"repositoryOwner":{"projectV2":{"id":"P","url":"https://github.com/orgs/example/projects/1"}}}),
                            conn("fields", json!([{"name":"Status","dataType":"SINGLE_SELECT","options":[{"id":"triage","name":"Triage"}]}])),
                            conn("items", json!([{"id":"M","content":{"__typename":"Issue","id":"I","number":7,"title":"recorded","state":"OPEN","repository":{"nameWithOwner":"example/intake"},"createdAt":"2026-10-08T00:00:00Z","updatedAt":"2026-10-08T00:00:00Z","closedAt":null}}])),
                            conn("fieldValues", json!(values)),
                            conn("labels", json!([])),
                            conn("assignees", json!([])),
                        ].into()),
                        variables: RefCell::new(vec![]),
                    },
                    owner: "example".into(),
                    number: 1,
                };
                let snapshot = github.snapshot().unwrap();
                assert_eq!(
                    snapshot.coverage.complete, expected_complete,
                    "{field}/{case}"
                );
                assert_eq!(snapshot.items[0].fields_complete, expected_complete);
                assert_eq!(github.transport.variables.borrow().len(), 6);
                let policy = board_core::Policy {
                    project: snapshot.project.clone(),
                    repositories: vec!["example/intake".into()],
                    capabilities: vec![Capability::ProjectFieldEdit],
                };
                let plan = board_core::reconcile(&snapshot, &policy, &snapshot.clock).unwrap();
                assert_eq!(plan.actions.len(), usize::from(expected_complete));
            }
        }
    }
}
