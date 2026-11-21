//! Standalone promotion evidence preview, including the upstream
//! contribution-policy gate, plus explicitly opted-in bounded publication.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    github::Transport,
    overlap::validate_branch,
    pass::{self, Config},
    poll::{Rest, validate_repository},
};

const API: &str = "https://api.github.com/";
const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 10;

/// An unreadable response, distinct from a well-read fact holding promotion.
#[derive(Debug)]
pub(crate) struct ReadFailure(String);

impl std::fmt::Display for ReadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ReadFailure {}

#[derive(Debug)]
struct Held(String);

impl std::fmt::Display for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Held {}

#[derive(Debug)]
pub(crate) struct NotSelected(String);

#[derive(Debug)]
struct ScopeFailure(String);

impl std::fmt::Display for ScopeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScopeFailure {}

impl std::fmt::Display for NotSelected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotSelected {}

macro_rules! hold {
    ($condition:expr, $($message:tt)*) => {
        if !$condition { return Err(Held(format!($($message)*)).into()); }
    };
}

/// Cached failures retain their classification, not just rendered diagnostics.
#[derive(Clone)]
struct GateFailure {
    why: String,
    operational: bool,
}

impl std::fmt::Display for GateFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.why)
    }
}

impl GateFailure {
    fn policy(error: anyhow::Error) -> Self {
        Self {
            operational: !error.is::<Held>(),
            why: format!("{error:#}"),
        }
    }

    fn into_error(self) -> anyhow::Error {
        if self.operational {
            ReadFailure(self.why).into()
        } else {
            Held(self.why).into()
        }
    }
}

type Policies = BTreeMap<String, std::result::Result<policy::Permitted, GateFailure>>;

fn read_failure(error: anyhow::Error) -> anyhow::Error {
    if error.is::<crate::github::RateLimited>() || pass::budget_spent(&error) {
        return error;
    }
    ReadFailure(format!("{error:#}")).into()
}
pub const REMAINING: &str = "promote: preview only; publication requires promote.publish, --apply and --max-writes; fork closure not implemented; bot-fork heads require recorded staging verification";

/// Authenticated command selection and its observed branch binding, not output
/// parsed back into authority. The pusher is from the newest activity event.
#[derive(Debug, PartialEq, Eq)]
struct Command {
    url: String,
    author: String,
    created: OffsetDateTime,
    head: String,
    pushed: OffsetDateTime,
    /// When the head first became the branch tip: no review of it is older.
    introduced: OffsetDateTime,
    pusher: Option<String>,
    human_text: bool,
}

impl std::fmt::Display for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "command={} at={} human-text={} push={}",
            self.url,
            self.created.format(&Rfc3339).map_err(|_| std::fmt::Error)?,
            self.human_text,
            self.pushed.format(&Rfc3339).map_err(|_| std::fmt::Error)?
        )
    }
}

/// What the candidate read gates found, before the policy gate.
enum Gates {
    /// The full line: an open upstream PR already has this head.
    Promoted(String),
    /// Every read gate passed: the commit summary and the gate details.
    Ready {
        summary: String,
        detail: String,
        signoffs: usize,
        /// Commits the base has that the head lacks: a publication rebase.
        behind: u64,
        /// The fork's own commits the head carries, which publication drops.
        fork_only: usize,
        /// Publishing the head itself would carry exactly `commits`.
        direct: bool,
        /// The upstream tip compared, when the comparison was pinned to one.
        tip: Option<String>,
        /// The PR's own commits. Publication replays and confirms this one
        /// value, never a second read of the range.
        commits: rewrite::Commits,
    },
}

/// Shared read-only assessment; rendered CLI lines are never parsed as facts.
fn assess(
    client: &(impl Rest + Transport),
    config: &Config,
    repo: &str,
    pull: &Value,
    target: &pass::promote::Fork,
    command: &Command,
    policies: &mut Policies,
) -> Result<(Gates, std::result::Result<&'static str, GateFailure>)> {
    let gate = gates(
        client,
        config,
        repo,
        pull,
        (&target.upstream, &target.base),
        command.introduced,
    )?;
    if matches!(gate, Gates::Promoted(_)) {
        return Ok((gate, Ok("")));
    }
    authorized_promotion_body(client, config, repo, pull, &target.upstream, command)?;
    let key = target.upstream.to_ascii_lowercase();
    if !policies.contains_key(&key) {
        let permission = match policy::gate(client, config, &target.upstream) {
            Err(error) if pass::budget_spent(&error) => return Err(error),
            result => result,
        };
        policies.insert(key.clone(), permission.map_err(GateFailure::policy));
    }
    let policy = policies.get(&key).context("promotion policy observation")?;
    let verdict = match policy {
        Ok(policy::Permitted::BotOk) => Ok("bot-ok"),
        Ok(policy::Permitted::HumanText) => {
            let verified = match provenance::verify(client, config, repo, pull, command) {
                Err(error) if pass::budget_spent(&error) => return Err(error),
                result => result,
            };
            verified
                .map(|()| "human-text; provenance verified")
                .map_err(|e| GateFailure {
                    operational: !e.is::<Held>(),
                    why: format!("needs-text: {e:#}"),
                })
        }
        Err(why) => Err(why.clone()),
    };
    Ok((gate, verdict))
}

/// The REST response fields a recorded preview keeps: those this evaluator
/// reads, not avatars, descriptions or other fields it never reads.
const RECORDED_FIELDS: &[&str] = &[
    // requestShare sizes standalone previews from this free read too.
    "resources",
    "core",
    "graphql",
    "remaining",
    "reset",
    "full_name",
    "fork",
    "archived",
    "private",
    "has_issues",
    "parent",
    "source",
    "owner",
    "login",
    "user",
    "number",
    "head",
    "base",
    "repo",
    "ref",
    "sha",
    "state",
    "draft",
    "labels",
    "name",
    "id",
    "created_at",
    "updated_at",
    "submitted_at",
    "timestamp",
    "before",
    "after",
    "activity_type",
    "actor",
    "title",
    "body",
    "repository_url",
    "pull_request",
    "merged",
    "merged_at",
    "ahead_by",
    "behind_by",
    "total_commits",
    "merge_base_commit",
    "base_commit",
    "commits",
    "commit",
    "message",
    "parents",
    "author",
    "protected",
    "protection",
    "enabled",
    "required_status_checks",
    "checks",
    "contexts",
    "context",
    "app_id",
    "integration_id",
    "enforcement_level",
    "type",
    "parameters",
    "check_runs",
    "total_count",
    // An approval trigger: the timeline's draft conversions.
    "event",
    // The policy gate: record files, listings, history and trees.
    "path",
    "size",
    "encoding",
    "content",
    "committer",
    "verification",
    "verified",
    "default_branch",
    "custom_properties",
    "origin",
    "topics",
    "merge_commit_sha",
    "commit_id",
    "tree",
    "mode",
    "truncated",
];

/// A [`crate::pass::Recorder`] fixture projected to [`RECORDED_FIELDS`] in
/// REST bodies. Statuses, links, typed absence and GraphQL answers (queried
/// for exactly their evidence) are kept whole. A projection, not scenario data.
pub fn recording(fixture: &str) -> Result<String> {
    fn project(value: &mut Value) {
        match value {
            Value::Array(rows) => rows.iter_mut().for_each(project),
            Value::Object(object) => {
                object.retain(|key, _| RECORDED_FIELDS.contains(&key.as_str()));
                object.values_mut().for_each(project);
            }
            _ => {}
        }
    }
    let mut value: Value = serde_json::from_str(fixture).context("recorded fixture")?;
    for entry in value["requests"]
        .as_array_mut()
        .context("recorded requests")?
    {
        if let Some(body) = entry["answer"].get_mut("body") {
            project(body);
        }
    }
    Ok(serde_json::to_string_pretty(&value)?)
}

pub fn enabled(config: &Config) -> bool {
    config
        .promote
        .as_ref()
        .is_some_and(|p| !p.forks.is_empty() || config.midstream.is_some())
}

/// The legacy map is readable for pinned callers, but cannot authorize a
/// destination. Only GitHub's parent and midstream metadata can do that.
pub(crate) fn target(
    metadata: &Value,
    repo: &str,
    legacy: Option<&pass::promote::Fork>,
) -> Result<Option<pass::promote::Fork>> {
    target_impl(metadata, repo, legacy).map_err(|error| ScopeFailure(format!("{error:#}")).into())
}

fn target_impl(
    metadata: &Value,
    repo: &str,
    legacy: Option<&pass::promote::Fork>,
) -> Result<Option<pass::promote::Fork>> {
    repository(metadata, repo)?;
    ensure!(
        metadata["fork"]
            .as_bool()
            .context("missing repository fork flag")?,
        "promotion requires a GitHub fork: {repo}"
    );
    let upstream = metadata["parent"]["full_name"]
        .as_str()
        .context("missing fork parent")?;
    validate_repository(upstream)?;
    ensure!(
        !upstream.eq_ignore_ascii_case(repo),
        "fork is its own parent"
    );
    if let Some(legacy) = legacy {
        ensure!(
            legacy.upstream.eq_ignore_ascii_case(upstream),
            "promote metadata disagrees with fork parent for {repo}"
        );
    }
    let topics = metadata["topics"]
        .as_array()
        .context("missing repository topics")?;
    ensure!(
        topics.iter().all(Value::is_string),
        "invalid repository topics"
    );
    let properties = metadata["custom_properties"]
        .as_object()
        .context("missing repository custom properties")?;
    ensure!(
        properties.get("origin").is_none_or(Value::is_string),
        "invalid origin property"
    );
    if properties.get("origin") != Some(&Value::String("fork".into()))
        || !topics.iter().any(|topic| topic == "bot-midstream")
    {
        return Ok(None);
    }
    let base = match legacy {
        Some(legacy) => legacy.base.as_str(),
        None => metadata["parent"]["default_branch"]
            .as_str()
            .context("missing parent default branch")?,
    };
    validate_branch(base)?;
    Ok(Some(pass::promote::Fork {
        upstream: upstream.into(),
        base: base.into(),
    }))
}

/// Discovery is bounded by the existing midstream listing and read budget.
/// Explicit legacy entries are checked too, so non-forks fail rather than skip.
pub(crate) fn targets(
    client: &impl Rest,
    config: &Config,
) -> Result<BTreeMap<String, pass::promote::Fork>> {
    let settings = config.promote.as_ref().context("promote configuration")?;
    let mut names: BTreeMap<_, _> = settings
        .forks
        .keys()
        .map(|name| (name.to_ascii_lowercase(), name.clone()))
        .collect();
    if let Some(midstream) = &config.midstream {
        for name in midstream.list(client, true, |value, _| {
            value["topics"]
                .as_array()
                .is_some_and(|topics| topics.iter().any(|topic| topic == "bot-midstream"))
                && value["custom_properties"]["origin"] == "fork"
        })? {
            names.insert(name.to_ascii_lowercase(), name);
        }
    }
    let mut found = BTreeMap::new();
    for repo in names.into_values() {
        let metadata = get(client, &format!("repos/{repo}"))?;
        let legacy = settings
            .forks
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&repo))
            .map(|(_, target)| target);
        if let Some(target) = target(&metadata, &repo, legacy)? {
            found.insert(repo, target);
        }
    }
    Ok(found)
}

/// No forge reads at all when off. Errors are never interpreted as an empty list.
pub fn preview<C: Rest + Transport>(
    client: &C,
    config: &Config,
    now: OffsetDateTime,
) -> Result<Vec<String>> {
    if !enabled(config) {
        return Ok(vec!["promote: off".into()]);
    }
    ensure!(
        config.forgejo_board().is_none(),
        "promote supports GitHub only"
    );
    ensure!(
        !config.bot.eq_ignore_ascii_case(&config.operator),
        "promote operator and bot must differ"
    );
    pass::promotion_reads(client, config, |client, lines| {
        // Each upstream's policy is read at most once per run.
        let mut policies = BTreeMap::new();
        for (repo, target) in &targets(client, config)? {
            validate_repository(repo)?;
            validate_repository(&target.upstream)?;
            validate_branch(&target.base)?;
            let metadata = get(client, &format!("repos/{repo}"))?;
            repository(&metadata, repo)?;
            let network = in_network(&metadata, &target.upstream);
            let mut pulls = pages(
                client,
                &format!("repos/{repo}/pulls?state=open&sort=created&direction=asc"),
            )?;
            pulls.sort_by_key(|p| p["number"].as_u64());
            let mut numbers = BTreeSet::new();
            for pull in pulls {
                let number = pull["number"]
                    .as_u64()
                    .filter(|n| *n > 0)
                    .context("invalid pull number")?;
                ensure!(numbers.insert(number), "duplicate pull {repo}#{number}");
                let head = pull
                    .pointer("/head/sha")
                    .and_then(Value::as_str)
                    .filter(|h| oid(h))
                    .context("invalid pull head")?;
                let evidence = evaluate(
                    client,
                    config,
                    (repo, number),
                    &pull,
                    (network, &target.upstream),
                    now,
                );
                match evidence {
                    Ok(command) => {
                        match assess(client, config, repo, &pull, target, &command, &mut policies) {
                            Ok((Gates::Promoted(result), _)) => {
                                lines.push(format!("{result}; head={head} {command}; staging verification required for bot-fork head"));
                            }
                            Ok((
                                Gates::Ready {
                                    summary, detail, ..
                                },
                                permission,
                            )) => {
                                lines.push(match permission {
                                    Ok(verdict) => format!(
                                          "would promote {repo}#{number} -> {} ({summary}) (policy {verdict}); preview only; publication revalidates approval and all gates; {detail}; head={head} {command}; staging verification required for bot-fork head",
                                         target.upstream
                                    ),
                                    Err(why) => format!("hold promote {repo}#{number} head={head}: {why}"),
                                });
                            }
                            Err(error) if pass::budget_spent(&error) => return Err(error.context(format!("evaluating {repo}#{number}; remaining candidates and forks not evaluated"))),
                            Err(error) => lines.push(format!(
                                "hold promote {repo}#{number} head={head} {command}: {error:#}"
                            )),
                        }
                    }
                    Err(error) if pass::budget_spent(&error) => return Err(error.context(format!("evaluating {repo}#{number}; remaining candidates and forks not evaluated"))),
                    Err(error) => lines.push(format!(
                        "hold promote {repo}#{number} head={head} command=unverified: {error:#}"
                    )),
                }
            }
        }
        lines.push(REMAINING.into());
        Ok(())
    })
}

fn get(client: &impl Rest, path: &str) -> Result<Value> {
    let response = client.get(&format!("{API}{path}"), None)?;
    ensure!(
        response.status == 200,
        "GET {path}: HTTP {}",
        response.status
    );
    Ok(response.body)
}

/// Construct page paths locally: a forged Link cannot redirect credentials.
/// A full last page without a next link is ambiguous and fails closed.
fn pages(client: &impl Rest, path: &str) -> Result<Vec<Value>> {
    object_pages(client, path, None)
}

fn object_pages(client: &impl Rest, path: &str, field: Option<&str>) -> Result<Vec<Value>> {
    let mut values = Vec::new();
    for page in 1..=MAX_PAGES {
        let separator = if path.contains('?') { '&' } else { '?' };
        let url = format!("{API}{path}{separator}per_page={PAGE_SIZE}&page={page}");
        let response = client.get(&url, None)?;
        ensure!(
            response.status == 200,
            "GET {path}: HTTP {}",
            response.status
        );
        let rows = field
            .map_or(&response.body, |key| &response.body[key])
            .as_array()
            .context("listing is not an array")?;
        ensure!(rows.len() <= PAGE_SIZE, "oversized listing");
        let more = pagination(response.link.as_deref())?;
        ensure!(!more || !rows.is_empty(), "empty intermediate page");
        values.extend(rows.iter().cloned());
        if !more {
            ensure!(rows.len() < PAGE_SIZE, "listing may be truncated");
            if field == Some("check_runs") {
                ensure!(
                    response.body["total_count"].as_u64() == Some(values.len() as u64),
                    "incomplete check runs"
                );
            }
            return Ok(values);
        }
    }
    anyhow::bail!("listing exceeds {MAX_PAGES} pages")
}

fn encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// These gates prevent duplicate publication, stale bases, unmerged dependencies,
/// unsafe sign-off rewrites and repository-relative text changing meaning.
fn gates(
    client: &(impl Rest + Transport),
    config: &Config,
    repo: &str,
    pull: &Value,
    target: (&str, &str),
    introduced: OffsetDateTime,
) -> Result<Gates> {
    gates_impl(
        client,
        config,
        repo,
        pull,
        target,
        introduced,
        Mode::Preview,
    )
}

enum Mode {
    /// An open upstream PR at the head reads as promoted.
    Preview,
    /// Pinned to one upstream tip. Revalidation passes the PR's commits as
    /// its first validation computed them, never deriving them again.
    Publication(Option<rewrite::Commits>),
}

fn gates_impl(
    client: &(impl Rest + Transport),
    config: &Config,
    repo: &str,
    pull: &Value,
    (upstream, base): (&str, &str),
    introduced: OffsetDateTime,
    mode: Mode,
) -> Result<Gates> {
    let (observe_existing, prior) = match mode {
        Mode::Preview => (true, None),
        Mode::Publication(prior) => (false, prior),
    };
    validate_repository(repo)?;
    validate_repository(upstream)?;
    validate_branch(base)?;
    let number = pull["number"].as_u64().context("invalid pull number")?;
    let owner = pull["head"]["repo"]["owner"]["login"]
        .as_str()
        .context("missing head owner")?;
    let branch = pull["head"]["ref"]
        .as_str()
        .context("missing head branch")?;
    validate_branch(branch)?;
    let head = pull["head"]["sha"]
        .as_str()
        .filter(|s| oid(s))
        .context("invalid head sha")?;
    let existing = upstream_pulls(client, upstream, owner, branch)?;
    if !existing.is_empty() {
        ensure!(
            existing.len() == 1,
            "multiple upstream PRs for head {owner}:{branch}"
        );
        let p = &existing[0];
        let n = p["number"]
            .as_u64()
            .filter(|n| *n > 0)
            .context("invalid upstream pull number")?;
        hold!(
            p["state"] == "open",
            "upstream {upstream}#{n} is closed/merged"
        );
        hold!(
            p["head"]["sha"] == head,
            "upstream {upstream}#{n} has a different head"
        );
        if observe_existing {
            return Ok(Gates::Promoted(format!(
                "already promoted {repo}#{number} -> {upstream}#{n}"
            )));
        }
    }
    // Publication pins its comparison to one upstream tip, so the range this
    // proves is the one it replays and confirms.
    let (compare, tip) = if observe_existing {
        let path = format!(
            "repos/{upstream}/compare/{}...{}",
            encode(base),
            encode(&format!("{owner}:{branch}"))
        );
        (comparison(client, &path, head)?, None)
    } else {
        let tip = branch_tip(client, upstream, base)?;
        let compare = comparison(
            client,
            &format!("repos/{upstream}/compare/{tip}...{head}"),
            head,
        )?;
        ensure!(
            compare["base_commit"]["sha"] == tip,
            "upstream comparison base mismatch"
        );
        (compare, Some(tip))
    };
    let ahead = compare["ahead_by"].as_u64().context("missing ahead_by")?;
    let behind = compare["behind_by"].as_u64().context("missing behind_by")?;
    hold!(ahead > 0, "already upstream (zero ahead)");
    let fork_base = base_settled(client, repo, pull, introduced)?;
    let mut commits = match prior {
        Some(commits) => {
            hold!(
                commits.fork_base == fork_base && commits.head() == head,
                "fork base or head changed before publication; retry with fresh evidence"
            );
            commits
        }
        None => rewrite::own(client, repo, &fork_base, head)?,
    };
    let upstream_shas = commits.unintegrated(&compare)?;
    let direct = upstream_shas == commits.shas();
    let fork_only = upstream_shas.len().saturating_sub(commits.shas().len());
    // Publication replays the PR's own commits onto the current base when the
    // head is behind it or carries the fork's own commits; only a conflict,
    // found by that replay, is left for --apply to hold.
    if behind > 0 || !direct {
        rewrite::replayable(config, &commits)?;
    }
    let stack = stack_gate(client, config, repo, pull, upstream, base)?;
    let dco = dco_gate(client, config, upstream, base, &commits)?;
    publication_text(repo, upstream, pull)?;
    let rebase = if behind > 0 || !direct {
        let skipped = if fork_only > 0 {
            format!(", skipping {fork_only} fork commits")
        } else {
            String::new()
        };
        format!(
            "; publication rebase onto {base} (behind_by={behind}{skipped}), held by --apply on conflict"
        )
    } else {
        String::new()
    };
    Ok(Gates::Ready {
        summary: dco.1,
        detail: format!("{stack}; {}{rebase}", dco.0),
        signoffs: dco.2,
        behind,
        fork_only,
        direct,
        tip,
        commits,
    })
}

const BASE_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){nameWithOwner pullRequest(number:$number){number baseRefName baseRef{target{oid}} baseChange:timelineItems(last:1,itemTypes:[BASE_REF_CHANGED_EVENT]){nodes{... on BaseRefChangedEvent{createdAt}}}}}}";

/// The fork base the PR's commits are cut at: the live tip of the base selected
/// before the head was `introduced`, so every review of that head showed this
/// range. Approval times cannot bind it: a review submitted after a base move
/// may come from a diff loaded before it. A later retarget or tip move holds
/// for a new head rather than proving the PR's commits unchanged with a second
/// derivation.
fn base_settled(
    client: &(impl Rest + Transport),
    repo: &str,
    pull: &Value,
    introduced: OffsetDateTime,
) -> Result<String> {
    let branch = pull["base"]["ref"]
        .as_str()
        .context("missing base branch")?;
    validate_branch(branch)?;
    let reference = format!("refs/heads/{branch}");
    let activity = format!(
        "repos/{repo}/activity?ref={}&direction=desc",
        encode(&reference)
    );
    let latest = get(client, &format!("{activity}&per_page=1"))?;
    let event = latest
        .as_array()
        .context("branch activity is not an array")?
        .first();
    let (owner, name) = repo.split_once('/').context("invalid fork repository")?;
    let number = pull["number"].as_u64().context("invalid pull number")?;
    let evidence = client.graphql(
        BASE_QUERY,
        serde_json::json!({"owner":owner,"name":name,"number":number}),
    )?;
    let repository = &evidence["repository"];
    let pr = &repository["pullRequest"];
    hold!(
        repository["nameWithOwner"]
            .as_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(repo))
            && pr["number"] == number
            && pr["baseRefName"] == branch,
        "fork PR base changed during validation; retry with fresh evidence"
    );
    // The REST PR's base SHA can lag a branch push. Read the live ref and
    // require independent activity setting it to exactly that tip.
    let sha = pr["baseRef"]["target"]["oid"]
        .as_str()
        .filter(|s| oid(s))
        .context("invalid live fork base SHA")?;
    hold!(
        event.is_some_and(|e| e["ref"] == reference && e["after"] == sha),
        "fork base {branch} has no activity setting it to {sha}"
    );
    let events = pr["baseChange"]["nodes"]
        .as_array()
        .context("missing base change evidence")?;
    ensure!(events.len() <= 1, "oversized base change evidence");
    if let Some(event) = events.first() {
        hold!(
            at(&event["createdAt"])? < introduced,
            "fork PR was retargeted after its head was pushed; push a new head and approve it"
        );
    }
    let event = event.context("missing base activity")?;
    hold!(
        at(&event["timestamp"])? < introduced,
        "fork base {branch} moved after the PR's head was pushed; push a new head and approve it"
    );
    Ok(sha.to_owned())
}

mod drafts;
pub(crate) mod handoff;
mod publish;
mod repair;
mod rewrite;
mod text;

pub use drafts::repair_drafts;
pub use publish::apply;
pub use repair::repair;
pub(crate) use text::midstream_brief;

fn comparison(client: &impl Rest, path: &str, head: &str) -> Result<Value> {
    let mut first: Option<Value> = None;
    let mut commits = Vec::new();
    for page in 1..=MAX_PAGES {
        let response = client.get(
            &format!("{API}{path}?per_page={PAGE_SIZE}&page={page}"),
            None,
        )?;
        ensure!(
            response.status == 200,
            "GET {path}: HTTP {}",
            response.status
        );
        let body = response.body;
        if let Some(first) = &first {
            for key in [
                "ahead_by",
                "behind_by",
                "total_commits",
                "merge_base_commit",
            ] {
                ensure!(body[key] == first[key], "compare changed during pagination");
            }
        } else {
            first = Some(body.clone());
        }
        let rows = body["commits"]
            .as_array()
            .context("missing compare commits")?;
        ensure!(rows.len() <= PAGE_SIZE, "oversized compare page");
        let more = pagination(response.link.as_deref())?;
        ensure!(!more || !rows.is_empty(), "empty intermediate compare page");
        commits.extend(rows.iter().cloned());
        if !more {
            let mut result = first.context("missing compare")?;
            ensure!(
                result["total_commits"].as_u64() == Some(commits.len() as u64)
                    && result["ahead_by"] == result["total_commits"],
                "incomplete compare commits"
            );
            ensure!(
                result["merge_base_commit"]["sha"].as_str().is_some_and(oid),
                "invalid merge base sha"
            );
            let mut seen = BTreeSet::new();
            for c in &commits {
                let sha = c["sha"]
                    .as_str()
                    .filter(|s| oid(s))
                    .context("invalid compare commit sha")?;
                ensure!(seen.insert(sha), "duplicate compare commit");
            }
            ensure!(
                commits.is_empty() || commits.last().is_some_and(|c| c["sha"] == head),
                "compare head mismatch"
            );
            result["commits"] = Value::Array(commits);
            return Ok(result);
        }
    }
    anyhow::bail!("compare exceeds {MAX_PAGES} pages")
}

fn branch_tip(client: &impl Rest, repo: &str, branch: &str) -> Result<String> {
    let branch = get(client, &format!("repos/{repo}/branches/{}", encode(branch)))?;
    branch["commit"]["sha"]
        .as_str()
        .filter(|s| oid(s))
        .map(str::to_owned)
        .context("invalid publication base")
}

/// One pinned compare summary proves ancestry without fetching the range.
fn ancestor(client: &impl Rest, repo: &str, base: &str, head: &str) -> Result<bool> {
    if base == head {
        return Ok(true);
    }
    let proof = get(
        client,
        &format!("repos/{repo}/compare/{base}...{head}?per_page=1"),
    )?;
    Ok(proof["base_commit"]["sha"] == base
        && proof["merge_base_commit"]["sha"] == base
        && proof["behind_by"].as_u64() == Some(0))
}

/// The upstream tip, held unless it still contains `base`: the commit a
/// publication sits on, without which it would carry upstream's old commits.
fn tip_containing(client: &impl Rest, target: &pass::promote::Fork, base: &str) -> Result<String> {
    let tip = branch_tip(client, &target.upstream, &target.base)?;
    hold!(
        ancestor(client, &target.upstream, base, &tip)?,
        "upstream no longer contains the publication's base {base}; manual migration required"
    );
    Ok(tip)
}

fn upstream_pulls(
    client: &impl Rest,
    upstream: &str,
    owner: &str,
    branch: &str,
) -> Result<Vec<Value>> {
    validate_repository(upstream)?;
    validate_repository(&format!("{owner}/head"))?;
    validate_branch(branch)?;
    let rows = pages(
        client,
        &format!(
            "repos/{upstream}/pulls?state=all&head={}",
            encode(&format!("{owner}:{branch}"))
        ),
    )?;
    for p in &rows {
        ensure!(
            p["head"]["ref"] == branch
                && p["head"]["repo"]["owner"]["login"]
                    .as_str()
                    .is_some_and(|o| o.eq_ignore_ascii_case(owner)),
            "upstream head identity mismatch"
        );
        ensure!(
            p["head"]["sha"].as_str().is_some_and(oid),
            "invalid upstream head sha"
        );
    }
    Ok(rows)
}

fn referenced_numbers(text: &str) -> BTreeSet<u64> {
    text.split(['#'])
        .skip(1)
        .chain(text.split("/issues/").skip(1))
        .filter_map(|s| {
            s.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .ok()
        })
        .filter(|n| *n > 0)
        .collect()
}

fn stack_gate(
    client: &(impl Rest + Transport),
    config: &Config,
    repo: &str,
    pull: &Value,
    upstream: &str,
    base: &str,
) -> Result<String> {
    let issue = source_issue(repo, pull)?;
    promotion_body(client, repo, pull, upstream)?;
    let comments = crate::plan::issue_comments_with_edit_evidence(client, &issue)?;
    let writers = [&*config.operator, &*config.bot];
    if crate::plan::no_plan(&comments, &writers) {
        return Ok("stack=none (no plan)".into());
    }
    let (plan, _) =
        crate::plan::newest_plan(&comments, &writers).map_err(|e| Held(format!("{e:#}")))?;
    hold!(
        plan.repo.eq_ignore_ascii_case(repo) && plan.item == issue.number,
        "accepted plan issue mismatch"
    );
    let Some(lower) = plan.stacks_on else {
        return Ok("stack=none".into());
    };
    let parents = pages(
        client,
        &format!("repos/{repo}/pulls?state=all&sort=created&direction=asc"),
    )?;
    let parents: Vec<_> = parents
        .iter()
        .filter(|p| {
            let body = p["body"].as_str().unwrap_or_default();
            p["user"]["login"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&config.bot))
                && (crate::stack::closes(body, repo, lower)
                    || crate::stack::part_of(body, repo, lower))
        })
        .collect();
    hold!(
        parents.len() == 1,
        "stack parent {repo}#{lower} has no unique fork PR"
    );
    let parent = parents[0];
    let parent_head = parent["head"]["sha"]
        .as_str()
        .filter(|s| oid(s))
        .context("invalid parent head sha")?;
    let n = parent["number"]
        .as_u64()
        .filter(|n| *n > 0)
        .context("invalid parent PR number")?;
    let owner = parent["head"]["repo"]["owner"]["login"]
        .as_str()
        .context("missing parent head owner")?;
    let branch = parent["head"]["ref"]
        .as_str()
        .context("missing parent head branch")?;
    let prs = upstream_pulls(client, upstream, owner, branch)?;
    ensure!(
        prs.len() <= 1,
        "stack parent {repo}#{n} has multiple upstream PRs"
    );
    if let Some(p) = prs.first() {
        let pn = p["number"]
            .as_u64()
            .filter(|n| *n > 0)
            .context("invalid parent upstream PR number")?;
        if p["state"] == "closed" && p["merged_at"].is_string() {
            hold!(
                p["base"]["ref"] == base,
                "stack parent {upstream}#{pn}: merged into a different base, not {base}"
            );
            hold!(
                p["head"]["sha"] == parent_head,
                "stack parent {upstream}#{pn}: merged head differs from fork parent {repo}#{n}"
            );
            return Ok(format!("stack={repo}#{n} -> {upstream}#{pn} merged"));
        }
        hold!(
            p["state"] != "open",
            "stack parent {upstream}#{pn}: open upstream PR"
        );
        return Err(Held(format!(
            "stack parent {upstream}#{pn}: closed unmerged; rebase by hand"
        ))
        .into());
    }
    let comments = pages(client, &format!("repos/{repo}/issues/{n}/comments"))?;
    let target = pass::promote::Fork {
        upstream: upstream.into(),
        base: base.into(),
    };
    if let Some(handoff::Publication::Current(pr)) =
        handoff::published(client, &config.bot, repo, &target, parent, &comments)?
    {
        let pn = pr["number"]
            .as_u64()
            .context("invalid parent upstream PR number")?;
        hold!(
            pr["state"] == "closed" && pr["merged"] == true && pr["merged_at"].is_string(),
            "stack parent {upstream}#{pn}: publication has not merged"
        );
        return Ok(format!("stack={repo}#{n} -> {upstream}#{pn} merged"));
    }
    hold!(
        parent["state"] != "open",
        "stack parent {repo}#{n}: open fork PR"
    );
    Err(Held(format!(
        "stack parent {repo}#{n}: closed unmerged; rebase by hand"
    ))
    .into())
}

fn source_issue(repo: &str, pull: &Value) -> Result<board_core::ItemRef> {
    let text = pull["body"]
        .as_str()
        .context("cannot tie fork PR to issue: missing body")?;
    let issues: Vec<_> = referenced_numbers(text)
        .into_iter()
        .filter(|n| crate::stack::closes(text, repo, *n) || crate::stack::part_of(text, repo, *n))
        .collect();
    hold!(
        issues.len() == 1,
        "cannot tie fork PR to one issue deterministically{}",
        if text.trim().is_empty() {
            ": empty body"
        } else {
            ""
        }
    );
    crate::parse_github_item(
        &format!("{repo}#{}", issues[0]),
        None,
        crate::github::DEFAULT_HOST,
    )
}

/// The approved title and body as upstream would read them: the body less its
/// standalone source line and generated footer ([`text::strip`]), and both held unless publishable.
fn publication_text(repo: &str, upstream: &str, pull: &Value) -> Result<String> {
    let source = source_issue(repo, pull)?;
    let body = pull["body"].as_str().context("missing pull body")?;
    let published = text::strip(repo, body, source.number)?;
    hold!(
        !published.trim().is_empty(),
        "empty body{}",
        if published == body {
            ""
        } else {
            " without its source line"
        }
    );
    let title = pull["title"].as_str().context("missing pull title")?;
    for text in [title, &published] {
        text::check(repo, upstream, text)?;
    }
    Ok(published)
}

/// Only an entire source-reference line is bookkeeping. Never silently delete
/// surrounding approved prose, or reinterpret a bare reference upstream.
/// Hold rather than remove an example ([`text::top_level`]) that happens to
/// match the real source metadata.
fn source_text(repo: &str, pull: &Value) -> Result<String> {
    let source = source_issue(repo, pull)?;
    let text = pull["body"].as_str().context("missing pull body")?;
    let qualified = format!("{repo}#{}", source.number);
    let url = format!("https://github.com/{repo}/issues/{}", source.number);
    let mut removed = false;
    let mut body = String::new();
    for (at, line) in text.split_inclusive('\n').enumerate() {
        let words: Vec<_> = line.split_whitespace().collect();
        let reference = match words.as_slice() {
            [part, of, reference]
                if part.eq_ignore_ascii_case("part") && of.eq_ignore_ascii_case("of") =>
            {
                Some(*reference)
            }
            [keyword, reference]
                if [
                    "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves",
                    "resolved",
                ]
                .contains(&keyword.trim_end_matches(':').to_ascii_lowercase().as_str()) =>
            {
                Some(*reference)
            }
            _ => None,
        };
        let bookkeeping = reference.is_some_and(|r| {
            let r = r.trim_end_matches(['.', ',', ';', '!', '?']);
            r.eq_ignore_ascii_case(&qualified) || r.eq_ignore_ascii_case(&url)
        });
        if bookkeeping {
            hold!(
                text::top_level(text, at) && line.starts_with(|c: char| c.is_ascii_alphabetic()),
                "source metadata in code or HTML needs manual publication repair"
            );
            removed = true;
        } else {
            body.push_str(line);
        }
    }
    // Removing bookkeeping can join a dangling keyword to unrelated prose or
    // an upstream issue. Approval of the original text does not authorize that.
    hold!(
        !crate::stack::introduces_closing_reference(text, &body),
        "removing source metadata would introduce a closing reference; manual publication repair required"
    );
    Ok(if removed {
        body.trim_matches('\n').to_owned()
    } else {
        body
    })
}

/// Only this mechanically derived closing line supplements the approved text,
/// and only the source line and generated footer are removed. A qualified body link
/// cannot redirect publication or close another repo's issue.
fn promotion_body(client: &impl Rest, repo: &str, pull: &Value, upstream: &str) -> Result<String> {
    let source = source_issue(repo, pull)?;
    let held = |error: anyhow::Error| {
        if pass::budget_spent(&error) {
            error
        } else {
            Held(format!("upstream link held: {error:#}")).into()
        }
    };
    let target = crate::upstream::target(client, &source).map_err(held)?;
    let body = pull["body"].as_str().context("missing pull body")?;
    let mut body = text::strip(repo, body, source.number)?;
    if let Some(target) = target {
        hold!(
            target.repository.eq_ignore_ascii_case(upstream),
            "upstream link held: fork parent {} differs from configured target {upstream}",
            target.repository
        );
        crate::upstream::issue(client, &target.repository, target.number).map_err(held)?;
        body.push_str(&format!("\n\nFixes {}", target.display()));
    }
    Ok(body)
}

const SOURCE_TEXT_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){issue(number:$number){number url body createdAt author{login} lastEditedAt editor{login}}}}";

/// A closing reference is publication text, so mutable issue prose alone is
/// not authority. Require a trusted author/last editor and a change strictly
/// before the selected operator command, including on repeated publication.
fn authorized_promotion_body(
    client: &(impl Rest + Transport),
    config: &Config,
    repo: &str,
    pull: &Value,
    upstream: &str,
    command: &Command,
) -> Result<String> {
    let body = promotion_body(client, repo, pull, upstream)?;
    let published = publication_text(repo, upstream, pull)?;
    if body == published {
        return Ok(body);
    }
    let source = source_issue(repo, pull)?;
    let (owner, name) = repo.split_once('/').context("invalid source repository")?;
    let data = client
        .graphql(
            SOURCE_TEXT_QUERY,
            serde_json::json!({"owner":owner,"name":name,"number":source.number}),
        )
        .context("upstream link text evidence unreadable")?;
    let issue = &data["repository"]["issue"];
    hold!(
        issue["number"] == source.number
            && issue["url"] == format!("https://github.com/{repo}/issues/{}", source.number),
        "upstream link source text identity mismatch"
    );
    let number = crate::upstream::parse(issue["body"].as_str().context("missing source text")?)?
        .context("upstream link changed during publication")?;
    hold!(
        body == format!("{published}\n\nFixes {upstream}#{number}"),
        "upstream link changed during publication"
    );
    let edited = issue
        .get("lastEditedAt")
        .context("missing source edit metadata")?;
    let (actor, changed) = if edited.is_null() {
        hold!(
            issue.get("editor") == Some(&Value::Null),
            "ambiguous source editor"
        );
        (&issue["author"], at(&issue["createdAt"])?)
    } else {
        (&issue["editor"], at(edited)?)
    };
    hold!(
        actor["login"]
            .as_str()
            .is_some_and(|login| login.eq_ignore_ascii_case(&config.operator)
                || login.eq_ignore_ascii_case(&config.bot)),
        "upstream link source author/editor is not trusted"
    );
    hold!(
        changed < command.created,
        "upstream link source changed after command; fresh command required"
    );
    Ok(body)
}

fn dco_gate(
    client: &impl Rest,
    config: &Config,
    upstream: &str,
    base: &str,
    commits: &rewrite::Commits,
) -> Result<(String, String, usize)> {
    let rules = pages(
        client,
        &format!("repos/{upstream}/rules/branches/{}", encode(base)),
    )?;
    for rule in &rules {
        let kind = rule["type"].as_str().context("missing branch rule type")?;
        if kind == "required_status_checks" {
            ensure!(
                rule["parameters"]["required_status_checks"].is_array(),
                "unreadable required status checks"
            );
        }
    }
    let branch = get(
        client,
        &format!("repos/{upstream}/branches/{}", encode(base)),
    )?;
    let sha = branch["commit"]["sha"]
        .as_str()
        .filter(|s| oid(s))
        .context("invalid base head sha")?;
    let required = crate::poll::required_checks(&Value::Array(rules), &branch)?;
    let checks = object_pages(
        client,
        &format!("repos/{upstream}/commits/{sha}/check-runs"),
        Some("check_runs"),
    )?;
    let dco_name = |name: &str| {
        name.to_ascii_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|s| s == "dco")
    };
    let mut dco = false;
    for (rows, key) in [(&required, "context"), (&checks, "name")] {
        for row in rows {
            dco |= dco_name(row[key].as_str().context("missing check name")?);
        }
    }
    if !dco {
        return Ok(("dco=not-required".into(), "no DCO".into(), 0));
    }
    signoff_gate(config, commits).context("dco=required")
}

fn signoff_gate(config: &Config, own: &rewrite::Commits) -> Result<(String, String, usize)> {
    (|| {
        let commits = own.rows();
        let mut known = BTreeSet::from([own.parent()]);
        let signoff = config
            .promote
            .as_ref()
            .and_then(|p| p.signoff.as_ref())
            .context("missing sign-off identity")?;
        let trailer = format!("Signed-off-by: {} <{}>", signoff.name, signoff.email);
        let mut missing = 0;
        let mut problems = Vec::new();
        for c in commits {
            let sha = c["sha"]
                .as_str()
                .filter(|s| oid(s))
                .context("invalid commit sha")?;
            let message = c["commit"]["message"]
                .as_str()
                .context("missing commit message")?;
            let parents = c["parents"].as_array().context("missing commit parents")?;
            ensure!(
                parents.iter().all(|p| p["sha"].as_str().is_some_and(oid)),
                "invalid parent sha"
            );
            if !message
                .trim_end()
                .lines()
                .rev()
                .take_while(|l| !l.trim().is_empty())
                .any(|l| l.eq_ignore_ascii_case(&trailer))
            {
                missing += 1;
                if parents.len() != 1 {
                    problems.push(format!("{sha} is a merge commit"));
                }
                if !c["author"]["login"].as_str().is_some_and(|s| {
                    s.eq_ignore_ascii_case(&config.bot) || s.eq_ignore_ascii_case(&config.operator)
                }) {
                    problems.push(format!("{sha} is by neither bot nor operator"));
                }
            }
            ensure!(
                parents
                    .iter()
                    .all(|p| p["sha"].as_str().is_some_and(|s| known.contains(s))),
                "incomplete compare commit DAG"
            );
            known.insert(sha);
        }
        hold!(
            problems.is_empty(),
            "missing sign-off on {missing} of {} commits: {}",
            commits.len(),
            problems.join(", ")
        );
        Ok((
            "dco=required".into(),
            format!("sign-off on {missing} of {} commits", commits.len()),
            missing,
        ))
    })()
}

/// Interpret and validate the same grammar, so malformed `next` cannot be
/// mistaken for the end of the cancellation evidence. Targets are never followed.
fn pagination(header: Option<&str>) -> Result<bool> {
    let Some(header) = header else {
        return Ok(false);
    };
    let mut relations = BTreeSet::new();
    for part in header.split(',') {
        let (target, relation) = part
            .trim()
            .split_once(';')
            .context("malformed pagination evidence")?;
        let target = target
            .trim()
            .strip_prefix('<')
            .and_then(|s| s.strip_suffix('>'))
            .context("malformed pagination evidence")?;
        url::Url::parse(target).context("malformed pagination target")?;
        let relation = relation
            .trim()
            .strip_prefix("rel=\"")
            .and_then(|s| s.strip_suffix('"'))
            .context("malformed pagination evidence")?;
        ensure!(
            ["next", "prev", "first", "last"].contains(&relation) && relations.insert(relation),
            "ambiguous pagination evidence"
        );
    }
    Ok(relations.contains("next"))
}

fn oid(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn repository(value: &Value, expected: &str) -> Result<()> {
    let name = value["full_name"]
        .as_str()
        .context("missing repository identity")?;
    validate_repository(name)?;
    ensure!(
        name.eq_ignore_ascii_case(expected),
        "repository identity mismatch"
    );
    Ok(())
}

fn in_network(value: &Value, upstream: &str) -> bool {
    value["fork"] == true
        && ["parent", "source"].iter().any(|key| {
            value[*key]["full_name"]
                .as_str()
                .is_some_and(|r| r.eq_ignore_ascii_case(upstream))
        })
}

fn observed_network(value: &Value, upstream: &str) -> Result<bool> {
    let fork = value["fork"]
        .as_bool()
        .context("missing repository fork flag")?;
    if fork {
        ensure!(
            ["parent", "source"]
                .iter()
                .any(|k| value[*k]["full_name"].is_string()),
            "missing repository network identity"
        );
    }
    Ok(in_network(value, upstream))
}

fn at(value: &Value) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(value.as_str().context("missing evidence time")?, &Rfc3339)
        .context("invalid evidence time")
}

fn evaluate(
    client: &(impl Rest + Transport),
    config: &Config,
    (repo, number): (&str, u64),
    pull: &Value,
    (network, upstream): (bool, &str),
    now: OffsetDateTime,
) -> Result<Command> {
    hold!(network, "fork-network mismatch");
    hold!(pull["state"] == "open", "pull is not open");
    repository(&pull["base"]["repo"], repo)?;
    hold!(
        pull["user"]["login"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(&config.bot)),
        "not bot-authored"
    );
    let head_repo = pull["head"]["repo"]["full_name"]
        .as_str()
        .context("missing head repository")?;
    validate_repository(head_repo)?;
    let owner = pull["head"]["repo"]["owner"]["login"]
        .as_str()
        .context("missing head owner")?;
    ensure!(
        head_repo
            .split_once('/')
            .is_some_and(|(o, _)| o.eq_ignore_ascii_case(owner)),
        "head owner mismatch"
    );
    let branch = pull["head"]["ref"]
        .as_str()
        .context("missing head branch")?;
    validate_branch(branch)?;
    hold!(
        (owner.eq_ignore_ascii_case(&config.bot) || head_repo.eq_ignore_ascii_case(repo))
            && (branch.starts_with("bot/") || branch.starts_with("dispatch/")),
        "not a bot-owned branch"
    );
    let head_metadata = get(client, &format!("repos/{head_repo}"))?;
    repository(&head_metadata, head_repo)?;
    hold!(
        observed_network(&head_metadata, upstream)?,
        "head fork-network mismatch"
    );
    let reference = format!("refs/heads/{branch}");
    let encoded: String = url::form_urlencoded::byte_serialize(reference.as_bytes()).collect();
    let activity = pages(
        client,
        &format!("repos/{head_repo}/activity?ref={encoded}&direction=desc"),
    )?;
    let head = pull["head"]["sha"].as_str().context("missing head")?;
    let (pushed, introduced) = push_time(&activity, &reference, head, now)?;
    let comments = pages(client, &format!("repos/{repo}/issues/{number}/comments"))?;
    // REST's second-resolution created/updated timestamps cannot prove no edit:
    // a collaborator could replace the operator's text in the same second.
    let item = crate::parse_github_item(
        &format!("{repo}#{number}"),
        None,
        crate::github::DEFAULT_HOST,
    )?;
    let metadata = crate::plan::comment_metadata(client, &item, true)?;
    let reviews = pages(client, &format!("repos/{repo}/pulls/{number}/reviews"))?;
    // Validate the complete observation before selecting a decision. Otherwise
    // a malformed latest review could look like no permission and disappear
    // from the pass without reporting the unreadable evidence.
    let mut previous = None;
    let mut ids = BTreeSet::new();
    for review in &reviews {
        let id = review["id"].as_u64().context("missing review id")?;
        ensure!(ids.insert(id), "duplicate review evidence");
        ensure!(review["user"]["login"].is_string(), "missing review author");
        ensure!(review["state"].is_string(), "missing review state");
        if review["user"]["login"]
            .as_str()
            .is_some_and(|a| a.eq_ignore_ascii_case(&config.operator))
        {
            ensure!(
                matches!(
                    review["state"].as_str(),
                    Some("APPROVED" | "COMMENTED" | "CHANGES_REQUESTED" | "DISMISSED")
                ),
                "unknown operator review state"
            );
        }
        let submitted = at(&review["submitted_at"])?;
        ensure!(
            submitted <= now && previous.is_none_or(|p| submitted >= p),
            "out-of-order reviews"
        );
        previous = Some(submitted);
    }
    let on_approval = config.promote.as_ref().is_some_and(|p| p.on_approval);
    if !on_approval && pull["draft"] != false {
        return Err(NotSelected("draft".into()).into());
    }
    let labels = pull["labels"].as_array().context("missing labels")?;
    for label in labels {
        ensure!(label["name"].is_string(), "missing label name");
    }
    let mut command = None;
    ensure!(
        metadata.len() == comments.len(),
        "incomplete comment edit evidence"
    );
    let mut stale = None;
    let mut previous = None;
    let mut ids = BTreeSet::new();
    for comment in &comments {
        let id = comment["id"]
            .as_u64()
            .filter(|n| *n > 0)
            .context("missing comment id")?;
        ensure!(ids.insert(id), "duplicate comment evidence");
        let created = at(&comment["created_at"])?;
        ensure!(
            created <= now && previous.is_none_or(|p| created >= p),
            "out-of-order comments"
        );
        previous = Some(created);
        let author = comment["user"]["login"]
            .as_str()
            .context("missing comment author")?;
        let observed = metadata
            .iter()
            .find(|m| crate::plan::comment_id(&m.url) == Some(id))
            .context("missing comment edit evidence")?;
        ensure!(
            observed.url == format!("https://github.com/{repo}/pull/{number}#issuecomment-{id}")
                && observed.created == created
                && observed.author.as_deref() == Some(author),
            "comment identity/edit evidence mismatch"
        );
        if !author.eq_ignore_ascii_case(&config.operator) {
            continue;
        }
        ensure!(
            at(&comment["updated_at"])? == created
                && !observed.edited
                && observed.editor.is_none()
                && observed.changed == created,
            "edited operator comment has unverifiable authorship"
        );
        let body = comment["body"].as_str().context("missing comment body")?;
        if let Some(human_text) = command_line(body)? {
            ensure!(
                created != pushed,
                "command and push have ambiguous ordering"
            );
            if created > pushed {
                ensure!(
                    command.as_ref().is_none_or(|(_, t, _)| *t != created),
                    "ambiguous command ordering"
                );
                command = Some((
                    format!("https://github.com/{repo}/pull/{number}#issuecomment-{id}"),
                    created,
                    human_text,
                ));
            } else {
                stale = Some(format!(
                    "stale command=https://github.com/{repo}/pull/{number}#issuecomment-{id} at={} push={}",
                    created.format(&Rfc3339)?,
                    pushed.format(&Rfc3339)?
                ));
            }
        }
    }
    // Only on a draft: approving a ready fork PR may mean merging it into the
    // fork alone, and must not post it upstream.
    if command.is_none() && on_approval && pull["draft"] == true {
        command = approval(client, &reviews, config, (repo, number), head, pushed)?;
    }
    let (url, created, human_text) = command.ok_or_else(|| {
        NotSelected(stale.unwrap_or_else(|| {
            if on_approval {
                "no operator command or approval after observed push".into()
            } else {
                "no operator command after observed push".into()
            }
        }))
    })?;
    let evidence = format!(
        "command={url} at={} human-text={human_text} push={}",
        created.format(&Rfc3339)?,
        pushed.format(&Rfc3339)?
    );
    let selected = (|| {
        for review in &reviews {
            let author = review["user"]["login"]
                .as_str()
                .context("missing review author")?;
            let submitted = at(&review["submitted_at"])?;
            if !author.eq_ignore_ascii_case(&config.operator) {
                continue;
            }
            let state = review["state"].as_str().context("missing review state")?;
            // REST dates the original submission, not the dismissal. Without a
            // proved dismissal time no command can be proved newer than it.
            if state == "DISMISSED" {
                return Err(NotSelected(
                    "operator dismissal has no proved cancellation time".into(),
                )
                .into());
            }
            if state == "CHANGES_REQUESTED" && submitted >= created {
                return Err(NotSelected("operator change request cancels command".into()).into());
            }
            ensure!(
                matches!(state, "APPROVED" | "COMMENTED" | "CHANGES_REQUESTED"),
                "unknown operator review state"
            );
        }
        Ok(Command {
            url,
            author: config.operator.clone(),
            created,
            head: head.into(),
            pushed,
            introduced,
            pusher: activity[0]["actor"]["login"].as_str().map(str::to_owned),
            human_text,
        })
    })()
    .with_context(|| evidence)?;
    hold!(
        !labels.iter().any(|l| l["name"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("needs-human"))),
        "needs-human"
    );
    Ok(selected)
}

/// With `promote.onApproval`, on a draft, the operator's latest deciding review,
/// when it approves this head after the push establishing it. The commit id
/// alone cannot bind it: a force-push back to an approved old oid needs a new
/// one. Nor can the current draft flag: an approval of the ready PR, meant for
/// the fork alone, must not count after a later conversion to draft, so it has
/// to follow the timeline's latest `convert_to_draft`. Review order, duplicates
/// and dismissals are checked with the command evidence afterwards, which fails
/// closed.
fn approval(
    client: &impl Rest,
    reviews: &[Value],
    config: &Config,
    (repo, number): (&str, u64),
    head: &str,
    pushed: OffsetDateTime,
) -> Result<Option<(String, OffsetDateTime, bool)>> {
    let Some(review) = reviews.iter().rev().find(|r| {
        r["user"]["login"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(&config.operator))
            && r["state"] != "COMMENTED"
    }) else {
        return Ok(None);
    };
    if review["state"] != "APPROVED" || review["commit_id"] != head {
        return Ok(None);
    }
    let id = review["id"]
        .as_u64()
        .filter(|n| *n > 0)
        .context("missing review id")?;
    let submitted = at(&review["submitted_at"])?;
    ensure!(
        submitted != pushed,
        "approval and push have ambiguous ordering"
    );
    if submitted < pushed {
        return Ok(None);
    }
    let timeline = pages(client, &format!("repos/{repo}/issues/{number}/timeline"))?;
    let mut drafted = None;
    for event in &timeline {
        if event["event"] == "convert_to_draft" {
            drafted = drafted.max(Some(at(&event["created_at"])?));
        }
    }
    ensure!(
        drafted != Some(submitted),
        "approval and draft conversion have ambiguous ordering"
    );
    Ok(drafted.is_none_or(|d| submitted > d).then(|| {
        (
            format!("https://github.com/{repo}/pull/{number}#pullrequestreview-{id}"),
            submitted,
            false,
        )
    }))
}

/// Complete authenticated chain, including force-pushes back to an old oid:
/// the head's latest push and the first time the branch was set to it.
fn push_time(
    events: &[Value],
    reference: &str,
    head: &str,
    now: OffsetDateTime,
) -> Result<(OffsetDateTime, OffsetDateTime)> {
    ensure!(!events.is_empty(), "missing branch activity");
    let mut expected = head;
    let mut previous = now;
    for event in events {
        let time = at(&event["timestamp"])?;
        let before = event["before"]
            .as_str()
            .filter(|s| oid(s))
            .context("invalid activity before")?;
        ensure!(
            event["ref"] == reference && event["after"] == expected && time < previous,
            "ambiguous or out-of-order branch activity"
        );
        ensure!(
            matches!(
                event["activity_type"].as_str(),
                Some(
                    "push"
                        | "force_push"
                        | "branch_creation"
                        | "branch_deletion"
                        | "pr_merge"
                        | "merge_queue_merge"
                )
            ),
            "unknown branch activity"
        );
        expected = before;
        previous = time;
    }
    ensure!(
        expected == "0".repeat(40)
            && events
                .last()
                .is_some_and(|e| e["activity_type"] == "branch_creation"),
        "incomplete branch activity"
    );
    let first = events
        .iter()
        .rev()
        .find(|e| e["after"] == head)
        .context("missing head activity")?;
    Ok((at(&events[0]["timestamp"])?, at(&first["timestamp"])?))
}

/// Only a complete plain command comment (with optional surrounding newlines).
/// Do not try to infer top-level intent from arbitrary Markdown contexts.
fn command_line(body: &str) -> Result<Option<bool>> {
    let count = body
        .lines()
        .filter(|line| matches!(*line, "/promote" | "/promote --human-text"))
        .count();
    ensure!(count <= 1, "ambiguous command lines");
    Ok(match body.trim_matches(['\r', '\n']) {
        "/promote" => Some(false),
        "/promote --human-text" => Some(true),
        _ => None,
    })
}

mod policy;

mod provenance;

pub(crate) mod awaiting;

#[cfg(test)]
pub(crate) mod tests;
