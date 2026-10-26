//! GitHub Projects v2 read transport. Connections are exhausted, never inferred complete.
use super::Forge;
use crate::changes::{self, Changes};
use anyhow::{Context, Result, ensure};
use board_core::{Capability, Coverage, Evidence, Item, ItemRef, Snapshot};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant, SystemTime};
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

/// GitHub refused a request under a rate limit, primary or secondary, and
/// it was not waited out ([`Backoff`]); or a pass stopped by one did not
/// send it. It says why.
#[derive(Debug)]
pub struct RateLimited(pub String);

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RateLimited {}

pub trait Transport {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value>;
}

/// The async [`Transport`], which the snapshot reads through. Its futures
/// need not be `Send`: the CLI runs them on one current-thread runtime.
#[allow(async_fn_in_trait)]
pub trait GraphQl {
    async fn graphql(&self, query: &str, variables: Value) -> Result<Value>;
}

/// A sync [`Transport`] as a [`GraphQl`] one too, whose futures are ready
/// when first polled: for fakes, and for the sync paths that share the
/// async paging ([`ready`]).
pub struct Blocking<'a, T>(pub &'a T);

impl<T: Transport> Transport for Blocking<'_, T> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.0.graphql(query, variables)
    }
}

impl<T: Transport> GraphQl for Blocking<'_, T> {
    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.0.graphql(query, variables)
    }
}

/// The output of a future that never waits, as one reading only through
/// [`Blocking`] does; one that would wait is an error, not a hang.
fn ready<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    match std::pin::pin!(future).poll(&mut context) {
        std::task::Poll::Ready(output) => output,
        std::task::Poll::Pending => anyhow::bail!("a synchronous GitHub read had to wait"),
    }
}

/// Attach the operation and its fine-grained permission to live failures.
pub struct DiagnosticTransport<'a, T> {
    pub transport: &'a T,
    pub project: String,
    pub repository: String,
}

fn operation(query: &str) -> (&'static str, &'static str, bool) {
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
        // Before fieldValues: a snapshot page reads both, and is a membership read.
        (
            "items(first",
            "reading membership in project",
            "organization Projects read",
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
        let (context, permission) = self.explain(query, &variables);
        let result = self.transport.graphql(query, variables);
        result.map_err(|error| refusal(error, context, permission))
    }
}

impl<T: GraphQl> GraphQl for DiagnosticTransport<'_, T> {
    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let (context, permission) = self.explain(query, &variables);
        let result = self.transport.graphql(query, variables).await;
        result.map_err(|error| refusal(error, context, permission))
    }
}

/// `error` with `context`, and on a refusal the `permission` it needs.
fn refusal(error: anyhow::Error, context: String, permission: &str) -> anyhow::Error {
    let message = format!("{error:#}");
    let context = if !error.is::<RateLimited>()
        && ["FORBIDDEN", "HTTP status 403", "HTTP status 404"]
            .iter()
            .any(|s| message.contains(s))
    {
        format!(
            "{context}\nFine-grained token needs {permission}; an organization can also hold a fine-grained token pending approval."
        )
    } else {
        context
    };
    error.context(context)
}

impl<T> DiagnosticTransport<'_, T> {
    /// The step of `query` and its target, and the permission it needs.
    fn explain(&self, query: &str, variables: &Value) -> (String, &'static str) {
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
        (context, permission)
    }
}

/// The default request budget of a snapshot, which its caller passes, and of
/// the other whole-board reads. Shared across every connection and per-item
/// read in one read. Exhaustion returns an error, never a partial snapshot
/// that could yield proposals.
pub const SNAPSHOT_REQUEST_LIMIT: usize = 1_000;

// A snapshot page reads each item's field values, labels and assignees inline,
// so a board costs a request per page rather than three per item (#276).
// GitHub refuses a query that could return more than 500,000 nodes, counting
// every nested `first`: with 100 values nested in each field value, a page
// of 100 items asks for 509,100. Nesting 10 asks for 59,100 (a cost of 53
// points per page, measured on a 28-field board), and the rare item that
// has more is read again by itself, below, with the full page sizes.
const ITEM_PAGE: usize = 100;
/// Values per item read inline: a value per field set, so more than a board's
/// usual field count (GitHub's default fields included).
const VALUE_PAGE: usize = 50;
/// Users, labels, pull requests or reviewers in a field value, read inline.
const NESTED_VALUE_PAGE: usize = 10;
/// The content's labels and assignees, read inline.
const FACT_PAGE: usize = 20;
// GitHub's count for a page, as measured: the items, their labels and
// assignees, their values, and the entries nested in each value.
const _: () =
    assert!(ITEM_PAGE * (1 + 2 * FACT_PAGE + VALUE_PAGE * (1 + NESTED_VALUE_PAGE)) < 500_000);
/// The page size of every per-item follow-up read.
const FOLLOW_UP_PAGE: usize = 100;
/// Field values that hold a connection, which have no node ID of their own.
const NESTED_VALUES: [(&str, &str); 4] = [
    ("ProjectV2ItemFieldLabelValue", "labels"),
    ("ProjectV2ItemFieldUserValue", "users"),
    ("ProjectV2ItemFieldPullRequestValue", "pullRequests"),
    ("ProjectV2ItemFieldReviewerValue", "reviewers"),
];
/// The content's connections, with the field each node is read for.
const CONTENT_FACTS: [(&str, &str); 2] = [("labels", "name"), ("assignees", "login")];

/// The selection of an item's field values, nesting `nested` values in each.
fn field_values_selection(page: usize, nested: usize, after: &str) -> String {
    format!(
        "fieldValues(first:{page}{after}){{nodes{{__typename ... on ProjectV2ItemFieldSingleSelectValue{{name optionId field{{... on ProjectV2SingleSelectField{{name}}}}}} ... on ProjectV2ItemFieldTextValue{{text field{{... on ProjectV2Field{{name}}}}}} ... on ProjectV2ItemFieldNumberValue{{number field{{... on ProjectV2Field{{name}}}}}} ... on ProjectV2ItemFieldDateValue{{date field{{... on ProjectV2Field{{name}}}}}} ... on ProjectV2ItemFieldIterationValue{{title iterationId field{{... on ProjectV2IterationField{{name}}}}}} ... on ProjectV2ItemFieldRepositoryValue{{repository{{id nameWithOwner}} field{{... on ProjectV2Field{{name}}}}}} ... on ProjectV2ItemFieldMilestoneValue{{milestone{{id title state dueOn}} field{{... on ProjectV2Field{{name}}}}}} ... on ProjectV2ItemFieldLabelValue{{labels(first:{nested}){{nodes{{id name}} pageInfo{{hasNextPage}}}} field{{... on ProjectV2Field{{name}}}}}} ... on ProjectV2ItemFieldUserValue{{users(first:{nested}){{nodes{{id login}} pageInfo{{hasNextPage}}}} field{{... on ProjectV2Field{{name}}}}}} ... on ProjectV2ItemFieldPullRequestValue{{pullRequests(first:{nested}){{nodes{{id number url title state}} pageInfo{{hasNextPage}}}} field{{... on ProjectV2Field{{name}}}}}} ... on ProjectV2ItemFieldReviewerValue{{reviewers(first:{nested}){{nodes{{__typename ... on User{{id login}} ... on Team{{id name}}}} pageInfo{{hasNextPage}}}} field{{... on ProjectV2Field{{name}}}}}}}} pageInfo{{hasNextPage endCursor}}}}"
    )
}

/// The selection of one of the content's [`CONTENT_FACTS`].
fn content_fact_selection(field: &str, selection: &str, page: usize, after: &str) -> String {
    format!(
        "{field}(first:{page}{after}){{nodes{{{selection}}} pageInfo{{hasNextPage endCursor}}}}"
    )
}

/// One page of the board's items with everything the snapshot reads of each.
fn snapshot_items_query() -> String {
    let facts = CONTENT_FACTS
        .map(|(field, selection)| content_fact_selection(field, selection, FACT_PAGE, ""))
        .join(" ");
    let values = field_values_selection(VALUE_PAGE, NESTED_VALUE_PAGE, "");
    format!(
        "query($id:ID!,$cursor:String){{node(id:$id){{... on ProjectV2{{items(first:{ITEM_PAGE},after:$cursor){{nodes{{id content{{__typename ... on Issue{{id number title state stateReason createdAt updatedAt closedAt repository{{nameWithOwner}} {facts}}} ... on PullRequest{{id number title state createdAt updatedAt closedAt mergedAt repository{{nameWithOwner}} {facts}}}}} {values}}} pageInfo{{hasNextPage endCursor}}}}}}}}}}"
    )
}

/// All of one item's field values, for an item the page did not hold whole.
fn item_field_values_query() -> String {
    let values = field_values_selection(FOLLOW_UP_PAGE, FOLLOW_UP_PAGE, ",after:$cursor");
    format!("query($id:ID!,$cursor:String){{node(id:$id){{... on ProjectV2Item{{{values}}}}}}}")
}

/// The rest of the content's `field` after the page read inline.
fn content_fact_query(field: &str, selection: &str) -> String {
    let connection = content_fact_selection(field, selection, FOLLOW_UP_PAGE, ",after:$cursor");
    format!(
        "query($id:ID!,$cursor:String){{node(id:$id){{... on Issue{{{connection}}} ... on PullRequest{{{connection}}}}}}}"
    )
}

pub(crate) struct ReadBudget<'a, T> {
    transport: &'a T,
    remaining: std::cell::Cell<usize>,
}

impl<'a, T> ReadBudget<'a, T> {
    pub(crate) fn new(transport: &'a T, limit: usize) -> Self {
        Self {
            transport,
            remaining: std::cell::Cell::new(limit),
        }
    }

    fn spend(&self) -> Result<()> {
        let remaining = self.remaining.get();
        ensure!(
            remaining > 0,
            "GitHub snapshot request budget exhausted; no snapshot produced"
        );
        self.remaining.set(remaining - 1);
        Ok(())
    }
}

impl<T: Transport> Transport for ReadBudget<'_, T> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.spend()?;
        self.transport.graphql(query, variables)
    }
}

impl<T: GraphQl> GraphQl for ReadBudget<'_, T> {
    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.spend()?;
        self.transport.graphql(query, variables).await
    }
}

/// The live GitHub client: [`Api`] for the snapshot, and ureq for every
/// other read and write, which are still sync.
pub struct Client {
    token: String,
    agent: ureq::Agent,
    api: Api,
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

    /// The client with no retry under a rate limit waiting past `deadline`,
    /// as a pass's time bound ([`Backoff`]).
    pub fn retry_within(mut self, deadline: Instant) -> Self {
        self.api.deadline = Some(deadline);
        self
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
            api: Api::new(token.clone()),
            token,
            agent: http_agent(),
        }
    }
}

impl GraphQl for Client {
    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.api.graphql(query, variables).await
    }
}

const GRAPHQL_URL: &str = "https://api.github.com/graphql";

/// Requests one [`Api`] has in flight at once. GitHub counts concurrent
/// requests toward its secondary rate limits and asks clients to keep them
/// few; a snapshot reads its pages one after another today.
pub const MAX_IN_FLIGHT: usize = 4;

/// Retries of one request GitHub refused under a rate limit. They are the
/// client's, not one of an operation's budgeted requests.
pub const RATE_LIMIT_RETRIES: u32 = 3;

/// The first wait for a refusal that names none, doubled for each retry
/// after it: GitHub asks for at least a minute, backing off exponentially,
/// for a secondary rate limit.
pub const SECONDARY_WAIT: Duration = Duration::from_secs(60);

/// The longest one request's retries may wait together, the secondary waits
/// of every retry; a wait past it gives up at once. Per request, not per
/// operation: a read of many requests may wait this long for each, within
/// its client's deadline ([`Client::retry_within`]).
pub const RATE_LIMIT_WAIT: Duration =
    Duration::from_secs(SECONDARY_WAIT.as_secs() * ((1 << RATE_LIMIT_RETRIES) - 1));

/// The most of a response body read, as ureq's default for JSON.
const RESPONSE_BYTES: usize = 10 * 1024 * 1024;

/// One request's time bound, as [`http_agent`]'s.
const REQUEST_SECONDS: u64 = 30;

/// One HTTP response, as [`Api`] sees it.
#[derive(Clone)]
pub(crate) struct Reply {
    status: u16,
    limits: Limits,
    body: Vec<u8>,
}

/// A response's rate-limit headers.
#[derive(Clone, Debug, Default)]
pub(crate) struct Limits {
    retry_after: Option<String>,
    /// `x-ratelimit-remaining` and `x-ratelimit-reset`.
    remaining: Option<String>,
    reset: Option<String>,
}

impl Limits {
    /// The headers `header` looks up by their lower-case names.
    fn read(header: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            retry_after: header("retry-after"),
            remaining: header("x-ratelimit-remaining"),
            reset: header("x-ratelimit-reset"),
        }
    }

    /// Those of a ureq response.
    fn of(headers: &ureq::http::HeaderMap) -> Self {
        Self::read(|name| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        })
    }
}

/// One POST of JSON to the API: [`Https`] live, a fake in tests.
pub(crate) trait Http {
    async fn post(&self, url: &str, body: &[u8]) -> Result<Reply>;
}

/// reqwest with the token, its TLS client built at the first request.
pub(crate) struct Https {
    token: String,
    client: tokio::sync::OnceCell<reqwest::Client>,
}

/// rustls with ring, as ureq's, and the platform's trust store, which
/// honors locally installed CAs, including sandbox egress proxies.
fn https_client() -> Result<reqwest::Client> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_platform_verifier()
        .context("platform TLS verifier")?
        .with_no_client_auth();
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .timeout(Duration::from_secs(REQUEST_SECONDS))
        .user_agent("agent-board")
        .build()
        .context("build the GitHub HTTPS client")
}

impl Http for Https {
    async fn post(&self, url: &str, body: &[u8]) -> Result<Reply> {
        let client = self
            .client
            .get_or_try_init(|| async { https_client() })
            .await?;
        let failed = |_| anyhow::anyhow!("GitHub transport failed");
        let mut response = client
            .post(url)
            .bearer_auth(&self.token)
            .header("Content-Type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .map_err(failed)?;
        let limits = Limits::read(|name| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        });
        let status = response.status().as_u16();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(failed)? {
            ensure!(
                body.len() + chunk.len() <= RESPONSE_BYTES,
                "GitHub response over {RESPONSE_BYTES} bytes"
            );
            body.extend_from_slice(&chunk);
        }
        Ok(Reply {
            status,
            limits,
            body,
        })
    }
}

/// The async GitHub client, which holds its limits in one place: at most
/// [`MAX_IN_FLIGHT`] requests at once, and a request refused under a rate
/// limit retried at most [`RATE_LIMIT_RETRIES`] times within
/// [`RATE_LIMIT_WAIT`] ([`Backoff`]). An operation's request budget
/// is its own ([`GitHub::snapshot_with_policy`]).
pub(crate) struct Api<H = Https> {
    http: H,
    in_flight: tokio::sync::Semaphore,
    /// No retry waits past it ([`Client::retry_within`]).
    deadline: Option<Instant>,
}

impl Api {
    fn new(token: String) -> Self {
        Self::with_http(Https {
            token,
            client: tokio::sync::OnceCell::new(),
        })
    }
}

impl<H: Http> Api<H> {
    fn with_http(http: H) -> Self {
        Self {
            http,
            in_flight: tokio::sync::Semaphore::new(MAX_IN_FLIGHT),
            deadline: None,
        }
    }

    /// POST `body`, and again after each rate-limited refusal while the
    /// bounds allow. A request waiting out a limit holds no slot. The last
    /// reply comes back with why it was not retried, if it was refused.
    async fn post(&self, url: &str, body: &[u8]) -> Result<(Reply, Option<String>)> {
        let mut backoff = Backoff::new(self.deadline);
        loop {
            let reply = {
                let _slot = self.in_flight.acquire().await?;
                self.http.post(url, body).await?
            };
            let decoded = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
            let Some(wait) = rate_limited(reply.status, &reply.limits, &decoded, SystemTime::now())
            else {
                return Ok((reply, None));
            };
            match backoff.next(wait, tokio::time::Instant::now().into_std()) {
                Ok(wait) => tokio::time::sleep(wait).await,
                Err(why) => return Ok((reply, Some(why))),
            }
        }
    }
}

impl<H: Http> GraphQl for Api<H> {
    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let request = serde_json::to_vec(&json!({"query":query,"variables":variables}))?;
        let (reply, gave_up) = self.post(GRAPHQL_URL, &request).await?;
        let body: Value = match serde_json::from_slice(&reply.body) {
            Ok(body) => body,
            Err(_) if !(200..300).contains(&reply.status) => json!({}),
            Err(error) => return Err(error).context("decode GitHub GraphQL response"),
        };
        http_response_data(reply.status, body).map_err(|error| given_up(error, gave_up))
    }
}

/// A refusal given up on keeps today's error, and says why, as a
/// [`RateLimited`] one.
fn given_up(error: anyhow::Error, why: Option<String>) -> anyhow::Error {
    match why {
        Some(why) => error.context(RateLimited(why)),
        None => error,
    }
}

/// How long GitHub asks a request refused under a rate limit to wait.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Wait {
    /// By `Retry-After`, or by `x-ratelimit-reset` with no requests left.
    Asked(Duration),
    /// It does not say, as for most secondary rate limits.
    Unsaid,
}

/// Whether a GraphQL error is a rate limit's: `RATE_LIMITED` for the
/// hourly points, `RATE_LIMIT` (`graphql_rate_limit`) for a secondary one.
fn rate_limit_error(error: &Value) -> bool {
    ["RATE_LIMITED", "RATE_LIMIT"].contains(&error["type"].as_str().unwrap_or_default())
        || error["code"] == "graphql_rate_limit"
}

/// Whether a refusal's REST message names a rate limit, as GitHub's for a
/// primary and a secondary one do, where a permission refusal does not.
fn rate_limit_message(body: &Value) -> bool {
    body["message"]
        .as_str()
        .is_some_and(|m| m.to_ascii_lowercase().contains("rate limit"))
}

/// Whether GitHub refused a response under a rate limit, and how long it
/// asks to wait if so: a 429; a 403 that asks to wait, has no requests
/// left or names a rate limit; or GraphQL rate-limit errors without data,
/// as GitHub answers a secondary limit with HTTP 200. A GraphQL response
/// with data was answered, and a 403 that is none of these is a refusal:
/// neither is retried.
fn rate_limited(status: u16, limits: &Limits, body: &Value, now: SystemTime) -> Option<Wait> {
    let seconds = |value: &Option<String>| value.as_deref()?.trim().parse::<u64>().ok();
    let after = seconds(&limits.retry_after);
    let exhausted = seconds(&limits.remaining) == Some(0);
    let graphql = body["data"].is_null()
        && body["errors"]
            .as_array()
            .is_some_and(|errors| errors.iter().any(rate_limit_error));
    let refused = match status {
        429 => true,
        403 => graphql || after.is_some() || exhausted || rate_limit_message(body),
        _ => graphql,
    };
    if !refused {
        return None;
    }
    if let Some(after) = after {
        return Some(Wait::Asked(Duration::from_secs(after)));
    }
    let reset = seconds(&limits.reset).filter(|_| exhausted);
    let now = now.duration_since(std::time::UNIX_EPOCH).ok();
    Some(match reset.zip(now) {
        // A reset already past waits a second rather than none.
        Some((reset, now)) => Wait::Asked(Duration::from_secs(
            reset.saturating_sub(now.as_secs()).max(1),
        )),
        None => Wait::Unsaid,
    })
}

/// The waits of one request's retries under a rate limit: what GitHub asks,
/// or else [`SECONDARY_WAIT`] doubled for each retry before; at most
/// [`RATE_LIMIT_RETRIES`], [`RATE_LIMIT_WAIT`] together, and none past the
/// deadline. Every transport path retries by it.
struct Backoff {
    retries: u32,
    waited: Duration,
    deadline: Option<Instant>,
}

impl Backoff {
    fn new(deadline: Option<Instant>) -> Self {
        Self {
            retries: 0,
            waited: Duration::ZERO,
            deadline,
        }
    }

    /// How long to wait at `now` before retrying a refusal that asks for
    /// `wait`, or why it is not retried.
    fn next(&mut self, wait: Wait, now: Instant) -> std::result::Result<Duration, String> {
        let (wait, asked) = match wait {
            Wait::Asked(wait) => (wait, "asked to wait"),
            Wait::Unsaid => {
                let doubled = 1u32.checked_shl(self.retries).unwrap_or(u32::MAX);
                (SECONDARY_WAIT.saturating_mul(doubled), "backing off")
            }
        };
        let total = self.waited.saturating_add(wait);
        let late = self
            .deadline
            .is_some_and(|deadline| now.checked_add(wait).is_none_or(|at| at > deadline));
        if self.retries >= RATE_LIMIT_RETRIES || total > RATE_LIMIT_WAIT || late {
            let past = if late { ", past the time bound" } else { "" };
            return Err(format!(
                "GitHub rate limit: gave up after {} retries and {}s of waiting, {asked} {}s more{past}",
                self.retries,
                self.waited.as_secs(),
                wait.as_secs()
            ));
        }
        self.retries += 1;
        self.waited = total;
        Ok(wait)
    }
}

/// `send` again after each refusal under a rate limit, sleeping between by
/// `sleep`, while `backoff` allows: the sync transport paths' [`Api::post`].
/// The last answer comes back with why it was not retried, if it was
/// refused.
fn retrying<T>(
    mut backoff: Backoff,
    mut send: impl FnMut() -> Result<(T, Option<Wait>)>,
    mut sleep: impl FnMut(Duration),
) -> Result<(T, Option<String>)> {
    loop {
        let (answer, wait) = send()?;
        let Some(wait) = wait else {
            return Ok((answer, None));
        };
        match backoff.next(wait, Instant::now()) {
            Ok(wait) => sleep(wait),
            Err(why) => return Ok((answer, Some(why))),
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
    if response["errors"]
        .as_array()
        .is_some_and(|errors| errors.iter().any(rate_limit_error))
    {
        let why = format!("GitHub GraphQL: {details}; response may be partial");
        return Err(RateLimited(why).into());
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
        let request = json!({"query":query,"variables":variables});
        let send = || {
            let mut response = self
                .agent
                .post(GRAPHQL_URL)
                .header("Authorization", &format!("Bearer {}", self.token))
                .header("User-Agent", "agent-board")
                .send_json(&request)
                .map_err(transport_error)?;
            let status = response.status().as_u16();
            let limits = Limits::of(response.headers());
            let body: Value = match response.body_mut().read_json() {
                Ok(body) => body,
                Err(_) if !(200..300).contains(&status) => json!({}),
                Err(error) => return Err(error).context("decode GitHub GraphQL response"),
            };
            let wait = rate_limited(status, &limits, &body, SystemTime::now());
            Ok(((status, body), wait))
        };
        let ((status, body), gave_up) = retrying(self.backoff(), send, std::thread::sleep)?;
        http_response_data(status, body).map_err(|error| given_up(error, gave_up))
    }
}

impl Client {
    fn backoff(&self) -> Backoff {
        Backoff::new(self.api.deadline)
    }
}

/// The broker answers in well under a kilobyte; this is a bound, not a size.
const BROKER_BYTES: u64 = 256 * 1024;
const BROKER_SECONDS: u64 = 10;

impl crate::pass::Broker for Client {
    fn usage(&self, url: &str) -> Result<Value> {
        self.broker_get(url, "/usage")
    }

    fn grant(&self, url: &str) -> Result<Value> {
        self.broker_get(url, "/usage/grant")
    }
}

impl Client {
    /// The broker's answer at `url`, read without the token, which is
    /// GitHub's alone; the errors name `path`, not the address, which may be
    /// private.
    fn broker_get(&self, url: &str, path: &str) -> Result<Value> {
        let mut response = self
            .agent
            .get(url)
            .header("User-Agent", "agent-board")
            .config()
            .timeout_global(Some(std::time::Duration::from_secs(BROKER_SECONDS)))
            // The address may be private: not to a proxy from the environment.
            .proxy(None)
            .build()
            .call()
            .map_err(|error| match error {
                // It would repeat the address.
                ureq::Error::BadUri(_) => anyhow::anyhow!("GET {path}: invalid address"),
                error => anyhow::anyhow!("GET {path}: {error}"),
            })?;
        let status = response.status().as_u16();
        ensure!(status == 200, "GET {path}: HTTP status {status}");
        response
            .body_mut()
            .with_config()
            .limit(BROKER_BYTES)
            .read_json()
            .with_context(|| format!("GET {path}: not JSON, or over {BROKER_BYTES} bytes"))
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
        let send = || {
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
            let limits = Limits::of(response.headers());
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
            let wait = rate_limited(status, &limits, &body, SystemTime::now());
            Ok(((status, body), wait))
        };
        let ((status, body), gave_up) = retrying(self.backoff(), send, std::thread::sleep)?;
        if let Some(why) = gave_up {
            let message = body["message"].as_str().unwrap_or("message unavailable");
            return Err(
                anyhow::anyhow!("{method} {path}: GitHub HTTP status {status}: {message}")
                    .context(RateLimited(why)),
            );
        }
        Ok((status, body))
    }

    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<crate::poll::RestResponse> {
        // A `Link` header or a cached URL must not carry the token elsewhere.
        ensure!(
            url.starts_with(API_ROOT),
            "refusing to send the token outside {API_ROOT}"
        );
        let path = url.trim_start_matches(API_ROOT);
        let send = || {
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
            // Only a refusal's body says whether it is a rate limit's.
            let wait = matches!(status, 403 | 429).then(|| {
                let body = response.body_mut().read_json().unwrap_or(Value::Null);
                let limits = Limits::of(response.headers());
                (
                    rate_limited(status, &limits, &body, SystemTime::now()),
                    body,
                )
            });
            let (wait, refusal) = wait.unzip();
            Ok(((response, refusal), wait.flatten()))
        };
        let ((mut response, refusal), gave_up) =
            retrying(self.backoff(), send, std::thread::sleep)?;
        let status = response.status().as_u16();
        if let Some(why) = gave_up {
            let body = refusal.unwrap_or_default();
            let message = body["message"].as_str().unwrap_or("message unavailable");
            return Err(
                anyhow::anyhow!("GET {path}: GitHub HTTP status {status}: {message}")
                    .context(RateLimited(why)),
            );
        }
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let (etag, last_modified) = (header("etag"), header("last-modified"));
        let link = header("link");
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
        if status == 429 || (status == 403 && rate_limit_message(&body)) {
            return Err(RateLimited(diagnostic).into());
        }
        anyhow::bail!("{diagnostic}");
    }
    response_data(body)
}

/// Node IDs for opening an issue, resolved by `GitHub::issue_target`.
pub struct IssueTarget {
    repository: String,
    repository_id: String,
    labels: Vec<String>,
    label_ids: Vec<String>,
    assignee: String,
    assignee_id: String,
}

/// An issue as `GitHub::issue_facts` reads it.
pub struct IssueFacts {
    pub id: String,
    pub open: bool,
    pub labels: Vec<String>,
    pub body: String,
}

/// A `GitHub::open_board_issues` entry: the issue and its body.
pub type BoardIssue = (ItemRef, String);

/// Labels `GitHub::create_issue` applies and checks in one receipt.
const ISSUE_LABELS_MAX: usize = 10;

pub struct GitHub<T> {
    pub transport: T,
    pub owner: String,
    pub number: u64,
}

pub(crate) fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("missing GitHub {key}"))
}

/// [`connection`] through a sync transport.
pub(crate) fn sync_connection(
    t: &impl Transport,
    query: &str,
    variables: Value,
    pointer: &str,
) -> Result<(Vec<Value>, bool)> {
    ready(connection(&Blocking(t), query, variables, pointer))
}

pub(crate) async fn connection(
    t: &impl GraphQl,
    query: &str,
    variables: Value,
    pointer: &str,
) -> Result<(Vec<Value>, bool)> {
    continue_connection(t, None, query, variables, pointer).await
}

/// Collect a connection from its `first` page, if already read, then read the
/// rest with `query` from its cursor. Only a page that says it is the last one
/// is complete.
async fn continue_connection(
    t: &impl GraphQl,
    first: Option<&Value>,
    query: &str,
    mut variables: Value,
    pointer: &str,
) -> Result<(Vec<Value>, bool)> {
    let mut nodes = Vec::new();
    let mut cursors = BTreeSet::new();
    let mut complete = true;
    let mut conn = match first {
        Some(first) => first.clone(),
        None => read_page(t, query, &variables, pointer).await?,
    };
    loop {
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
        conn = read_page(t, query, &variables, pointer).await?;
    }
}

async fn read_page(
    t: &impl GraphQl,
    query: &str,
    variables: &Value,
    pointer: &str,
) -> Result<Value> {
    t.graphql(query, variables.clone())
        .await?
        .pointer_mut(pointer)
        .map(Value::take)
        .context("missing GitHub connection")
}

/// The connection a field value holds, if it is one of [`NESTED_VALUES`].
fn nested_value_field(value: &Value) -> Option<&'static str> {
    let kind = value["__typename"].as_str()?;
    NESTED_VALUES
        .iter()
        .find(|(ty, _)| *ty == kind)
        .map(|(_, field)| *field)
}

/// An item's field values: the page's, when it held all of them whole, or else
/// all of them read again by themselves, which nests a full page in each.
async fn item_field_values(t: &impl GraphQl, member: &Value) -> Result<(Vec<Value>, bool)> {
    let inline = &member["fieldValues"];
    let truncated = inline["pageInfo"]["hasNextPage"] == true
        || inline["nodes"].as_array().is_some_and(|values| {
            values.iter().any(|value| {
                nested_value_field(value)
                    .is_some_and(|field| value[field]["pageInfo"]["hasNextPage"] == true)
            })
        });
    let query = item_field_values_query();
    let variables = json!({"id":text(member, "id")?});
    if truncated {
        connection(t, &query, variables, "/node/fieldValues").await
    } else {
        continue_connection(t, Some(inline), &query, variables, "/node/fieldValues").await
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
async fn read_schema(
    t: &impl GraphQl,
    owner: &str,
    number: u64,
) -> Result<(Value, Vec<Value>, bool)> {
    let data = t.graphql("query($owner:String!,$number:Int!){repositoryOwner(login:$owner){... on Organization{projectV2(number:$number){id url}} ... on User{projectV2(number:$number){id url}}}}", json!({"owner":owner,"number":number})).await?;
    let project = data["repositoryOwner"]["projectV2"].clone();
    let (fields, complete) = connection(
        t,
        "query($id:ID!,$cursor:String){node(id:$id){... on ProjectV2{fields(first:100,after:$cursor){nodes{... on ProjectV2Field{id name dataType} ... on ProjectV2SingleSelectField{id name dataType options{id name}} ... on ProjectV2IterationField{id name dataType configuration{iterations{id title startDate duration} completedIterations{id title startDate duration}}}} pageInfo{hasNextPage endCursor}}}}}",
        json!({"id":text(&project, "id")?}),
        "/node/fields",
    )
    .await?;
    Ok((project, fields, complete))
}

impl<T: GraphQl> Forge for GitHub<T> {
    fn capabilities(&self) -> Result<Vec<Capability>> {
        // Read access never establishes mutation authority.
        Ok(vec![])
    }

    async fn snapshot(&self) -> Result<Snapshot> {
        let policy = board_core::AgentPolicy::parse(board_core::DEFAULT_AGENT_POLICY)?;
        self.snapshot_with_policy(&policy, SNAPSHOT_REQUEST_LIMIT)
            .await
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

impl<T: Transport> GitHub<T> {
    /// The project's changes since `since`, read with the snapshot's request
    /// budget; new pull requests outside the board are listed when the token's
    /// viewer or `operator` wrote them. `public_only` leaves out private
    /// repositories ([`changes::read`]).
    pub fn changes(
        &self,
        since: time::OffsetDateTime,
        until: Option<time::OffsetDateTime>,
        operator: Option<&str>,
        public_only: bool,
    ) -> Result<Changes> {
        let (id, _, _) = self.schema()?;
        changes::read(&self.diagnostic(), &id, since, until, operator, public_only)
    }

    /// Explicit operator publication, not delegated output application.
    /// `operator` is as for [`Self::changes`], when the body is generated.
    pub fn status_update(
        &self,
        status: &str,
        body: Option<&str>,
        operator: Option<&str>,
    ) -> Result<String> {
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
                let since = match changes::last_update(&diagnostic, &id)? {
                    Some(since) => since,
                    None => time::OffsetDateTime::now_utc() - changes::FIRST_WINDOW,
                };
                generated = changes::read(&diagnostic, &id, since, None, operator, false)?
                    .status_body(None, &BTreeMap::new(), STATUS_BODY_LIMIT)?;
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

    /// The project's node ID, URL and complete field schema, without reading items.
    /// The returned URL must name the requested project before anything writes to its ID.
    pub fn schema(&self) -> Result<(String, Url, Vec<Value>)> {
        ensure!(self.number > 0, "project number must be positive");
        let diagnostic = self.diagnostic();
        let transport = ReadBudget::new(&diagnostic, SNAPSHOT_REQUEST_LIMIT);
        let (project, fields, complete) =
            ready(read_schema(&Blocking(&transport), &self.owner, self.number))?;
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

    /// The node ID of an existing label. A missing label is refused rather than
    /// created, as `field_writes` refuses new fields.
    pub fn label_id(&self, repository: &str, label: &str) -> Result<String> {
        let (owner, name) = repository
            .split_once('/')
            .context("GitHub repositories require OWNER/REPO")?;
        let data = self.repository_diagnostic(repository).graphql(
            "query($owner:String!,$repo:String!,$label:String!){repository(owner:$owner,name:$repo){nameWithOwner label(name:$label){id name}}}",
            json!({"owner":owner,"repo":name,"label":label}),
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
        Ok(text(&repo["label"], "id")?.to_owned())
    }

    /// Resolve where an issue with these labels and one assignee can be opened,
    /// before anything is written. Each label must exist ([`Self::label_id`]), and
    /// the assignee must be assignable there: GitHub silently drops anyone else,
    /// which would leave an unassigned issue behind.
    pub fn issue_target(
        &self,
        repository: &str,
        labels: &[&str],
        assignee: &str,
    ) -> Result<IssueTarget> {
        let (owner, name) = repository
            .split_once('/')
            .context("GitHub repositories require OWNER/REPO")?;
        ensure!(
            is_github_login(assignee),
            "invalid GitHub login {assignee:?}"
        );
        ensure!(
            (1..=ISSUE_LABELS_MAX).contains(&labels.len()),
            "an issue is opened with 1 to {ISSUE_LABELS_MAX} labels"
        );
        let data = self.repository_diagnostic(repository).graphql(
            "query($owner:String!,$repo:String!,$login:String!){repository(owner:$owner,name:$repo){id nameWithOwner assignableUsers(query:$login,first:100){nodes{id login}}}}",
            json!({"owner":owner,"repo":name,"login":assignee}),
        )?;
        let repo = &data["repository"];
        ensure!(
            repo["nameWithOwner"]
                .as_str()
                .is_some_and(|r| r.eq_ignore_ascii_case(repository)),
            "GitHub returned a different repository than {repository}"
        );
        let label_ids = labels
            .iter()
            .map(|label| self.label_id(repository, label))
            .collect::<Result<_>>()?;
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
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            label_ids,
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
            // One more label than the most applied, so an extra one shows in the receipt.
            "mutation($repo:ID!,$title:String!,$body:String!,$labels:[ID!]!,$user:ID!){createIssue(input:{repositoryId:$repo,title:$title,body:$body,labelIds:$labels,assigneeIds:[$user]}){issue{id number labels(first:11){nodes{name}} assignees(first:2){nodes{login}}}}}",
            json!({"repo":target.repository_id,"title":title,"body":body,"labels":target.label_ids,"user":target.assignee_id}),
        )?;
        let issue = &created["createIssue"]["issue"];
        let number = issue["number"]
            .as_u64()
            .context("missing created issue number")?;
        let names = |key: &str, field: &str| -> Vec<String> {
            let mut names: Vec<_> = issue[key]["nodes"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|n| n[field].as_str().map(str::to_lowercase))
                .collect();
            names.sort();
            names
        };
        let mut labels: Vec<_> = target.labels.iter().map(|l| l.to_lowercase()).collect();
        labels.sort();
        ensure!(
            names("labels", "name") == labels
                && names("assignees", "login") == [target.assignee.to_lowercase()],
            "opened {}#{number}, but GitHub did not apply labels {} and assignee {}",
            target.repository,
            target.labels.join(", "),
            target.assignee
        );
        Ok((text(issue, "id")?.to_owned(), number))
    }

    /// The issue's node ID, whether it is open, its label names and its body. A
    /// truncated label list is refused: an unseen page could hold the label a caller
    /// requires.
    pub fn issue_facts(&self, issue: &ItemRef) -> Result<IssueFacts> {
        ensure!(issue.host == DEFAULT_HOST, "unsupported item host");
        let (owner, name) = issue
            .repository
            .split_once('/')
            .context("GitHub repositories require OWNER/REPO")?;
        let data = self.repository_diagnostic(&issue.repository).graphql(
            "query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,name:$repo){issue(number:$number){id number state body repository{nameWithOwner} labels(first:100){nodes{name} pageInfo{hasNextPage}}}}}",
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
        Ok(IssueFacts {
            id: text(found, "id")?.to_owned(),
            open: text(found, "state")? == "OPEN",
            labels: names,
            body: text(found, "body")?.to_owned(),
        })
    }

    /// Add a label to an issue, or with `present` false remove it, by node IDs, and
    /// check the receipt names the issue.
    pub fn set_label(
        &self,
        repository: &str,
        issue: &str,
        label: &str,
        present: bool,
    ) -> Result<()> {
        let mutation = if present {
            "addLabelsToLabelable"
        } else {
            "removeLabelsFromLabelable"
        };
        let data = self.repository_diagnostic(repository).graphql(
            &format!("mutation($id:ID!,$label:ID!){{{mutation}(input:{{labelableId:$id,labelIds:[$label]}}){{labelable{{... on Issue{{id}}}}}}}}"),
            json!({"id":issue,"label":label}),
        )?;
        ensure!(
            data[mutation]["labelable"]["id"].as_str() == Some(issue),
            "label receipt mismatch"
        );
        Ok(())
    }

    /// The open issues on `project` labelled `label`, with their bodies, and whether
    /// every item was read: unlike search, which may lag a just-opened issue, the
    /// board is read directly. An item whose content cannot be seen, or whose labels
    /// were not all read and do not include `label`, makes the list incomplete.
    pub fn open_board_issues(&self, project: &str, label: &str) -> Result<(Vec<BoardIssue>, bool)> {
        let diagnostic = self.diagnostic();
        let transport = ReadBudget::new(&diagnostic, SNAPSHOT_REQUEST_LIMIT);
        let (members, mut complete) = sync_connection(
            &transport,
            &format!(
                "query($id:ID!,$cursor:String){{node(id:$id){{... on ProjectV2{{items(first:{ITEM_PAGE},after:$cursor){{nodes{{content{{__typename ... on Issue{{number state body repository{{nameWithOwner}} labels(first:{FOLLOW_UP_PAGE}){{nodes{{name}} pageInfo{{hasNextPage}}}}}}}}}} pageInfo{{hasNextPage endCursor}}}}}}}}}}"
            ),
            json!({"id":project}),
            "/node/items",
        )?;
        let mut issues = Vec::new();
        for member in members {
            let content = &member["content"];
            if content.is_null() {
                complete = false;
                continue;
            }
            if content["__typename"] != "Issue" || content["state"] != "OPEN" {
                continue;
            }
            let labels = &content["labels"];
            if !labels["nodes"]
                .as_array()
                .context("missing issue labels")?
                .iter()
                .any(|n| n["name"] == label)
            {
                complete &= labels["pageInfo"]["hasNextPage"] == false;
                continue;
            }
            let identity = ItemRef {
                host: DEFAULT_HOST.into(),
                repository: text(&content["repository"], "nameWithOwner")?.into(),
                number: content["number"]
                    .as_u64()
                    .context("missing content number")?,
            };
            issues.push((identity, text(content, "body")?.to_owned()));
        }
        Ok((issues, complete))
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
}

impl<T> GitHub<T> {
    fn diagnostic(&self) -> DiagnosticTransport<'_, T> {
        DiagnosticTransport {
            transport: &self.transport,
            project: format!("{}/{}", self.owner, self.number),
            repository: String::new(),
        }
    }
}

impl<T: GraphQl> GitHub<T> {
    /// The whole board in at most `budget` requests: running out is an
    /// error, never a partial snapshot. [`SNAPSHOT_REQUEST_LIMIT`] is the
    /// default.
    pub async fn snapshot_with_policy(
        &self,
        policy: &board_core::AgentPolicy,
        budget: usize,
    ) -> Result<Snapshot> {
        ensure!(self.number > 0, "project number must be positive");
        let diagnostic = self.diagnostic();
        let transport = ReadBudget::new(&diagnostic, budget);
        let (project, fields, mut complete) =
            read_schema(&transport, &self.owner, self.number).await?;
        let id = text(&project, "id")?;
        let clock = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)?;
        let (members, members_complete) = connection(
            &transport,
            &snapshot_items_query(),
            json!({"id":id}),
            "/node/items",
        )
        .await?;
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
            let (values, values_complete) = item_field_values(&transport, &member).await?;
            let mut item_fields = BTreeMap::new();
            let mut values_supported = true;
            for mut value in values {
                let nested = nested_value_field(&value);
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
            for (field, selection) in CONTENT_FACTS {
                // The page read the first labels and assignees; follow the
                // cursor only for an item that has more.
                let (nodes, full) = continue_connection(
                    &transport,
                    Some(&content[field]),
                    &content_fact_query(field, selection),
                    json!({"id":content_id}),
                    &format!("/node/{field}"),
                )
                .await?;
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
                project_item: Some(text(&member, "id")?.to_owned()),
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
            status_field_absent: false,
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
        // reqwest takes the preconfigured rustls only of its own version.
        https_client().unwrap();
    }

    /// A [`Http`] that answers from a script, and counts what it is sent
    /// and how many requests were in flight at once. Each takes a second.
    #[derive(Default)]
    struct Scripted {
        replies: RefCell<VecDeque<Reply>>,
        posts: std::cell::Cell<usize>,
        in_flight: std::cell::Cell<usize>,
        peak: std::cell::Cell<usize>,
    }

    impl Http for Scripted {
        async fn post(&self, _: &str, _: &[u8]) -> Result<Reply> {
            self.posts.set(self.posts.get() + 1);
            self.in_flight.set(self.in_flight.get() + 1);
            self.peak.set(self.peak.get().max(self.in_flight.get()));
            tokio::time::sleep(Duration::from_secs(1)).await;
            self.in_flight.set(self.in_flight.get() - 1);
            Ok(self.replies.borrow_mut().pop_front().unwrap_or(Reply {
                status: 200,
                limits: Limits::default(),
                body: br#"{"data":{"ok":true}}"#.to_vec(),
            }))
        }
    }

    /// A REST secondary rate limit, a permission refusal, and GitHub's
    /// GraphQL secondary rate limit, which it answers with HTTP 200 and
    /// without `Retry-After`.
    const SECONDARY: &str = r#"{"message":"You have exceeded a secondary rate limit"}"#;
    const PERMISSION: &str = r#"{"message":"Resource not accessible by integration"}"#;
    const GRAPHQL_LIMIT: &str = r#"{"errors":[{"type":"RATE_LIMIT","code":"graphql_rate_limit","message":"API rate limit already exceeded for user ID 1."}]}"#;

    /// A response with `Retry-After`, the rate-limit headers and `body`.
    fn reply(status: u16, retry_after: Option<&str>, remaining: Option<&str>, body: &str) -> Reply {
        Reply {
            status,
            limits: Limits {
                retry_after: retry_after.map(str::to_owned),
                remaining: remaining.map(str::to_owned),
                reset: Some("1000090".into()),
            },
            body: body.as_bytes().to_vec(),
        }
    }

    /// Each response shape: whether it is refused under a rate limit and
    /// what wait it asks, and whether its error is a [`RateLimited`] one.
    #[test]
    fn rate_limit_classification_table() {
        // The reset above is 90 seconds after this.
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let asked = |seconds| Some(Wait::Asked(Duration::from_secs(seconds)));
        let unsaid = Some(Wait::Unsaid);
        for (reply, expected, typed) in [
            (
                reply(200, None, None, r#"{"data":{"ok":true}}"#),
                None,
                false,
            ),
            // GraphQL's secondary limit, with points left: the 26 Oct pass.
            (reply(200, None, Some("4350"), GRAPHQL_LIMIT), unsaid, true),
            (reply(200, None, Some("0"), GRAPHQL_LIMIT), asked(90), true),
            (
                reply(200, None, None, r#"{"errors":[{"type":"RATE_LIMITED"}]}"#),
                unsaid,
                true,
            ),
            // Answered with data: not retried, as a mutation may have applied.
            (
                reply(
                    200,
                    None,
                    None,
                    r#"{"data":{},"errors":[{"type":"RATE_LIMIT"}]}"#,
                ),
                None,
                true,
            ),
            (reply(500, Some("30"), Some("0"), SECONDARY), None, false),
            (reply(403, Some("30"), None, PERMISSION), asked(30), false),
            (reply(429, Some("0"), None, SECONDARY), asked(0), true),
            (
                reply(429, Some(" 7 "), Some("0"), SECONDARY),
                asked(7),
                true,
            ),
            // Without Retry-After, the reset when no requests remain.
            (reply(403, None, Some("0"), PERMISSION), asked(90), false),
            (
                reply(429, Some("soon"), Some("0"), SECONDARY),
                asked(90),
                true,
            ),
            // Without either, a secondary limit's message, or any 429.
            (reply(403, None, None, SECONDARY), unsaid, true),
            (
                reply(
                    403,
                    None,
                    Some("12"),
                    r#"{"message":"API rate limit exceeded"}"#,
                ),
                unsaid,
                true,
            ),
            (reply(429, None, None, ""), unsaid, true),
            // A 403 that is not a rate limit is a refusal, not retried.
            (reply(403, None, None, PERMISSION), None, false),
            (reply(403, None, Some("12"), PERMISSION), None, false),
            (reply(401, Some("5"), None, SECONDARY), None, false),
        ] {
            let body = serde_json::from_slice(&reply.body).unwrap_or(json!({}));
            let context = (reply.status, reply.limits.clone(), body.clone());
            let wait = rate_limited(reply.status, &reply.limits, &body, now);
            assert_eq!(wait, expected, "{context:?}");
            let result = http_response_data(reply.status, body);
            assert_eq!(
                result.is_err_and(|e| e.is::<RateLimited>()),
                typed,
                "{context:?}"
            );
        }
        let past = std::time::UNIX_EPOCH + Duration::from_secs(2_000_000);
        let reply = reply(403, None, Some("0"), PERMISSION);
        let wait = rate_limited(reply.status, &reply.limits, &Value::Null, past);
        assert_eq!(wait, Some(Wait::Asked(Duration::from_secs(1))));
    }

    /// The waits [`Backoff`] gives a run of refusals, by a clock that moves
    /// only by them, with an optional deadline that many seconds away.
    #[test]
    fn backoff_table() {
        use Wait::{Asked, Unsaid};
        let secs = Duration::from_secs;
        type Case = (
            Option<u64>,
            Vec<Wait>,
            Vec<std::result::Result<u64, &'static str>>,
        );
        let cases: Vec<Case> = vec![
            // At least a minute, doubled for each retry.
            (
                None,
                vec![Unsaid; 4],
                vec![
                    Ok(60),
                    Ok(120),
                    Ok(240),
                    Err("gave up after 3 retries and 420s of waiting, backing off 480s more"),
                ],
            ),
            (None, vec![Asked(secs(10)), Unsaid], vec![Ok(10), Ok(120)]),
            (
                None,
                vec![Asked(secs(3600))],
                vec![Err(
                    "gave up after 0 retries and 0s of waiting, asked to wait 3600s more",
                )],
            ),
            (
                None,
                vec![Asked(secs(400)), Asked(secs(30))],
                vec![Ok(400), Err("400s of waiting, asked to wait 30s more")],
            ),
            // Never past the deadline.
            (
                Some(90),
                vec![Unsaid; 2],
                vec![Ok(60), Err("backing off 120s more, past the time bound")],
            ),
            (
                Some(30),
                vec![Asked(secs(10)), Asked(secs(30))],
                vec![Ok(10), Err("asked to wait 30s more, past the time bound")],
            ),
        ];
        for (deadline, waits, expected) in cases {
            let start = Instant::now();
            let mut backoff = Backoff::new(deadline.map(|d| start + secs(d)));
            let mut now = start;
            let got: Vec<_> = waits
                .into_iter()
                .map(|wait| {
                    let next = backoff.next(wait, now);
                    now += next.clone().unwrap_or_default();
                    next
                })
                .collect();
            assert_eq!(got.len(), expected.len());
            for (got, expected) in got.iter().zip(&expected) {
                match (got, expected) {
                    (Ok(got), Ok(expected)) => assert_eq!(got.as_secs(), *expected),
                    (Err(got), Err(expected)) => assert!(got.contains(expected), "{got}"),
                    _ => panic!("{deadline:?}: {got:?}, expected {expected:?}"),
                }
            }
        }
    }

    /// The sync paths' retry sleeps through the sleep it is given.
    #[test]
    fn sync_retry_table() {
        let (asked, unsaid) = (
            Some(Wait::Asked(Duration::from_secs(5))),
            Some(Wait::Unsaid),
        );
        for (script, slept, gave_up) in [
            (vec![None], vec![], false),
            (vec![asked, None], vec![5], false),
            (vec![unsaid, unsaid, None], vec![60, 120], false),
            (vec![unsaid; 4], vec![60, 120, 240], true),
        ] {
            let sends = script.len();
            let mut script = script.into_iter();
            let mut sent = 0;
            let mut sleeps = vec![];
            let send = || {
                sent += 1;
                Ok((sent, script.next().flatten()))
            };
            let (last, why) =
                retrying(Backoff::new(None), send, |wait| sleeps.push(wait.as_secs())).unwrap();
            assert_eq!(last, sends, "{slept:?}");
            assert_eq!(sleeps, slept);
            assert_eq!(why.is_some(), gave_up, "{why:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limit_retry_table() {
        let limited = |after: &str| reply(429, Some(after), None, SECONDARY);
        let graphql = || reply(200, None, Some("4350"), GRAPHQL_LIMIT);
        // The script and the deadline, if any, in seconds; then the
        // requests made, the time waited between them and what the read
        // ends with.
        for (script, deadline, posts, waited, expected) in [
            (vec![], None, 1, 0, "ok"),
            (vec![limited("10")], None, 2, 10, "ok"),
            (
                vec![reply(403, Some("30"), None, PERMISSION); 2],
                None,
                3,
                60,
                "ok",
            ),
            // The reset, long past by the wall clock, waits a second.
            (
                vec![reply(403, None, Some("0"), PERMISSION)],
                None,
                2,
                1,
                "ok",
            ),
            // A refusal that asks no wait waits a minute, then two.
            (vec![reply(403, None, None, SECONDARY)], None, 2, 60, "ok"),
            (vec![graphql(); 2], None, 3, 180, "ok"),
            // The retry bound: the last refusal is not waited out.
            (
                vec![limited("1"); RATE_LIMIT_RETRIES as usize + 1],
                None,
                RATE_LIMIT_RETRIES as usize + 1,
                u64::from(RATE_LIMIT_RETRIES),
                "gave up after 3 retries",
            ),
            (
                vec![graphql(); RATE_LIMIT_RETRIES as usize + 1],
                None,
                RATE_LIMIT_RETRIES as usize + 1,
                RATE_LIMIT_WAIT.as_secs(),
                "gave up after 3 retries and 420s of waiting, backing off 480s more: GitHub GraphQL: RATE_LIMIT",
            ),
            // The wait bound: a wait past it gives up at once, with
            // today's error for the refusal.
            (
                vec![limited("3600")],
                None,
                1,
                0,
                "gave up after 0 retries and 0s of waiting, asked to wait 3600s more: GitHub HTTP status 429: You have exceeded",
            ),
            (
                vec![limited("400"), reply(403, Some("30"), None, PERMISSION)],
                None,
                2,
                400,
                "gave up after 1 retries and 400s of waiting, asked to wait 30s more: GitHub HTTP status 403",
            ),
            (
                vec![limited("10"), limited(&u64::MAX.to_string())],
                None,
                2,
                10,
                "gave up after 1 retries",
            ),
            // The deadline: the second wait would pass it.
            (
                vec![graphql(); 2],
                Some(90),
                2,
                60,
                "backing off 120s more, past the time bound",
            ),
            // Not a rate limit: today's errors, without a retry.
            (
                vec![reply(403, None, None, PERMISSION)],
                None,
                1,
                0,
                "GitHub HTTP status 403",
            ),
            (
                vec![reply(401, Some("5"), None, SECONDARY)],
                None,
                1,
                0,
                "authentication",
            ),
        ] {
            let mut api = Api::with_http(Scripted {
                replies: RefCell::new(script.into()),
                ..Default::default()
            });
            let start = tokio::time::Instant::now();
            api.deadline = deadline.map(|d| start.into_std() + Duration::from_secs(d));
            let result = api.graphql("query", json!({})).await;
            let outcome = match &result {
                Ok(data) => {
                    assert_eq!(data, &json!({"ok": true}));
                    "ok".to_owned()
                }
                Err(error) => format!("{error:#}"),
            };
            assert!(outcome.contains(expected), "{expected}: {outcome}");
            assert_eq!(
                result
                    .as_ref()
                    .is_err_and(|e| e.is::<AuthenticationRequired>()),
                expected == "authentication"
            );
            assert_eq!(
                result.is_err_and(|e| e.is::<RateLimited>()),
                outcome.contains("gave up"),
                "{expected}"
            );
            assert_eq!(api.http.posts.get(), posts, "{expected}");
            // Each request takes a second of the paused clock.
            let elapsed = start.elapsed().as_secs();
            assert_eq!(elapsed, waited + posts as u64, "{expected}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn in_flight_cap_table() {
        for requests in [1, MAX_IN_FLIGHT, MAX_IN_FLIGHT + 1, 3 * MAX_IN_FLIGHT] {
            let api = Api::with_http(Scripted::default());
            let start = tokio::time::Instant::now();
            let mut reads: Vec<_> = (0..requests)
                .map(|_| Box::pin(api.graphql("query", json!({}))))
                .collect();
            // Every read polled together on this task, as a join would.
            std::future::poll_fn(|cx| {
                reads.retain_mut(|read| match read.as_mut().poll(cx) {
                    std::task::Poll::Ready(result) => {
                        result.unwrap();
                        false
                    }
                    std::task::Poll::Pending => true,
                });
                if reads.is_empty() {
                    std::task::Poll::Ready(())
                } else {
                    std::task::Poll::Pending
                }
            })
            .await;
            assert_eq!(api.http.posts.get(), requests);
            assert_eq!(api.http.peak.get(), requests.min(MAX_IN_FLIGHT));
            // A second per round of at most MAX_IN_FLIGHT requests.
            let rounds = requests.div_ceil(MAX_IN_FLIGHT) as u64;
            assert_eq!(start.elapsed().as_secs(), rounds, "{requests}");
        }
    }

    struct Recorded {
        responses: RefCell<VecDeque<Value>>,
        variables: RefCell<Vec<Value>>,
    }

    impl Transport for Recorded {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            if query.contains("fieldValues(first") {
                // A page nests a few values; a follow-up read nests a full page.
                let nested = if query.contains("items(first") {
                    NESTED_VALUE_PAGE
                } else {
                    FOLLOW_UP_PAGE
                };
                for (ty, field) in NESTED_VALUES {
                    assert!(query.contains(&format!("... on {ty}{{{field}(first:{nested})")));
                }
            }
            self.variables.borrow_mut().push(variables);
            self.responses
                .borrow_mut()
                .pop_front()
                .context("unexpected request")
        }
    }

    impl GraphQl for Recorded {
        async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            Transport::graphql(self, query, variables)
        }
    }

    fn page(nodes: Value, next: bool, cursor: Value) -> Value {
        json!({"node":{"connection":{"nodes":nodes,"pageInfo":{"hasNextPage":next,"endCursor":cursor}}}})
    }

    /// A connection page that is the last one.
    fn last(nodes: Value) -> Value {
        json!({"nodes":nodes,"pageInfo":{"hasNextPage":false,"endCursor":null}})
    }

    /// A response holding the last page of the node's connection `name`.
    fn conn(name: &str, nodes: Value) -> Value {
        json!({"node":{name:last(nodes)}})
    }

    /// The project and its field schema, the first two reads of a snapshot.
    fn schema(fields: Value) -> [Value; 2] {
        [
            json!({"repositoryOwner":{"projectV2":{"id":"P","url":"https://github.com/orgs/example/projects/1"}}}),
            conn("fields", fields),
        ]
    }

    /// A board item as a snapshot page holds it, its connections whole.
    fn member(
        id: &str,
        mut content: Value,
        values: Value,
        labels: Value,
        assignees: Value,
    ) -> Value {
        content["labels"] = last(labels);
        content["assignees"] = last(assignees);
        json!({"id":id,"content":content,"fieldValues":last(values)})
    }

    /// An open issue's content, as a snapshot page reads it.
    fn issue(number: u64) -> Value {
        json!({"__typename":"Issue","id":format!("I{number}"),"number":number,"title":"recorded","state":"OPEN","stateReason":null,"repository":{"nameWithOwner":"example/intake"},"createdAt":"2026-10-08T00:00:00Z","updatedAt":"2026-10-08T00:00:00Z","closedAt":null})
    }

    fn recorded(responses: impl IntoIterator<Item = Value>) -> GitHub<Recorded> {
        GitHub {
            transport: Recorded {
                responses: RefCell::new(responses.into_iter().collect()),
                variables: RefCell::new(vec![]),
            },
            owner: "example".into(),
            number: 1,
        }
    }

    #[test]
    fn status_publication_validates_before_writing() {
        for status in ["on-track", "at-risk", "off-track"] {
            let native = status.replace('-', "_").to_uppercase();
            let github = recorded([
                json!({"repositoryOwner":{"projectV2":{"id":"P","url":"https://github.com/orgs/example/projects/1"}}}),
                json!({"node":{"fields":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}),
                json!({"createProjectV2StatusUpdate":{"statusUpdate":{"id":"U","body":"Report","status":native}}}),
            ]);
            assert_eq!(
                github.status_update(status, Some("Report"), None).unwrap(),
                "U"
            );
            assert_eq!(
                github.transport.variables.borrow()[2],
                json!({"project":"P","body":"Report","status":native})
            );
            for body in [
                "".to_owned(),
                " ".to_owned(),
                "x".repeat(STATUS_BODY_LIMIT + 1),
            ] {
                assert!(github.status_update(status, Some(&body), None).is_err());
            }
            assert!(
                github
                    .status_update("unknown", Some("Report"), None)
                    .is_err()
            );
            assert!(validate_status_body(&"é".repeat(STATUS_BODY_LIMIT)).is_ok());
            assert_eq!(github.transport.variables.borrow().len(), 3);
        }
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
            let body = "No changes since 2026-10-02T00:00:00Z\n";
            let github = recorded([
                json!({"repositoryOwner":{"projectV2":{"id":"P","url":url}}}),
                json!({"node":{"fields":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}),
                json!({"node":{"statusUpdates":{"nodes":[{"createdAt":"2026-10-02T00:00:00Z"},{"createdAt":"2026-10-01T00:00:00Z"}],"pageInfo":{"hasNextPage":false}}}}),
                json!({"viewer":{"login":"worker"}}),
                json!({"node":{"items":{"nodes":if case == "redacted" {json!([{"content":null}])} else {json!([])},"pageInfo":{"hasNextPage":case == "incomplete","endCursor":null}}}}),
                json!({"createProjectV2StatusUpdate":{"statusUpdate":{"id":"U","body":if case == "receipt" {"wrong"} else {body},"status":"ON_TRACK"}}}),
            ]);
            let result = github.status_update("on-track", None, None);
            assert_eq!(result.is_ok(), case == "valid", "{case}: {result:?}");
            let count = github.transport.variables.borrow().len();
            assert_eq!(
                count,
                match case {
                    "wrong-project" => 2,
                    "incomplete" | "redacted" => 5,
                    _ => 6,
                }
            );
            if case == "valid" {
                let variables = github.transport.variables.borrow();
                // The window starts at the latest update, not the first listed.
                assert_eq!(variables[4]["since"], "2026-10-02T00:00:00Z");
                assert_eq!(variables[5]["body"], body);
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

    #[tokio::test]
    async fn pagination_table() {
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
                connection(&t, "recorded", json!({"id":"project"}), "/node/connection")
                    .await
                    .unwrap();
            assert_eq!(complete, expected_complete);
            assert_eq!(nodes.len(), count);
            assert_eq!(t.variables.borrow().len(), requests);
            if requests == 2 {
                assert_eq!(t.variables.borrow()[1]["cursor"], "next");
            }
        }
    }

    #[tokio::test]
    async fn aggregate_request_budget_table() {
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
            let result = connection(&budget, "recorded", json!({}), "/node/connection").await;
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
            let another = connection(&budget, "another", json!({}), "/node/connection");
            assert!(another.await.is_err());
            // The sync paths share the paging and the budget.
            let sync = sync_connection(&budget, "another", json!({}), "/node/connection");
            assert!(sync.is_err());
            assert_eq!(t.variables.borrow().len(), limit);
        }
    }

    #[tokio::test]
    async fn snapshot_unique_cursor_exhaustion() {
        // The default budget, and a smaller one the caller passes.
        for budget in [SNAPSHOT_REQUEST_LIMIT, 5] {
            let mut responses = vec![json!({"repositoryOwner":{"projectV2":{"id":"P"}}})];
            for i in 1..SNAPSHOT_REQUEST_LIMIT {
                responses.push(json!({"node":{"fields":{"nodes":[],"pageInfo":{"hasNextPage":true,"endCursor":format!("cursor-{i}")}}}}));
            }
            let github = recorded(responses);
            let policy = board_core::AgentPolicy::parse(board_core::DEFAULT_AGENT_POLICY).unwrap();
            let read = github.snapshot_with_policy(&policy, budget).await;
            let error = read.unwrap_err();
            assert!(error.to_string().contains("request budget exhausted"));
            assert_eq!(github.transport.variables.borrow().len(), budget);
        }
    }

    #[tokio::test]
    async fn malformed_pagination_table() {
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
            let read = connection(&t, "recorded", json!({}), "/node/connection");
            assert!(read.await.is_err());
        }
    }

    #[tokio::test]
    async fn redacted_membership_blocks_reconcile() {
        let snapshot = recorded([
            json!({"repositoryOwner":{"projectV2":{"id":"P","url":"https://github.com/orgs/example/projects/1"}}}),
            json!({"node":{"fields":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}),
            json!({"node":{"items":{"nodes":[{"id":"M","content":null}],"pageInfo":{"hasNextPage":false}}}}),
        ])
        .snapshot()
        .await
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

    #[tokio::test]
    async fn issue_and_pull_request_facts_table() {
        for (kind, state, reason, repairs) in [
            ("Issue", "OPEN", None, 0),
            ("Issue", "CLOSED", None, 0),
            ("Issue", "CLOSED", Some("COMPLETED"), 0),
            ("Issue", "CLOSED", Some("NOT_PLANNED"), 1),
            ("Issue", "CLOSED", Some("OTHER"), 0),
            ("PullRequest", "OPEN", None, 0),
            ("PullRequest", "MERGED", None, 0),
        ] {
            let content = json!({"__typename":kind,"id":"I","number":7,"title":"recorded","state":state,"stateReason":reason,"repository":{"nameWithOwner":"example/intake"},"createdAt":"2026-10-08T00:00:00Z","updatedAt":"2026-10-08T00:00:00Z","closedAt":null,"mergedAt":null});
            let values = json!([{"__typename":"ProjectV2ItemFieldSingleSelectValue","name":"Done","optionId":"done","field":{"name":"Status"}}]);
            let [project, fields] = schema(
                json!([{"name":"Status","dataType":"SINGLE_SELECT","options":[{"id":"done","name":"Done"},{"id":"cancelled","name":"Cancelled"}]}]),
            );
            let items = conn(
                "items",
                json!([member(
                    "M",
                    content,
                    values,
                    json!([{"name":"bug"}]),
                    json!([{"login":"human"}])
                )]),
            );
            let github = recorded([project, fields, items]);
            let snapshot = github.snapshot().await.unwrap();
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

    #[tokio::test]
    async fn agent_assignment_reader_table() {
        for (name, valid) in [
            (None, true),
            (Some("rust-worker"), true),
            (Some("custom-worker"), true),
            (Some("unknown"), false),
        ] {
            let values = name.map(|name| vec![json!({"__typename":"ProjectV2ItemFieldSingleSelectValue", "name":name, "optionId":"agent", "field":{"name":"Agent"}})]).unwrap_or_default();
            let [project, fields] = schema(json!([
                {"name":"Status","dataType":"SINGLE_SELECT","options":[]},
                {"name":"Agent","dataType":"SINGLE_SELECT","options":[{"id":"agent","name":name.unwrap_or("rust-worker")}]}
            ]));
            let items = conn(
                "items",
                json!([member("M", issue(7), json!(values), json!([]), json!([]))]),
            );
            let github = recorded([project, fields, items]);
            let policy = board_core::AgentPolicy::parse(&format!(
                "{}\n[agents.custom-worker]\nenabled = true\nfields = []\noutputs = []",
                board_core::DEFAULT_AGENT_POLICY
            ))
            .unwrap();
            let result = github
                .snapshot_with_policy(&policy, SNAPSHOT_REQUEST_LIMIT)
                .await;
            if valid {
                assert_eq!(result.unwrap().items[0].assignment.as_deref(), name);
            } else {
                assert!(format!("{:#}", result.unwrap_err()).contains("unknown Agent value"));
            }
        }
    }

    #[tokio::test]
    async fn board_pages_table() {
        const ITEMS: u64 = 250;
        // Item 7's connection that is longer than the page holds, and its
        // follow-up read: the request it makes and the response it gets.
        let label = |name: &str| json!({"name":name});
        let more = json!({"hasNextPage":true,"endCursor":"c"});
        let values = json!([{"__typename":"ProjectV2ItemFieldSingleSelectValue","name":"Done","optionId":"done","field":{"name":"Status"}}]);
        let label_value = |next: bool| json!([{"__typename":"ProjectV2ItemFieldLabelValue","field":{"name":"Labels"},"labels":{"nodes":[{"id":"L","name":"bug"}],"pageInfo":{"hasNextPage":next}}}]);
        // What of item 7 is longer than the page holds, and the follow-up
        // read that brings the rest: the request it makes and its response.
        let cases = [
            ("whole", None, None, true),
            (
                "more values",
                Some(("/fieldValues/pageInfo", more.clone())),
                Some((json!({"id":"M7"}), conn("fieldValues", values.clone()))),
                true,
            ),
            (
                "nested values",
                Some(("/fieldValues/nodes", label_value(true))),
                Some((json!({"id":"M7"}), conn("fieldValues", label_value(false)))),
                true,
            ),
            (
                "nested values still truncated",
                Some(("/fieldValues/nodes", label_value(true))),
                Some((json!({"id":"M7"}), conn("fieldValues", label_value(true)))),
                false,
            ),
            (
                "more labels",
                Some(("/content/labels/pageInfo", more.clone())),
                Some((
                    json!({"id":"I7","cursor":"c"}),
                    conn("labels", json!([label("more")])),
                )),
                true,
            ),
            (
                "more assignees",
                Some(("/content/assignees/pageInfo", more.clone())),
                Some((
                    json!({"id":"I7","cursor":"c"}),
                    conn("assignees", json!([{"login":"other"}])),
                )),
                true,
            ),
        ];
        for (case, change, follow_up, complete) in cases {
            let [project, fields] = schema(
                json!([{"name":"Status","dataType":"SINGLE_SELECT","options":[{"id":"done","name":"Done"}]}]),
            );
            let mut members: Vec<Value> = (1..=ITEMS)
                .map(|n| {
                    member(
                        &format!("M{n}"),
                        issue(n),
                        values.clone(),
                        json!([label("bug")]),
                        json!([]),
                    )
                })
                .collect();
            if let Some((pointer, value)) = change {
                *members[6].pointer_mut(pointer).unwrap() = value;
            }
            let pages = members.chunks(ITEM_PAGE).enumerate().map(|(i, nodes)| {
                let next = (i + 1) * ITEM_PAGE < members.len();
                json!({"node":{"items":{"nodes":nodes,"pageInfo":{"hasNextPage":next,"endCursor":format!("page-{i}")}}}})
            });
            let mut responses: Vec<Value> = [project, fields].into_iter().chain(pages).collect();
            let page_requests = responses.len() - 2;
            assert_eq!(page_requests, 3);
            responses.extend(follow_up.iter().map(|(_, response)| response.clone()));
            let github = recorded(responses);
            let snapshot = github.snapshot().await.unwrap();
            let requests = github.transport.variables.borrow().clone();
            assert_eq!(snapshot.items.len(), ITEMS as usize, "{case}");
            assert_eq!(snapshot.coverage.complete, complete, "{case}");
            assert_eq!(
                requests.len(),
                2 + page_requests + usize::from(follow_up.is_some()),
                "{case}"
            );
            for (i, request) in requests[2..2 + page_requests].iter().enumerate() {
                let cursor = i.checked_sub(1).map(|i| format!("page-{i}"));
                assert_eq!(request["cursor"], json!(cursor), "{case}");
            }
            if let Some((request, _)) = &follow_up {
                assert_eq!(requests.last(), Some(request), "{case}");
            }
            let item = &snapshot.items[6];
            assert_eq!(item.identity.number, 7);
            assert_eq!(item.fields_complete, complete, "{case}");
            match case {
                "more labels" => assert_eq!(item.labels, ["bug", "more"]),
                "more assignees" => assert_eq!(item.assignees, ["other"]),
                _ => assert_eq!(item.labels, ["bug"]),
            }
        }
    }

    #[tokio::test]
    async fn inline_field_connections_table() {
        // API-shaped synthetic responses: these four value types have no id.
        let fixture: Vec<Value> =
            serde_json::from_str(include_str!("../../../fixtures/github-field-values.json"))
                .unwrap();
        for (_, field) in NESTED_VALUES {
            // `follow_up`: the values an item truncated in the page is read
            // again with, nesting a full page; truncated there, it stays so.
            for (case, follow_up, expected_complete) in [
                ("complete", None, true),
                ("empty", None, true),
                ("truncated", Some("complete"), true),
                ("truncated", Some("truncated"), false),
                ("redacted", None, false),
                ("malformed_object", None, false),
                ("missing_fact", None, false),
                ("missing_nodes", None, false),
                ("missing_page_info", None, false),
            ] {
                let values = |case: &str| {
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
                    json!(values)
                };
                let [project, fields] = schema(
                    json!([{"name":"Status","dataType":"SINGLE_SELECT","options":[{"id":"triage","name":"Triage"}]}]),
                );
                let items = conn(
                    "items",
                    json!([member("M", issue(7), values(case), json!([]), json!([]))]),
                );
                let github = recorded(
                    [project, fields, items]
                        .into_iter()
                        .chain(follow_up.map(|case| conn("fieldValues", values(case)))),
                );
                let snapshot = github.snapshot().await.unwrap();
                let context = format!("{field}/{case}/{follow_up:?}");
                assert_eq!(snapshot.coverage.complete, expected_complete, "{context}");
                assert_eq!(snapshot.items[0].fields_complete, expected_complete);
                let requests = github.transport.variables.borrow().clone();
                assert_eq!(
                    requests.len(),
                    3 + usize::from(follow_up.is_some()),
                    "{context}"
                );
                if follow_up.is_some() {
                    assert_eq!(requests[3], json!({"id":"M"}));
                }
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
