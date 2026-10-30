//! Read-only Forgejo board. Forgejo's v1 API has no projects, so the board is
//! the issues of configured repositories: Priority and Turn come from scoped
//! labels, Run and Result from marker comments. Nothing here writes to Forgejo.
use anyhow::{Context, Result, bail, ensure};
use board_core::{Coverage, Evidence, Item, ItemRef, Priority, Snapshot, Turn};
use serde::Deserialize;
use serde_json::Value;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

/// Exactly one label of each of these scopes per item; zero or two leave the
/// item's fields incomplete. Forgejo's exclusive flag is not relied on: a
/// label created without it would let two through unnoticed.
pub const PRIORITY_SCOPE: &str = "priority/";
pub const TURN_SCOPE: &str = "turn/";
/// Any number of `blocked/<reason>` labels, read into the item's Blocked field.
pub const BLOCKED_SCOPE: &str = "blocked/";
/// A comment's first line `<!-- agent-board-run: URL -->` (or `-result:`)
/// records the item's Run (or Result); the newest such comment wins.
pub const RUN_MARKER: &str = "agent-board-run:";
pub const RESULT_MARKER: &str = "agent-board-result:";
pub const DEFAULT_TOKEN_ENV: &str = "FORGEJO_TOKEN";
/// Forgejo's default maximum page size; a smaller server maximum is still
/// read completely because paging follows `X-Total-Count`.
const PAGE_LIMIT: &str = "50";
// Shared by every request in one snapshot. Exhaustion is an error, never a
// partial snapshot; it also stops a server that ignores `page` from looping.
const SNAPSHOT_REQUEST_LIMIT: usize = 1_000;
/// The GitHub token variables, which must not be sent to another host.
const GITHUB_TOKEN_ENVS: [&str; 2] = ["GH_TOKEN", "GITHUB_TOKEN"];
/// Names that resolve only inside a cluster or local network, so plain HTTP
/// to them does not warn.
const LOCAL_NAME_SUFFIXES: [&str; 4] = [".svc", ".cluster.local", ".local", ".internal"];

/// The `board` entry of the caller's trusted agent-board.json. Absent, the
/// board is a GitHub project chosen by `--owner`/`--project`, as before.
#[derive(Debug, Deserialize)]
#[serde(tag = "forge", rename_all = "lowercase", deny_unknown_fields)]
pub enum Board {
    Github {},
    Forgejo(Config),
}

impl Board {
    pub fn from_config(value: &Value) -> Result<Self> {
        crate::poll::check_config(value)?;
        let Some(board) = value.get("board") else {
            return Ok(Self::Github {});
        };
        let board = Self::deserialize(board).context("decode board config")?;
        if let Self::Forgejo(config) = &board {
            config.validate()?;
        }
        Ok(board)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    /// The Forgejo instance, for example `https://forge.example/` or
    /// `http://forgejo-http.forgejo.svc:3000/`.
    pub url: Url,
    /// The environment variable holding a token that can read the issues.
    #[serde(default = "default_token_env")]
    pub token_env: String,
    /// `OWNER/REPO` repositories whose issues make up the board.
    pub repositories: Vec<String>,
    pub membership: Membership,
    /// User ids whose marker comments count. Anyone who can comment on an
    /// issue could otherwise set its Run or Result, and a login can be
    /// renamed and taken over, so ids; with none, comments are not read.
    #[serde(default)]
    pub recorders: Vec<u64>,
}

fn default_token_env() -> String {
    DEFAULT_TOKEN_ENV.into()
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum Membership {
    /// Every open issue of the repositories.
    Open,
    /// Every issue, open or closed, carrying this label.
    Label(String),
}

fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

impl Config {
    pub(crate) fn validate(&self) -> Result<()> {
        let url = &self.url;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "board url must be a canonical HTTP(S) URL without credentials, query or fragment"
        );
        ensure!(
            !self.token_env.is_empty() && !GITHUB_TOKEN_ENVS.contains(&self.token_env.as_str()),
            "board tokenEnv must name a Forgejo token variable, not empty or a GitHub token"
        );
        ensure!(
            !self.repositories.is_empty(),
            "board repositories must not be empty"
        );
        let mut seen = BTreeSet::new();
        for repository in &self.repositories {
            let parts: Vec<_> = repository.split('/').collect();
            ensure!(
                parts.len() == 2 && parts.iter().all(|part| is_name(part)),
                "invalid board repository: {repository}"
            );
            ensure!(
                seen.insert(repository.to_ascii_lowercase()),
                "duplicate board repository: {repository}"
            );
        }
        if let Membership::Label(label) = &self.membership {
            // The issues API takes a comma-separated label list.
            // A field label would also count towards its own scope.
            ensure!(
                !label.trim().is_empty()
                    && !label.contains(',')
                    && ![PRIORITY_SCOPE, TURN_SCOPE, BLOCKED_SCOPE]
                        .iter()
                        .any(|scope| label.starts_with(scope)),
                "invalid board membership label"
            );
        }
        ensure!(
            self.recorders.iter().all(|id| *id > 0),
            "invalid board recorder id"
        );
        Ok(())
    }

    /// A warning for a plain `http://` URL to a host that is not loopback,
    /// a private address or a cluster or local name: anyone on the path to
    /// it can read the token sent with each request.
    pub fn plain_http_warning(&self) -> Option<String> {
        if self.url.scheme() != "http" {
            return None;
        }
        let local = match self.url.host()? {
            url::Host::Ipv4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
            url::Host::Ipv6(ip) => {
                ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
            }
            url::Host::Domain(name) => {
                let name = name.trim_end_matches('.');
                !name.contains('.')
                    || LOCAL_NAME_SUFFIXES
                        .iter()
                        .any(|suffix| name.ends_with(suffix))
            }
        };
        if local {
            return None;
        }
        Some(format!(
            "Warning: board url {} is plain HTTP to a non-local host; the token is sent unencrypted",
            self.url
        ))
    }

    /// The instance URL as a directory, so joins keep a subpath install.
    pub(crate) fn base(&self) -> Url {
        let mut base = self.url.clone();
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        base
    }

    pub(crate) fn api_root(&self) -> Result<Url> {
        Ok(self.base().join("api/v1/")?)
    }
}

/// A Forgejo token was not provided or was refused.
#[derive(Debug)]
pub struct AuthenticationRequired(pub String);

impl std::fmt::Display for AuthenticationRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "authentication required: provide a valid Forgejo token in {} (repository Issues read)",
            self.0
        )
    }
}

impl std::error::Error for AuthenticationRequired {}

pub struct Response {
    pub body: Value,
    /// The listing's `X-Total-Count`, where the server sent one.
    pub total: Option<u64>,
}

pub trait Transport {
    /// GET a JSON body from a URL under the API root.
    fn get(&self, url: &Url) -> Result<Response>;

    /// Mutation is opt-in; existing read transports cannot accidentally write.
    fn send(&self, _method: &str, _url: &Url, _body: &Value) -> Result<(u16, Value)> {
        bail!("Forgejo transport does not support writes")
    }
}

pub struct Client {
    api: Url,
    token: String,
    token_env: String,
    agent: ureq::Agent,
}

impl Client {
    pub fn from_env(config: &Config) -> Result<Self> {
        config.validate()?;
        let token = std::env::var(&config.token_env)
            .ok()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| AuthenticationRequired(config.token_env.clone()))?;
        Ok(Self {
            api: config.api_root()?,
            token,
            token_env: config.token_env.clone(),
            agent: crate::github::http_agent(),
        })
    }
}

impl Transport for Client {
    fn send(&self, method: &str, url: &Url, body: &Value) -> Result<(u16, Value)> {
        ensure!(
            url.as_str().starts_with(self.api.as_str()),
            "refusing Forgejo token outside configured API root"
        );
        ensure!(
            matches!(method, "POST" | "PUT"),
            "unsupported Forgejo method"
        );
        let request = match method {
            "POST" => self.agent.post(url.as_str()),
            "PUT" => self.agent.put(url.as_str()),
            _ => bail!("unsupported Forgejo method"),
        };
        let mut response = request
            .header("Authorization", &format!("token {}", self.token))
            .header("Accept", "application/json")
            .header("User-Agent", "agent-board")
            .config()
            .http_status_as_error(false)
            .max_redirects(0)
            .build()
            .send_json(body)
            .map_err(|_| anyhow::anyhow!("Forgejo mutation reply lost; do not retry"))?;
        let status = response.status().as_u16();
        let body = if status == 204 {
            Value::Null
        } else {
            response
                .body_mut()
                .read_json()
                .context("decode Forgejo mutation reply; do not retry")?
        };
        Ok((status, body))
    }

    fn get(&self, url: &Url) -> Result<Response> {
        ensure!(
            url.as_str().starts_with(self.api.as_str()),
            "refusing to send the token outside {}",
            self.api
        );
        let path = &url.as_str()[self.api.as_str().len()..];
        let mut response = self
            .agent
            .get(url.as_str())
            .header("Authorization", &format!("token {}", self.token))
            .header("Accept", "application/json")
            .header("User-Agent", "agent-board")
            // A redirect could carry the token to another origin.
            .config()
            .max_redirects(0)
            .build()
            .call()
            .map_err(|_| anyhow::anyhow!("GET {path}: Forgejo transport failed"))?;
        match response.status().as_u16() {
            200..300 => {
                let total = response
                    .headers()
                    .get("x-total-count")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok());
                let body = response
                    .body_mut()
                    .read_json()
                    .with_context(|| format!("decode GET {path}"))?;
                Ok(Response { body, total })
            }
            401 | 403 => Err(
                anyhow::Error::from(AuthenticationRequired(self.token_env.clone()))
                    .context(format!("GET {path}: Forgejo refused the token")),
            ),
            status => bail!("GET {path}: Forgejo HTTP status {status}"),
        }
    }
}

pub struct Forgejo<T> {
    pub transport: T,
    pub config: Config,
}

impl<T: Transport> Forgejo<T> {
    pub fn snapshot_with_policy(&self, policy: &board_core::AgentPolicy) -> Result<Snapshot> {
        let clock = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)?;
        self.snapshot_at(clock, policy)
    }

    fn get(&self, budget: &Cell<usize>, url: &Url) -> Result<(Vec<Value>, Option<u64>)> {
        let remaining = budget.get();
        ensure!(
            remaining > 0,
            "Forgejo snapshot request budget exhausted; no snapshot produced"
        );
        budget.set(remaining - 1);
        let response = self.transport.get(url)?;
        let items = response
            .body
            .as_array()
            .context("Forgejo listing is not an array")?;
        Ok((items.clone(), response.total))
    }

    /// Every page of a listing, and whether it read as many items as one
    /// `X-Total-Count` that every page reported. Offset pages shift when
    /// issues change mid-read; this catches an issue lost to a shift, not
    /// one removal and one addition that cancel out.
    fn pages(&self, budget: &Cell<usize>, mut url: Url) -> Result<(Vec<Value>, bool)> {
        let base: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        let mut all = Vec::new();
        let mut totals = BTreeSet::new();
        for page in 1.. {
            url.query_pairs_mut()
                .clear()
                .extend_pairs(&base)
                .append_pair("limit", PAGE_LIMIT)
                .append_pair("page", &page.to_string());
            let (items, total) = self.get(budget, &url)?;
            totals.insert(total);
            all.extend(items.iter().cloned());
            if items.is_empty() || total.is_some_and(|total| all.len() as u64 >= total) {
                break;
            }
        }
        let complete = totals.len() == 1 && totals.first() == Some(&Some(all.len() as u64));
        Ok((all, complete))
    }

    fn snapshot_at(&self, clock: String, policy: &board_core::AgentPolicy) -> Result<Snapshot> {
        let config = &self.config;
        config.validate()?;
        let api = config.api_root()?;
        let host = config.url.host_str().context("board url has no host")?;
        let budget = Cell::new(SNAPSHOT_REQUEST_LIMIT);
        let mut items = Vec::new();
        let mut complete = true;
        for repository in &config.repositories {
            let mut url = api.join(&format!("repos/{repository}/issues"))?;
            url.query_pairs_mut().append_pair("type", "issues");
            match &config.membership {
                Membership::Open => url.query_pairs_mut().append_pair("state", "open"),
                Membership::Label(label) => url
                    .query_pairs_mut()
                    .append_pair("state", "all")
                    .append_pair("labels", label),
            };
            let (issues, listed) = self.pages(&budget, url)?;
            complete &= listed;
            let mut seen = BTreeSet::new();
            for issue in issues {
                let number = issue["number"].as_u64().context("missing issue number")?;
                // A repeated number means pages overlapped, so others may be missing.
                ensure!(
                    seen.insert(number),
                    "{repository}#{number} listed twice; no snapshot produced"
                );
                let count = issue["comments"]
                    .as_u64()
                    .context("missing Forgejo comment count")?;
                // The comments endpoint is not paginated: one request returns
                // the whole thread, checked against the issue's count.
                let comments = if config.recorders.is_empty() || count == 0 {
                    None
                } else {
                    let url = api.join(&format!("repos/{repository}/issues/{number}/comments"))?;
                    Some(self.get(&budget, &url)?.0)
                };
                items.push(item(
                    host,
                    repository,
                    &issue,
                    comments.as_deref().map(|c| (c, count)),
                    config,
                    policy,
                    &clock,
                )?);
            }
        }
        let mut snapshot = Snapshot {
            schema: board_core::SNAPSHOT_SCHEMA.into(),
            project: config.base(),
            clock: clock.clone(),
            // Membership only. Label errors stay per item so reconcile can
            // name them instead of blocking the whole board.
            coverage: Coverage {
                complete,
                observed_at: clock,
            },
            items,
            fields: vec![],
            // Forgejo has no project board here, only labels.
            status_field_absent: true,
        };
        snapshot.items.sort_by(|a, b| a.identity.cmp(&b.identity));
        snapshot.validate()?;
        policy.normalize(&mut snapshot)?;
        Ok(snapshot)
    }
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("missing Forgejo {key}"))
}

fn item(
    host: &str,
    repository: &str,
    issue: &Value,
    // The thread read and the issue's comment count, when comments were read.
    comments: Option<(&[Value], u64)>,
    config: &Config,
    policy: &board_core::AgentPolicy,
    clock: &str,
) -> Result<Item> {
    let number = issue["number"].as_u64().context("missing issue number")?;
    // Issue versus pull request identity is a boundary: `type=issues` must hold.
    ensure!(
        issue["pull_request"].is_null(),
        "{repository}#{number} is a pull request"
    );
    let listed = text(&issue["repository"], "full_name")?;
    ensure!(
        listed.eq_ignore_ascii_case(repository),
        "{repository}#{number} reported from {listed}"
    );
    let state = text(issue, "state")?;
    ensure!(
        matches!(state, "open" | "closed"),
        "unknown Forgejo issue state"
    );
    let labels = issue["labels"]
        .as_array()
        .context("missing Forgejo labels")?
        .iter()
        .map(|label| text(label, "name").map(str::to_owned))
        .collect::<Result<Vec<_>>>()?;
    if let Membership::Label(member) = &config.membership {
        ensure!(
            labels.contains(member),
            "{repository}#{number} lacks the membership label"
        );
    }
    let mut errors = Vec::new();
    let priority = exclusive(&labels, PRIORITY_SCOPE, &mut errors, |value| {
        serde_json::from_value::<Priority>(value.into()).ok()
    });
    let turn = exclusive(&labels, TURN_SCOPE, &mut errors, |value| {
        match value.to_ascii_lowercase().as_str() {
            "coordinator" => Some(Turn::Coordinator),
            "worker" => Some(Turn::Worker),
            "operator" => Some(Turn::Operator),
            "external" => Some(Turn::External),
            "none" => Some(Turn::None),
            // `turn/<identity>` hands the item to that policy identity. Which
            // identity is not yet an assignment: that waits for the write
            // side's check that an agent cannot relabel its own item.
            _ if policy.agents.contains_key(value) => Some(Turn::Worker),
            _ => None,
        }
    });
    let mut fields = BTreeMap::new();
    let blocked: Vec<_> = labels
        .iter()
        .filter_map(|label| label.strip_prefix(BLOCKED_SCOPE))
        .collect();
    if blocked.iter().any(|reason| reason.is_empty()) {
        errors.push(format!("{BLOCKED_SCOPE} label without a reason"));
    }
    if !blocked.is_empty() {
        fields.insert("Blocked".to_owned(), blocked.into());
    }
    if let Some((comments, count)) = comments {
        // A thread that changed while it was read may hide a newer marker.
        if comments.len() as u64 != count {
            errors.push(format!("{} comments read, {count} counted", comments.len()));
        }
        for (name, value) in markers(comments, &config.recorders, &mut errors)? {
            fields.insert(name.to_owned(), value.into());
        }
    }
    let mut assignees = Vec::new();
    for assignee in issue["assignees"].as_array().into_iter().flatten() {
        assignees.push(text(assignee, "login")?.to_owned());
    }
    let mut timestamps = BTreeMap::new();
    for (key, native) in [
        ("createdAt", "created_at"),
        ("updatedAt", "updated_at"),
        ("closedAt", "closed_at"),
    ] {
        let value = issue.get(native).context("missing Forgejo timestamp")?;
        ensure!(
            value.is_null() || value.is_string(),
            "invalid Forgejo timestamp"
        );
        timestamps.insert(key.to_owned(), value.as_str().map(str::to_owned));
    }
    let identity = ItemRef {
        host: host.into(),
        repository: repository.into(),
        number,
    };
    let url = config
        .base()
        .join(&format!("{repository}/issues/{number}"))?;
    Ok(Item {
        identity,
        project_item: None,
        assignment: None,
        title: text(issue, "title")?.into(),
        state: state.into(),
        // Forgejo records no close reason.
        state_reason: None,
        // Forgejo has no Status; a label board does not invent one.
        status: None,
        priority,
        turn,
        fields_complete: errors.is_empty(),
        field_errors: errors,
        evidence: vec![Evidence {
            url,
            observed_at: clock.into(),
            provenance: "Forgejo API".into(),
        }],
        content_kind: "issue".into(),
        labels,
        assignees,
        author: None,
        timestamps,
        fields,
    })
}

/// The value of the one label in `scope`, or an error when there are zero,
/// several, or one whose value `parse` does not know.
fn exclusive<V>(
    labels: &[String],
    scope: &str,
    errors: &mut Vec<String>,
    parse: impl Fn(&str) -> Option<V>,
) -> Option<V> {
    let values: Vec<_> = labels
        .iter()
        .filter_map(|label| label.strip_prefix(scope))
        .collect();
    match values.as_slice() {
        [value] => {
            let parsed = parse(value);
            if parsed.is_none() {
                // Quoted: label names are anyone-with-label-rights' text.
                errors.push(format!("unknown label {:?}", format!("{scope}{value}")));
            }
            parsed
        }
        [] => {
            errors.push(format!("no {scope} label"));
            None
        }
        _ => {
            errors.push(format!("{} {scope} labels", values.len()));
            None
        }
    }
}

/// The newest recorder's Run and Result markers. A malformed newest marker
/// is an error rather than falling back to an older one.
fn markers(
    comments: &[Value],
    recorders: &[u64],
    errors: &mut Vec<String>,
) -> Result<BTreeMap<&'static str, String>> {
    let mut newest: BTreeMap<&'static str, (u64, Option<String>)> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for comment in comments {
        let id = comment["id"].as_u64().context("missing comment id")?;
        ensure!(seen.insert(id), "comment {id} listed twice");
        // Forgejo's ghost (deleted) and Actions users have negative ids, so
        // anything but a recorder's id is skipped rather than refused.
        if !comment["user"]["id"]
            .as_u64()
            .is_some_and(|author| recorders.contains(&author))
        {
            continue;
        }
        let first = text(comment, "body")?.lines().next().unwrap_or_default();
        let Some(inner) = first
            .trim()
            .strip_prefix("<!--")
            .and_then(|s| s.strip_suffix("-->"))
        else {
            continue;
        };
        for (name, marker) in [("Run", RUN_MARKER), ("Result", RESULT_MARKER)] {
            let Some(value) = inner.trim().strip_prefix(marker) else {
                continue;
            };
            let value = Url::parse(value.trim())
                .ok()
                .filter(|url| url.scheme() == "https")
                .map(String::from);
            if newest.get(name).is_none_or(|(at, _)| *at < id) {
                newest.insert(name, (id, value));
            }
        }
    }
    let mut fields = BTreeMap::new();
    for (name, (id, value)) in newest {
        match value {
            Some(value) => {
                fields.insert(name, value);
            }
            None => errors.push(format!("comment {id}: {name} marker is not an HTTPS URL")),
        }
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Answers only the URLs it holds, so an unexpected request fails the test.
    struct Pages(BTreeMap<String, (Value, Option<u64>)>);

    impl Transport for Pages {
        fn get(&self, url: &Url) -> Result<Response> {
            let (body, total) = self
                .0
                .get(url.as_str())
                .cloned()
                .with_context(|| format!("unexpected request {url}"))?;
            Ok(Response { body, total })
        }
    }

    const API: &str = "https://forge.example/api/v1/repos/sample/board/issues";
    const BOT: u64 = 9;

    fn config(extra: Value) -> Config {
        let mut value = json!({
            "url": "https://forge.example/",
            "repositories": ["sample/board"],
            "membership": {"label": "agent-board"},
            "recorders": [BOT],
        });
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(value).unwrap()
    }

    fn issues() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/forgejo/issues.json")).unwrap()
    }

    fn page(n: u64) -> String {
        format!("{API}?type=issues&state=all&labels=agent-board&limit=50&page={n}")
    }

    fn fixture_pages() -> BTreeMap<String, (Value, Option<u64>)> {
        BTreeMap::from([
            (page(1), (issues(), Some(3))),
            (
                format!("{API}/1/comments"),
                (
                    serde_json::from_str(include_str!("../../../fixtures/forgejo/comments-1.json"))
                        .unwrap(),
                    None,
                ),
            ),
        ])
    }

    fn snapshot(config: Config, pages: BTreeMap<String, (Value, Option<u64>)>) -> Result<Snapshot> {
        Forgejo {
            transport: Pages(pages),
            config,
        }
        .snapshot_at(
            "2026-10-20T00:00:00Z".into(),
            &board_core::AgentPolicy::parse(board_core::DEFAULT_AGENT_POLICY).unwrap(),
        )
    }

    /// The CLI tests read the committed snapshot; it must be what the reader
    /// makes of the committed issue listing and comment thread.
    #[test]
    fn fixture_snapshot_matches_reader() {
        let made = snapshot(config(json!({})), fixture_pages()).unwrap();
        let committed: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/forgejo/board.json")).unwrap();
        assert_eq!(
            made,
            committed,
            "{}",
            serde_json::to_string_pretty(&made).unwrap()
        );
    }

    #[test]
    fn label_scope_table() {
        for (labels, priority, turn, errors) in [
            (
                vec!["priority/P1", "turn/operator"],
                Some(Priority::P1),
                Some(Turn::Operator),
                vec![],
            ),
            (
                vec!["priority/P3", "turn/rust-worker", "blocked/ci"],
                Some(Priority::P3),
                Some(Turn::Worker),
                vec![],
            ),
            (
                vec!["turn/coordinator"],
                None,
                Some(Turn::Coordinator),
                vec!["no priority/ label"],
            ),
            (
                vec!["priority/P1", "priority/P2", "turn/operator"],
                None,
                Some(Turn::Operator),
                vec!["2 priority/ labels"],
            ),
            (
                vec!["priority/P9", "turn/stranger"],
                None,
                None,
                vec![
                    "unknown label \"priority/P9\"",
                    "unknown label \"turn/stranger\"",
                ],
            ),
            // Label text is quoted, so it cannot forge diagnostic lines.
            (
                vec!["priority/P1\nsample/board#1 ok", "turn/operator"],
                None,
                Some(Turn::Operator),
                vec!["unknown label \"priority/P1\\nsample/board#1 ok\""],
            ),
            (
                vec!["priority/P2", "turn/none", "blocked/"],
                Some(Priority::P2),
                Some(Turn::None),
                vec!["blocked/ label without a reason"],
            ),
        ] {
            let mut labels: Vec<_> = labels.into_iter().map(str::to_owned).collect();
            labels.push("agent-board".into());
            let issue = json!({
                "number": 7, "title": "t", "state": "open", "pull_request": null,
                "repository": {"full_name": "sample/board"},
                "labels": labels.iter().map(|name| json!({"name": name, "exclusive": true})).collect::<Vec<_>>(),
                "assignees": null,
                "created_at": "2026-10-01T00:00:00Z", "updated_at": "2026-10-01T00:00:00Z", "closed_at": null,
            });
            let item = item(
                "forge.example",
                "sample/board",
                &issue,
                None,
                &config(json!({})),
                &board_core::AgentPolicy::parse(board_core::DEFAULT_AGENT_POLICY).unwrap(),
                "2026-10-20T00:00:00Z",
            )
            .unwrap();
            assert_eq!(
                (&item.priority, &item.turn),
                (&priority, &turn),
                "{labels:?}"
            );
            assert_eq!(item.field_errors, errors, "{labels:?}");
            assert_eq!(item.fields_complete, errors.is_empty());
        }
    }

    #[test]
    fn marker_table() {
        // The login is display text; only the user id is trusted.
        let comment = |id: u64, user: u64, body: &str| json!({"id": id, "user": {"id": user, "login": "board-bot"}, "body": body});
        let run = "<!-- agent-board-run: https://forge.example/sample/runs/1 -->\nRun started";
        let newer = "<!-- agent-board-run: https://forge.example/sample/runs/2 -->";
        for (comments, expected, errors) in [
            (vec![comment(1, BOT, run)], Some("runs/1"), 0),
            // Another account's copied marker does not count, whatever its login.
            (vec![comment(1, 11, run)], None, 0),
            // Ghost (-1) and Actions (-2) authors, or none, are not refused.
            (
                vec![
                    json!({"id": 3, "user": {"id": -1, "login": "Ghost"}, "body": newer}),
                    json!({"id": 4, "user": {"id": -2, "login": "forgejo-actions"}, "body": newer}),
                    json!({"id": 5, "user": null, "body": newer}),
                    comment(1, BOT, run),
                ],
                Some("runs/1"),
                0,
            ),
            (
                vec![comment(2, BOT, newer), comment(1, BOT, run)],
                Some("runs/2"),
                0,
            ),
            // Quoted or later-line markers are text, not records.
            (vec![comment(1, BOT, &format!("> {run}"))], None, 0),
            (
                vec![
                    comment(1, BOT, run),
                    comment(2, BOT, "<!-- agent-board-run: http://insecure -->"),
                ],
                None,
                1,
            ),
        ] {
            let mut errs = vec![];
            let fields = markers(&comments, &[BOT], &mut errs).unwrap();
            assert_eq!(
                fields
                    .get("Run")
                    .map(|v| v.rsplit_once("sample/").unwrap().1),
                expected
            );
            assert_eq!(errs.len(), errors);
        }
        let mut errs = vec![];
        assert!(
            markers(
                &[comment(1, BOT, run), comment(1, BOT, run)],
                &[BOT],
                &mut errs
            )
            .is_err()
        );
    }

    #[test]
    fn completeness_table() {
        let three = issues();
        let two = json!(three.as_array().unwrap()[..2]);
        let comments = || {
            serde_json::from_str::<Value>(include_str!("../../../fixtures/forgejo/comments-1.json"))
                .unwrap()
        };
        let mut short = comments();
        short.as_array_mut().unwrap().pop();
        for (pages, complete, field_errors) in [
            (fixture_pages(), true, None),
            // No count to check the pages against.
            (
                BTreeMap::from([
                    (page(1), (three.clone(), None)),
                    (page(2), (json!([]), None)),
                    (format!("{API}/1/comments"), (comments(), None)),
                ]),
                false,
                None,
            ),
            // An issue closed mid-read shifted one past the pages read.
            (
                BTreeMap::from([
                    (page(1), (two.clone(), Some(3))),
                    (page(2), (json!([]), Some(2))),
                    (format!("{API}/1/comments"), (comments(), None)),
                ]),
                false,
                None,
            ),
            // A comment arrived between listing the issue and reading its thread.
            (
                BTreeMap::from([
                    (page(1), (three.clone(), Some(3))),
                    (format!("{API}/1/comments"), (short, None)),
                ]),
                true,
                Some("3 comments read, 4 counted"),
            ),
        ] {
            let made = snapshot(config(json!({})), pages).unwrap();
            assert_eq!(made.coverage.complete, complete);
            assert_eq!(
                made.items[0].field_errors.first().map(String::as_str),
                field_errors
            );
        }
    }

    #[test]
    fn client_keeps_the_token_on_the_api_root() {
        let client = Client {
            api: config(json!({})).api_root().unwrap(),
            token: "test-token-not-secret".into(),
            token_env: DEFAULT_TOKEN_ENV.into(),
            agent: crate::github::http_agent(),
        };
        for url in [
            "https://elsewhere.example/api/v1/repos/sample/board/issues",
            "https://forge.example/api/v2/x",
            "https://forge.example.evil/api/v1/",
        ] {
            let error = client.get(&Url::parse(url).unwrap()).err().unwrap();
            assert!(error.to_string().contains("refusing"), "{url}");
        }
    }

    #[test]
    fn listing_refusal_table() {
        let issue = |number: u64| {
            let mut issue = issues()[0].clone();
            issue["number"] = number.into();
            issue["comments"] = 0.into();
            issue
        };
        let mut pull = issue(1);
        pull["pull_request"] = json!({"merged": false});
        let mut moved = issue(1);
        moved["repository"]["full_name"] = "other/board".into();
        let mut unlabelled = issue(1);
        unlabelled["labels"] = json!([]);
        for (pages, message) in [
            (vec![(page(1), json!([pull]))], "is a pull request"),
            (vec![(page(1), json!([moved]))], "reported from other/board"),
            // Forgejo drops an unknown label filter and lists every issue.
            (vec![(page(1), json!([unlabelled]))], "membership label"),
            (
                vec![(page(1), json!([issue(5)])), (page(2), json!([issue(5)]))],
                "listed twice",
            ),
            (vec![(page(1), json!({"message": "no"}))], "not an array"),
        ] {
            let total: usize = pages
                .iter()
                .map(|(_, body)| body.as_array().map_or(0, Vec::len))
                .sum();
            let pages = pages
                .into_iter()
                .map(|(url, body)| (url, (body, Some(total as u64))))
                .collect();
            let error = snapshot(config(json!({})), pages).unwrap_err();
            assert!(format!("{error:#}").contains(message), "{error:#}");
        }
        // A server that ignores `page` exhausts the budget instead of looping.
        struct Endless;
        impl Transport for Endless {
            fn get(&self, url: &Url) -> Result<Response> {
                let page: u64 = url
                    .query_pairs()
                    .find(|(k, _)| k == "page")
                    .unwrap()
                    .1
                    .parse()
                    .unwrap();
                let mut issue = issues()[0].clone();
                issue["number"] = page.into();
                issue["comments"] = 0.into();
                Ok(Response {
                    body: json!([issue]),
                    total: None,
                })
            }
        }
        let error = Forgejo {
            transport: Endless,
            config: config(json!({"recorders": []})),
        }
        .snapshot_at(
            "2026-10-20T00:00:00Z".into(),
            &board_core::AgentPolicy::parse(board_core::DEFAULT_AGENT_POLICY).unwrap(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("budget exhausted"));
    }

    #[test]
    fn membership_and_recorders_shape_requests() {
        // Open membership lists open issues without a label filter, and no
        // recorders means no comment reads (Pages refuses unlisted URLs).
        let pages = BTreeMap::from([(
            format!("{API}?type=issues&state=open&limit=50&page=1"),
            (issues(), Some(3)),
        )]);
        let made = snapshot(
            config(json!({"membership": "open", "recorders": []})),
            pages,
        )
        .unwrap();
        assert_eq!(made.items.len(), 3);
        assert!(
            made.items
                .iter()
                .all(|item| !item.fields.contains_key("Run"))
        );
    }

    #[test]
    fn board_config_table() {
        for (value, ok) in [
            (json!({}), true),
            (json!({"board": {"forge": "github"}}), true),
            (
                json!({"board": {"forge": "github", "url": "https://x.example/"}}),
                false,
            ),
            (json!({"board": {"forge": "gitlab"}}), false),
            (
                json!({"board": {"forge": "forgejo", "url": "https://forge.example/", "repositories": ["sample/board"], "membership": "open"}}),
                true,
            ),
            (
                json!({"board": {"forge": "forgejo", "url": "https://forge.example/git/", "repositories": ["sample/board"], "membership": {"label": "agent-board"}, "tokenEnv": "BOARD_TOKEN", "recorders": [9]}}),
                true,
            ),
            // The GitHub token must not go to another host.
            (
                json!({"board": {"forge": "forgejo", "url": "https://forge.example/", "repositories": ["sample/board"], "membership": "open", "tokenEnv": "GH_TOKEN"}}),
                false,
            ),
            (
                json!({"board": {"forge": "forgejo", "url": "https://forge.example/", "repositories": ["sample/board"], "membership": {"label": "priority/P1"}}}),
                false,
            ),
            (
                json!({"board": {"forge": "forgejo", "url": "https://forge.example/", "repositories": ["sample/board"], "membership": "open", "recorders": [0]}}),
                false,
            ),
            (
                json!({"board": {"forge": "forgejo", "url": "http://forgejo-http.forgejo.svc:3000/", "repositories": ["sample/board"], "membership": "open"}}),
                true,
            ),
            (
                json!({"board": {"forge": "forgejo", "url": "https://forge.example:3000/", "repositories": ["sample/board"], "membership": "open"}}),
                true,
            ),
            (
                json!({"board": {"forge": "forgejo", "url": "http://user:pw@forge.example/", "repositories": ["sample/board"], "membership": "open"}}),
                false,
            ),
            (
                json!({"board": {"forge": "forgejo", "url": "http://forge.example/?a=b", "repositories": ["sample/board"], "membership": "open"}}),
                false,
            ),
            (
                json!({"board": {"forge": "forgejo", "url": "https://forge.example/#x", "repositories": ["sample/board"], "membership": "open"}}),
                false,
            ),
            (
                json!({"board": {"forge": "forgejo", "url": "ftp://forge.example/", "repositories": ["sample/board"], "membership": "open"}}),
                false,
            ),
            (
                json!({"board": {"forge": "forgejo", "url": "https://forge.example/", "repositories": [], "membership": "open"}}),
                false,
            ),
            (
                json!({"board": {"forge": "forgejo", "url": "https://forge.example/", "repositories": ["sample/../x"], "membership": "open"}}),
                false,
            ),
            (
                json!({"board": {"forge": "forgejo", "url": "https://forge.example/", "repositories": ["sample/board", "Sample/Board"], "membership": "open"}}),
                false,
            ),
            (
                json!({"board": {"forge": "forgejo", "url": "https://forge.example/", "repositories": ["sample/board"], "membership": {"label": "a,b"}}}),
                false,
            ),
            (
                json!({"board": {"forge": "forgejo", "url": "https://forge.example/", "repositories": ["sample/board"], "membership": "open", "recorders": ["a/b"]}}),
                false,
            ),
            (
                json!({"board": {"forge": "forgejo", "url": "https://forge.example/", "repositories": ["sample/board"], "membership": "open", "extra": 1}}),
                false,
            ),
            (json!({"bord": {"forge": "forgejo"}}), false),
        ] {
            assert_eq!(Board::from_config(&value).is_ok(), ok, "{value}");
        }
        let Board::Forgejo(config) = Board::from_config(&json!({"board": {"forge": "forgejo", "url": "https://forge.example/git", "repositories": ["sample/board"], "membership": "open"}})).unwrap() else {
            panic!("not forgejo");
        };
        assert_eq!(
            config.api_root().unwrap().as_str(),
            "https://forge.example/git/api/v1/"
        );
        assert_eq!(config.token_env, DEFAULT_TOKEN_ENV);
        let Board::Forgejo(config) = Board::from_config(&json!({"board": {"forge": "forgejo", "url": "http://forgejo-http.forgejo.svc:3000/git", "repositories": ["sample/board"], "membership": "open"}})).unwrap() else {
            panic!("not forgejo");
        };
        assert_eq!(
            config.api_root().unwrap().as_str(),
            "http://forgejo-http.forgejo.svc:3000/git/api/v1/"
        );
    }

    #[test]
    fn plain_http_warning_table() {
        for (url, warns) in [
            ("http://localhost:3000/", false),
            ("http://forgejo/", false),
            ("http://127.0.0.1/", false),
            ("http://127.8.0.1/", false),
            ("http://[::1]:3000/", false),
            ("http://10.0.0.5/", false),
            ("http://172.16.0.1/", false),
            ("http://192.168.1.10:3000/", false),
            ("http://169.254.0.1/", false),
            ("http://[fd00::1]/", false),
            ("http://[fe80::1]/", false),
            ("http://forgejo-http.forgejo.svc:3000/", false),
            ("http://forgejo-http.forgejo.svc.cluster.local/", false),
            ("http://forge.local/", false),
            ("http://forge.corp.internal/", false),
            ("http://forge.example/", true),
            ("http://forge.example:3000/", true),
            ("http://8.8.8.8/", true),
            ("http://[2001:db8::1]/", true),
            ("http://forge.svc.example/", true),
            ("https://forge.example/", false),
            ("https://8.8.8.8:3000/", false),
        ] {
            let config = config(json!({"url": url}));
            let warning = config.plain_http_warning();
            assert_eq!(warning.is_some(), warns, "{url}");
            if let Some(warning) = warning {
                assert!(warning.contains("unencrypted"), "{warning}");
            }
        }
    }
}
