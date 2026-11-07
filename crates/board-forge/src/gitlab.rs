//! Read-only GitLab board. Like Forgejo's, the board is the issues of
//! configured projects: Priority and Turn come from scoped labels, Run and
//! Result from append-only recorder notes. There is no write path; the
//! [`crate::Forge`] defaults refuse `board_write` and `plan_claim`.
use anyhow::{Context, Result, bail, ensure};
use board_core::{Coverage, Evidence, Item, ItemRef, Snapshot};
use serde::Deserialize;
use serde_json::Value;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

pub use crate::forgejo::Membership;
use crate::forgejo::{
    BLOCKED_SCOPE, GITHUB_TOKEN_ENVS, PRIORITY_SCOPE, STATUS_SCOPE, TURN_SCOPE, exclusive, markers,
    parse_priority, parse_status, validate_instance_url, validate_membership_label,
};

/// The GitLab token variable, `PRIVATE-TOKEN` authenticated.
pub const DEFAULT_TOKEN_ENV: &str = "GITLAB_TOKEN";
/// GitLab's default maximum page size; a smaller server maximum is still
/// read completely because paging follows `X-Total` and `X-Next-Page`.
const PAGE_LIMIT: &str = "50";
// Shared by every request in one snapshot. Exhaustion is an error, never a
// partial snapshot; it also stops a server that ignores `page` from looping.
const SNAPSHOT_REQUEST_LIMIT: usize = 1_000;
const DEFAULT_URL: &str = "https://gitlab.com/";

/// The `board` entry of a GitLab `--board-config`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    /// The GitLab instance, `https://gitlab.com/` by default.
    #[serde(default = "default_url")]
    pub url: Url,
    /// The environment variable holding a token that can read the issues.
    #[serde(default = "default_token_env")]
    pub token_env: String,
    /// Project paths whose issues make up the board, each
    /// `NAMESPACE/PROJECT` where the namespace may be a group subgroup
    /// chain, for example `group/subgroup/project`.
    pub repositories: Vec<String>,
    pub membership: Membership,
    /// User ids whose marker notes count. Anyone who can comment on an
    /// issue could otherwise set its Run or Result, and a login can be
    /// renamed and taken over, so ids; with none, notes are not read.
    #[serde(default)]
    pub recorders: Vec<u64>,
    /// Opt in to the reviewed Status label representation. Absent, the
    /// board has no Status field, as Forgejo's does without it.
    #[serde(default)]
    pub status_labels: bool,
}

fn default_url() -> Url {
    Url::parse(DEFAULT_URL).expect("default GitLab URL parses")
}

fn default_token_env() -> String {
    DEFAULT_TOKEN_ENV.into()
}

/// Safe existing project identity, not a project-creation request: every
/// path segment is a name GitLab can already serve, and the last segment is
/// not a `.git` clone URL spelling.
pub fn validate_project(project: &str) -> Result<()> {
    let segments: Vec<_> = project.split('/').collect();
    ensure!(
        segments.len() >= 2,
        "invalid project {project:?}: expected GitLab NAMESPACE/PROJECT"
    );
    for segment in &segments {
        ensure!(
            !segment.is_empty()
                && *segment != "."
                && *segment != ".."
                && !segment.starts_with('-')
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)),
            "invalid project {project:?}: bad path segment {segment:?}"
        );
    }
    ensure!(
        !segments.last().is_some_and(|last| last.ends_with(".git")),
        "invalid project {project:?}: a project path does not end in .git"
    );
    Ok(())
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        validate_instance_url(&self.url)?;
        ensure!(
            !self.token_env.is_empty() && !GITHUB_TOKEN_ENVS.contains(&self.token_env.as_str()),
            "board tokenEnv must name a GitLab token variable, not empty or a GitHub token"
        );
        ensure!(
            !self.repositories.is_empty(),
            "board repositories must not be empty"
        );
        let mut seen = BTreeSet::new();
        for project in &self.repositories {
            validate_project(project).context("invalid board repository")?;
            ensure!(
                seen.insert(project.to_ascii_lowercase()),
                "duplicate board repository: {project}"
            );
        }
        if let Membership::Label(label) = &self.membership {
            validate_membership_label(label)?;
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
        crate::forgejo::plain_http_warning(&self.url)
    }

    /// The instance URL as a directory, so joins keep a subpath install.
    fn base(&self) -> Url {
        let mut base = self.url.clone();
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        base
    }

    pub(crate) fn api_root(&self) -> Result<Url> {
        Ok(self.base().join("api/v4/")?)
    }
}

/// A GitLab token was not provided or was refused.
#[derive(Debug)]
pub struct AuthenticationRequired(pub String);

impl std::fmt::Display for AuthenticationRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "authentication required: provide a valid GitLab token in {} (project Issues read)",
            self.0
        )
    }
}

impl std::error::Error for AuthenticationRequired {}

#[derive(Debug)]
pub struct NotFound;

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GitLab object not found")
    }
}

impl std::error::Error for NotFound {}

pub struct Response {
    pub body: Value,
    /// The listing's `X-Total`, where the server sent one.
    pub total: Option<u64>,
    /// The listing's `X-Next-Page`, where the server sent one.
    pub next_page: Option<u64>,
}

pub trait Transport {
    /// GET a JSON body from a URL under the API root, with the listing's
    /// pagination headers.
    fn get(&self, url: &Url) -> Result<Response>;

    /// Reads-only: this backend has no write path at all, so even the
    /// mutation-probing `send` refuses rather than silently dropping it.
    fn send(&self, _method: &str, _url: &Url, _body: &Value) -> Result<(u16, Value)> {
        bail!("GitLab transport does not support writes")
    }

    /// Unsupported by read-only transports.
    fn write(&self, _method: &str, _url: &Url, _body: &Value) -> Result<Value> {
        bail!("GitLab writes unsupported by this transport")
    }
}

impl<T: Transport + ?Sized> Transport for &T {
    fn get(&self, url: &Url) -> Result<Response> {
        (**self).get(url)
    }

    fn send(&self, method: &str, url: &Url, body: &Value) -> Result<(u16, Value)> {
        (**self).send(method, url, body)
    }

    fn write(&self, method: &str, url: &Url, body: &Value) -> Result<Value> {
        (**self).write(method, url, body)
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
            .header("PRIVATE-TOKEN", &self.token)
            .header("Accept", "application/json")
            .header("User-Agent", "agent-board")
            // A redirect could carry the token to another origin.
            .config()
            .max_redirects(0)
            .build()
            .call()
            .map_err(|_| anyhow::anyhow!("GET {path}: GitLab transport failed"))?;
        match response.status().as_u16() {
            200..300 => {
                let total = response
                    .headers()
                    .get("x-total")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok());
                let next_page = response
                    .headers()
                    .get("x-next-page")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok());
                let body = response
                    .body_mut()
                    .read_json()
                    .with_context(|| format!("decode GET {path}"))?;
                Ok(Response {
                    body,
                    total,
                    next_page,
                })
            }
            401 | 403 => Err(
                anyhow::Error::from(AuthenticationRequired(self.token_env.clone()))
                    .context(format!("GET {path}: GitLab refused the token")),
            ),
            404 => Err(NotFound.into()),
            status => bail!("GET {path}: GitLab HTTP status {status}"),
        }
    }
}

pub struct Gitlab<T> {
    pub transport: T,
    pub config: Config,
}

impl<T: Transport> crate::Forge for Gitlab<T> {
    async fn snapshot(&self) -> Result<Snapshot> {
        self.snapshot_with_policy(&board_core::AgentPolicy::parse(
            board_core::DEFAULT_AGENT_POLICY,
        )?)
    }

    fn capabilities(&self) -> Result<Vec<board_core::Capability>> {
        // A read is not proof of mutation authority.
        Ok(vec![])
    }
}

impl<T: Transport> Gitlab<T> {
    pub fn snapshot_with_policy(&self, policy: &board_core::AgentPolicy) -> Result<Snapshot> {
        let clock = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)?;
        self.snapshot_at(clock, policy)
    }

    fn get(
        &self,
        budget: &Cell<usize>,
        url: &Url,
    ) -> Result<(Vec<Value>, Option<u64>, Option<u64>)> {
        let remaining = budget.get();
        ensure!(
            remaining > 0,
            "GitLab snapshot request budget exhausted; no snapshot produced"
        );
        budget.set(remaining - 1);
        let response = self.transport.get(url)?;
        let items = response
            .body
            .as_array()
            .context("GitLab listing is not an array")?;
        Ok((items.clone(), response.total, response.next_page))
    }

    /// Every page of a listing, and whether it read whole ([`pages`]).
    fn pages(&self, budget: &Cell<usize>, url: Url) -> Result<(Vec<Value>, bool)> {
        pages(url, usize::MAX, |url| self.get(budget, url))
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
            // GitLab requires the project path with `/` percent-encoded as
            // %2F; the URL parser preserves that through a parse (unlike a
            // relative join, which normalizes %2F into a path separator, and
            // unlike path_segments_mut().push(), which double-encodes). Only
            // `/` needs encoding: validation admits just unreserved
            // characters per segment.
            let project = repository.replace('/', "%2F");
            let mut url = api_url(&api, &format!("projects/{project}/issues"))?;
            // Scope all, not just the token's own issues.
            url.query_pairs_mut().append_pair("scope", "all");
            match &config.membership {
                Membership::Open => url.query_pairs_mut().append_pair("state", "opened"),
                Membership::Label(label) => url
                    .query_pairs_mut()
                    .append_pair("state", "all")
                    .append_pair("labels", label),
            };
            let (issues, listed) = self.pages(&budget, url)?;
            complete &= listed;
            let mut seen = BTreeSet::new();
            for issue in issues {
                let iid = issue["iid"].as_u64().context("missing issue iid")?;
                // A repeated iid means pages overlapped, so others may be missing.
                ensure!(
                    seen.insert(iid),
                    "{repository}#{iid} listed twice; no snapshot produced"
                );
                let count = issue["user_notes_count"]
                    .as_u64()
                    .context("missing GitLab note count")?;
                // The notes listing is paginated and includes system notes;
                // the user notes among them are checked against the issue's
                // count.
                let notes = if config.recorders.is_empty() || count == 0 {
                    None
                } else {
                    let url = api_url(&api, &format!("projects/{project}/issues/{iid}/notes"))?;
                    Some(self.pages(&budget, url)?)
                };
                items.push(item(
                    host,
                    repository,
                    &issue,
                    notes
                        .as_ref()
                        .map(|(notes, listed)| (notes.as_slice(), count, *listed)),
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
            // GitLab has no project board here, only labels.
            status_field_absent: !config.status_labels,
        };
        snapshot.items.sort_by(|a, b| a.identity.cmp(&b.identity));
        snapshot.validate()?;
        policy.normalize(&mut snapshot)?;
        Ok(snapshot)
    }
}

/// Every page of a listing at `url`, and whether it read whole: whole only
/// where every page reported one `X-Total` equal to the items read. Paging
/// stops at an empty page or a missing `X-Next-Page`; more than `cap` pages
/// is not whole (the caller's request budget caps the loop).
pub(crate) fn pages(
    mut url: Url,
    cap: usize,
    mut get: impl FnMut(&Url) -> Result<(Vec<Value>, Option<u64>, Option<u64>)>,
) -> Result<(Vec<Value>, bool)> {
    let base: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    let mut all = Vec::new();
    let mut totals = BTreeSet::new();
    for page in 1..=cap {
        let (items, total, next) = get(page_url(&mut url, &base, page))?;
        totals.insert(total);
        all.extend(items.iter().cloned());
        if items.is_empty() || next.is_none() {
            break;
        }
    }
    let complete = totals.len() == 1 && totals.first() == Some(&Some(all.len() as u64));
    Ok((all, complete))
}

/// `url` with its `base` query and the `page` of [`PAGE_LIMIT`] items.
fn page_url<'a>(url: &'a mut Url, base: &[(String, String)], page: usize) -> &'a Url {
    url.query_pairs_mut()
        .clear()
        .extend_pairs(base)
        .append_pair("per_page", PAGE_LIMIT)
        .append_pair("page", &page.to_string());
    url
}

/// A request URL under the API root, parsed from the whole string so that
/// percent-encoding in the path (the project path's `%2F`) survives: a
/// relative join would normalize `%2F` into a path separator.
fn api_url(api: &Url, path: &str) -> Result<Url> {
    Url::parse(&format!("{api}{path}")).context("build GitLab API URL")
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("missing GitLab {key}"))
}

fn item(
    host: &str,
    repository: &str,
    issue: &Value,
    // The thread's user notes read, the issue's note count, and whether the
    // thread read whole, when notes were read.
    notes: Option<(&[Value], u64, bool)>,
    config: &Config,
    policy: &board_core::AgentPolicy,
    clock: &str,
) -> Result<Item> {
    let iid = issue["iid"].as_u64().context("missing issue iid")?;
    // Issue versus merge request identity is a boundary. GitLab's project
    // issues API lists issues only, not merge requests; the web URL is the
    // proof, so a merge request that a server merged into the listing is
    // refused rather than read as an issue. New projects serve issues as
    // work items (`/-/work_items/IID`), GitLab's issue model sharing the
    // issue iid namespace; `references.full` stays the identity authority.
    let web_url = text(issue, "web_url")?;
    ensure!(
        ["/-/issues/", "/-/work_items/"]
            .iter()
            .any(|kind| web_url.ends_with(&format!("{kind}{iid}"))),
        "{repository}#{iid} is not an issue: {web_url}"
    );
    let listed = text(&issue["references"], "full")?;
    let (project, number) = listed
        .rsplit_once('#')
        .with_context(|| format!("missing GitLab issue reference {listed:?}"))?;
    ensure!(
        number == iid.to_string() && project.eq_ignore_ascii_case(repository),
        "{repository}#{iid} reported from {listed}"
    );
    let state = match text(issue, "state")? {
        "opened" => "open",
        "closed" => "closed",
        _ => bail!("unknown GitLab issue state"),
    };
    let labels = issue["labels"]
        .as_array()
        .context("missing GitLab labels")?
        .iter()
        .map(|label| label.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .context("missing GitLab label name")?;
    if let Membership::Label(member) = &config.membership {
        ensure!(
            labels.contains(member),
            "{repository}#{iid} lacks the membership label"
        );
    }
    let mut errors = Vec::new();
    let status =
        if config.status_labels && labels.iter().any(|label| label.starts_with(STATUS_SCOPE)) {
            exclusive(&labels, STATUS_SCOPE, &mut errors, parse_status)
        } else {
            None
        };
    let priority = exclusive(&labels, PRIORITY_SCOPE, &mut errors, parse_priority);
    let turn = exclusive(&labels, TURN_SCOPE, &mut errors, |value| {
        match value.to_ascii_lowercase().as_str() {
            "coordinator" => Some(board_core::Turn::Coordinator),
            "worker" => Some(board_core::Turn::Worker),
            "operator" => Some(board_core::Turn::Operator),
            "external" => Some(board_core::Turn::External),
            "none" => Some(board_core::Turn::None),
            // `turn/<identity>` hands the item to that policy identity.
            _ if policy.agents.contains_key(value) => Some(board_core::Turn::Worker),
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
    if let Some((notes, count, listed)) = notes {
        // The listing includes GitLab's system notes; the issue's count is
        // of user notes, so compare and scan only those.
        let user: Vec<Value> = notes
            .iter()
            .filter(|note| note["system"].as_bool() != Some(true))
            .cloned()
            .collect();
        if !listed {
            // A thread that may be missing notes can hide a newer marker.
            errors.push("notes listing incomplete".to_owned());
        } else {
            if user.len() as u64 != count {
                errors.push(format!("{} notes read, {count} counted", user.len()));
            }
            for (name, value) in markers(&user, "author", &config.recorders, &mut errors)? {
                fields.insert(name.to_owned(), value.into());
            }
        }
    }
    let mut assignees = Vec::new();
    for assignee in issue["assignees"].as_array().into_iter().flatten() {
        assignees.push(text(assignee, "username")?.to_owned());
    }
    let mut timestamps = BTreeMap::new();
    for (key, native) in [
        ("createdAt", "created_at"),
        ("updatedAt", "updated_at"),
        ("closedAt", "closed_at"),
    ] {
        let value = issue.get(native).context("missing GitLab timestamp")?;
        ensure!(
            value.is_null() || value.is_string(),
            "invalid GitLab timestamp"
        );
        timestamps.insert(key.to_owned(), value.as_str().map(str::to_owned));
    }
    let identity = ItemRef {
        host: host.into(),
        repository: repository.into(),
        number: iid,
    };
    let url = config
        .base()
        .join(&format!("{repository}/-/issues/{iid}"))?;
    Ok(Item {
        identity,
        project_item: None,
        assignment: None,
        title: text(issue, "title")?.into(),
        state: state.into(),
        // GitLab records no close reason on issues.
        state_reason: None,
        status,
        priority,
        turn,
        fields_complete: errors.is_empty(),
        field_errors: errors,
        evidence: vec![Evidence {
            url,
            observed_at: clock.into(),
            provenance: "GitLab API".into(),
        }],
        content_kind: "issue".into(),
        labels,
        assignees,
        author: None,
        timestamps,
        fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forgejo::Board;
    use serde_json::json;

    /// Answers only the URLs it holds, so an unexpected request fails the test.
    struct Pages(BTreeMap<String, (Value, Option<u64>, Option<u64>)>);

    impl Transport for Pages {
        fn get(&self, url: &Url) -> Result<Response> {
            let (body, total, next_page) = self
                .0
                .get(url.as_str())
                .cloned()
                .with_context(|| format!("unexpected request {url}"))?;
            Ok(Response {
                body,
                total,
                next_page,
            })
        }
    }

    const API: &str = "https://gitlab.example/api/v4/projects/sample%2Fboard/issues";
    const BOT: u64 = 9;

    pub(super) fn config(extra: Value) -> Config {
        let mut value = json!({
            "url": "https://gitlab.example/",
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

    fn issues(range: std::ops::Range<usize>) -> Value {
        let all: Value =
            serde_json::from_str(include_str!("../../../fixtures/gitlab/issues.json")).unwrap();
        json!(&all.as_array().unwrap()[range])
    }

    fn issue(iid: u64) -> Value {
        let mut issue = issues(0..1).as_array().unwrap()[0].clone();
        issue["iid"] = iid.into();
        issue["user_notes_count"] = 0.into();
        issue["web_url"] = format!("https://gitlab.example/sample/board/-/issues/{iid}").into();
        issue["references"]["full"] = format!("sample/board#{iid}").into();
        issue
    }

    fn notes() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/gitlab/notes-1.json")).unwrap()
    }

    fn page(n: u64) -> String {
        format!("{API}?scope=all&state=all&labels=agent-board&per_page=50&page={n}")
    }

    fn notes_url(iid: u64) -> String {
        format!("{API}/{iid}/notes?per_page=50&page=1")
    }

    fn fixture_pages() -> BTreeMap<String, (Value, Option<u64>, Option<u64>)> {
        BTreeMap::from([
            (page(1), (issues(0..2), Some(3), Some(2))),
            (page(2), (issues(2..3), Some(3), None)),
            (notes_url(1), (notes(), Some(5), None)),
        ])
    }

    fn snapshot(
        config: Config,
        pages: BTreeMap<String, (Value, Option<u64>, Option<u64>)>,
    ) -> Result<Snapshot> {
        Gitlab {
            transport: Pages(pages),
            config,
        }
        .snapshot_at(
            "2026-10-20T00:00:00Z".into(),
            &board_core::AgentPolicy::parse(board_core::DEFAULT_AGENT_POLICY).unwrap(),
        )
    }

    /// The CLI tests read the committed snapshot; it must be what the reader
    /// makes of the committed issue listing and note thread.
    #[test]
    fn fixture_snapshot_matches_reader() {
        let made = snapshot(config(json!({})), fixture_pages()).unwrap();
        let committed: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/gitlab/board.json")).unwrap();
        assert_eq!(
            made,
            committed,
            "{}",
            serde_json::to_string_pretty(&made).unwrap()
        );
    }

    #[test]
    fn completeness_table() {
        // The note thread read short of its count.
        let mut short = notes();
        short
            .as_array_mut()
            .unwrap()
            .retain(|note| note["id"] != json!(504));
        for (pages, complete, field_errors) in [
            (fixture_pages(), true, None),
            // No total to prove the pages against.
            (
                BTreeMap::from([
                    (page(1), (issues(0..3), None, Some(2))),
                    (page(2), (json!([]), None, None)),
                    (notes_url(1), (notes(), None, None)),
                ]),
                false,
                Some("notes listing incomplete"),
            ),
            // An issue closed mid-read shifted one past the pages read.
            (
                BTreeMap::from([
                    (page(1), (issues(0..2), Some(3), Some(2))),
                    (page(2), (json!([]), Some(2), None)),
                    (notes_url(1), (notes(), Some(5), None)),
                ]),
                false,
                None,
            ),
            // A note arrived between listing the issue and reading its thread.
            (
                BTreeMap::from([
                    (page(1), (issues(0..2), Some(3), Some(2))),
                    (page(2), (issues(2..3), Some(3), None)),
                    (notes_url(1), (short, Some(4), None)),
                ]),
                true,
                Some("3 notes read, 4 counted"),
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
    fn listing_refusal_table() {
        let mut merge_request = issue(1);
        merge_request["web_url"] = "https://gitlab.example/sample/board/-/merge_requests/1".into();
        // An epic's URL is not an issue's, even though an epic also carries
        // an iid in the project; a work item's is (the fixture's issue 2).
        let mut epic = issue(1);
        epic["web_url"] = "https://gitlab.example/sample/board/-/epics/1".into();
        let mut moved = issue(1);
        moved["references"]["full"] = "other/board#1".into();
        let mut unlabelled = issue(1);
        unlabelled["labels"] = json!([]);
        let mut odd_state = issue(1);
        odd_state["state"] = "reopened".into();
        for (pages, message) in [
            (
                vec![(page(1), json!([merge_request]), None)],
                "is not an issue",
            ),
            (vec![(page(1), json!([epic]), None)], "is not an issue"),
            (
                vec![(page(1), json!([moved]), None)],
                "reported from other/board",
            ),
            // GitLab drops an unknown label filter and lists every issue.
            (
                vec![(page(1), json!([unlabelled]), None)],
                "membership label",
            ),
            (
                vec![(page(1), json!([odd_state]), None)],
                "unknown GitLab issue state",
            ),
            (
                vec![
                    (page(1), json!([issue(5)]), Some(2)),
                    (page(2), json!([issue(5)]), None),
                ],
                "listed twice",
            ),
            (
                vec![(page(1), json!({"message": "no"}), None)],
                "not an array",
            ),
        ] {
            let total: usize = pages
                .iter()
                .map(|(_, body, _)| body.as_array().map_or(0, Vec::len))
                .sum();
            let pages = pages
                .into_iter()
                .map(|(url, body, next)| (url, (body, Some(total as u64), next)))
                .collect();
            let error = snapshot(config(json!({"recorders": []})), pages).unwrap_err();
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
                let mut issue = issue(page);
                issue["iid"] = page.into();
                issue["user_notes_count"] = 0.into();
                Ok(Response {
                    body: json!([issue]),
                    total: None,
                    next_page: Some(page + 1),
                })
            }
        }
        let error = Gitlab {
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
        // recorders means no note reads (Pages refuses unlisted URLs).
        let pages = BTreeMap::from([(
            format!("{API}?scope=all&state=opened&per_page=50&page=1"),
            (issues(0..3), Some(3), None),
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
    fn subgroup_project_paths_are_percent_encoded() {
        // GitLab answers only the %2F-encoded project path, so the fake
        // transport answers only these URLs: an unencoded request, or one
        // that a relative join normalized back to plain slashes, fails
        // the test.
        let mut issue = issues(0..1).as_array().unwrap()[0].clone();
        issue["references"]["full"] = "group/subgroup/project#1".into();
        issue["user_notes_count"] = 0.into();
        let pages = BTreeMap::from([(
            "https://gitlab.example/api/v4/projects/group%2Fsubgroup%2Fproject/issues\
             ?scope=all&state=opened&per_page=50&page=1"
                .to_owned(),
            (json!([issue]), Some(1), None),
        )]);
        let config: Config = serde_json::from_value(json!({
            "url": "https://gitlab.example/",
            "repositories": ["group/subgroup/project"],
            "membership": "open",
        }))
        .unwrap();
        let made = Gitlab {
            transport: Pages(pages),
            config,
        }
        .snapshot_at(
            "2026-10-20T00:00:00Z".into(),
            &board_core::AgentPolicy::parse(board_core::DEFAULT_AGENT_POLICY).unwrap(),
        )
        .unwrap();
        assert_eq!(made.items.len(), 1);
        assert_eq!(made.items[0].identity.repository, "group/subgroup/project");
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
            "https://elsewhere.example/api/v4/projects/sample%2Fboard/issues",
            "https://gitlab.example/api/v3/projects",
            "https://gitlab.example.evil/api/v4/",
        ] {
            let error = client.get(&Url::parse(url).unwrap()).err().unwrap();
            assert!(error.to_string().contains("refusing"), "{url}");
        }
    }

    #[test]
    fn board_config_table() {
        for (value, ok) in [
            (json!({}), true),
            (json!({"board": {"forge": "github"}}), true),
            // GitLab needs repositories and membership like Forgejo.
            (json!({"board": {"forge": "gitlab"}}), false),
            (
                json!({"board": {"forge": "gitlab", "url": "https://gitlab.example/", "repositories": ["sample/board"], "membership": "open"}}),
                true,
            ),
            // The default instance is gitlab.com, and subgroups are paths.
            (
                json!({"board": {"forge": "gitlab", "repositories": ["group/subgroup/project"], "membership": "open"}}),
                true,
            ),
            (
                json!({"board": {"forge": "gitlab", "url": "https://gitlab.example/", "repositories": ["sample/board"], "membership": {"label": "agent-board"}, "tokenEnv": "BOARD_TOKEN", "recorders": [9]}}),
                true,
            ),
            // The GitHub token must not go to another host.
            (
                json!({"board": {"forge": "gitlab", "url": "https://gitlab.example/", "repositories": ["sample/board"], "membership": "open", "tokenEnv": "GITHUB_TOKEN"}}),
                false,
            ),
            (
                json!({"board": {"forge": "gitlab", "url": "https://gitlab.example/", "repositories": [], "membership": "open"}}),
                false,
            ),
            (
                json!({"board": {"forge": "gitlab", "url": "https://gitlab.example/", "repositories": ["justone"], "membership": "open"}}),
                false,
            ),
            (
                json!({"board": {"forge": "gitlab", "url": "https://gitlab.example/", "repositories": ["sample/board.git"], "membership": "open"}}),
                false,
            ),
            (
                json!({"board": {"forge": "gitlab", "url": "https://gitlab.example/", "repositories": ["/sample/board"], "membership": "open"}}),
                false,
            ),
            (
                json!({"board": {"forge": "gitlab", "url": "https://gitlab.example/", "repositories": ["sample/board", "Sample/Board"], "membership": "open"}}),
                false,
            ),
            (
                json!({"board": {"forge": "gitlab", "url": "https://gitlab.example/", "repositories": ["sample/board"], "membership": {"label": "priority/P1"}}}),
                false,
            ),
            (
                json!({"board": {"forge": "gitlab", "url": "https://gitlab.example/", "repositories": ["sample/board"], "membership": "open", "recorders": [0]}}),
                false,
            ),
            (
                json!({"board": {"forge": "gitlab", "url": "http://user:pw@gitlab.example/", "repositories": ["sample/board"], "membership": "open"}}),
                false,
            ),
            (
                json!({"board": {"forge": "gitlab", "url": "https://gitlab.example/?a=b", "repositories": ["sample/board"], "membership": "open"}}),
                false,
            ),
            (
                json!({"board": {"forge": "gitlab", "url": "https://gitlab.example/", "repositories": ["sample/board"], "membership": "open", "extra": 1}}),
                false,
            ),
            (json!({"board": {"forge": "gitea"}}), false),
        ] {
            assert_eq!(Board::from_config(&value).is_ok(), ok, "{value}");
        }
        let Board::Gitlab(config) = Board::from_config(&json!({"board": {"forge": "gitlab", "url": "https://gitlab.example/git", "repositories": ["sample/board"], "membership": "open"}})).unwrap() else {
            panic!("not gitlab");
        };
        assert_eq!(
            config.api_root().unwrap().as_str(),
            "https://gitlab.example/git/api/v4/"
        );
        assert_eq!(config.token_env, DEFAULT_TOKEN_ENV);
        let Board::Gitlab(default) = Board::from_config(&json!({"board": {"forge": "gitlab", "repositories": ["sample/board"], "membership": "open"}})).unwrap() else {
            panic!("not gitlab");
        };
        assert_eq!(default.url.as_str(), "https://gitlab.com/");
    }

    #[test]
    fn plain_http_warns_outside_the_cluster() {
        assert!(
            config(json!({"url": "http://gitlab.example:3000/"}))
                .plain_http_warning()
                .is_some()
        );
        assert!(
            config(json!({"url": "https://gitlab.example/"}))
                .plain_http_warning()
                .is_none()
        );
    }
}
