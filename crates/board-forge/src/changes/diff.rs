//! What changed in the followed and midstream repositories since each one's
//! watermark, read-only (`agent-board changes` without `--since`,
//! agent-board#895): the first stage of a change-driven pass
//! (agent-board#894), which looks at what moved rather than at every open
//! item again.
//!
//! The repositories are the configuration's `follow` list, explicit names
//! included whatever their visibility, plus the midstream forks of its
//! `midstream` owner, discovered from the organization's public listing
//! with no allowlist: a GitHub fork (its `parent` is what the listing's
//! `fork` flag says), neither archived nor private, with the configured
//! topic or the custom property `origin: fork` ([`discover`]).
//!
//! Each repository is read incrementally. One conditional REST read of its
//! issues and pull requests updated since the watermark, less [`SKEW`] for
//! clocks that disagree, is the change detector: GitHub answers an unchanged
//! list with 304, which it does not count against the rate limit, and an
//! empty list costs one request. The numbers it lists are then read in
//! detail, [`DETAIL_BATCH`] to a GraphQL request, with their timeline since
//! the same time: opened, closed, reopened, merged, edited, renamed and
//! labelled; comments, the operator's flagged; reviews; pushes, and the check
//! state of an open pull request's head; sub-issue and parent links; project
//! membership and Status changes. A pull request whose head checks were
//! still running is read again on later runs until they settle, so that
//! their result is a change too, although it bumps nothing GitHub lists. An
//! item whose update the timeline does not explain is still listed, as
//! `updated`. Nothing else is read: a Project field change alone, which
//! does not update the item, does not surface it.
//!
//! The watermarks and ETags are the caller's state ([`State`]), under its
//! cache directory, with the numbers each list held: a 304 means the list
//! is as it was, and an uncommitted read lists the same changes again,
//! reading their detail again. A watermark moves only when the caller
//! commits the read (`--commit`), or later when a pass consumes the diff;
//! the first read of a repository covers [`FIRST_WINDOW`], and every read
//! before its commit the same window, whose start the state keeps until
//! then. The output is a
//! summary of at most [`SUMMARY_LINES`] lines, a line per changed
//! repository, or the whole diff as JSON ([`Diff`]). Lists are sorted and
//! the detail requests are batched
//! in number order, so a recorded read replays deterministically.
use crate::changes::{format_time, parse_time};
use crate::github::{API_ROOT, Transport, text};
use crate::pass::midstream::Midstream;
use crate::poll::{Rest, RestResponse, next_link, validate_repository};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use time::OffsetDateTime;

pub const SCHEMA: &str = "agent-board/changes/v1";
pub const STATE_SCHEMA: &str = "agent-board/changes-state/v1";
/// The diff's top-level JSON fields, which `--json` selects among.
pub const FIELDS: &[&str] = &["schema", "now", "repositories", "requests", "revalidated"];
/// What the first read of a repository covers.
pub const FIRST_WINDOW: time::Duration = time::Duration::hours(24);
/// Subtracted from a watermark, for clocks that disagree: a change near
/// the watermark may be listed twice rather than never.
pub const SKEW: time::Duration = time::Duration::minutes(5);
/// The most pages of a repository's updated issues one read lists; more
/// refuses the read rather than report part of it.
pub const LIST_PAGES: usize = 10;
/// Items read in detail by one GraphQL request.
pub const DETAIL_BATCH: usize = 25;
/// The summary's longest form: a header, then a line per changed repository.
pub const SUMMARY_LINES: usize = 20;
/// The most pull requests whose checks are followed until they settle; one
/// past it is reported with its running checks and not followed.
pub const PENDING_LIMIT: usize = 100;
/// The organization's custom property that marks a fork as the bot's.
const ORIGIN_PROPERTY: &str = "origin";
const ORIGIN_FORK: &str = "fork";
const PAGE_SIZE: usize = 100;
/// Head check states that have not settled.
const RUNNING_CHECKS: &[&str] = &["PENDING", "EXPECTED"];

/// The detail of one item: its own fields and its timeline since the
/// watermark, by the kinds of event the diff reports. Each item is read
/// under an alias, so a batch is one request ([`detail_query`]).
const DETAIL_FRAGMENTS: &str = "\
fragment F on IssueOrPullRequest{__typename \
... on Issue{number title url state createdAt updatedAt closedAt lastEditedAt author{login} \
timelineItems(first:100,since:$since,itemTypes:[CLOSED_EVENT,REOPENED_EVENT,LABELED_EVENT,UNLABELED_EVENT,ISSUE_COMMENT,RENAMED_TITLE_EVENT,SUB_ISSUE_ADDED_EVENT,SUB_ISSUE_REMOVED_EVENT,PARENT_ISSUE_ADDED_EVENT,PARENT_ISSUE_REMOVED_EVENT,ADDED_TO_PROJECT_V2_EVENT,REMOVED_FROM_PROJECT_V2_EVENT,PROJECT_V2_ITEM_STATUS_CHANGED_EVENT]){nodes{...TI} pageInfo{hasNextPage}}} \
... on PullRequest{number title url state createdAt updatedAt closedAt lastEditedAt merged mergedAt isDraft author{login} \
commits(last:1){nodes{commit{abbreviatedOid statusCheckRollup{state}}}} \
timelineItems(first:100,since:$since,itemTypes:[CLOSED_EVENT,REOPENED_EVENT,LABELED_EVENT,UNLABELED_EVENT,ISSUE_COMMENT,RENAMED_TITLE_EVENT,MERGED_EVENT,PULL_REQUEST_REVIEW,PULL_REQUEST_COMMIT,HEAD_REF_FORCE_PUSHED_EVENT,READY_FOR_REVIEW_EVENT,CONVERT_TO_DRAFT_EVENT,ADDED_TO_PROJECT_V2_EVENT,REMOVED_FROM_PROJECT_V2_EVENT,PROJECT_V2_ITEM_STATUS_CHANGED_EVENT]){nodes{...TP} pageInfo{hasNextPage}}}} \
fragment TI on IssueTimelineItems{__typename \
... on ClosedEvent{createdAt actor{login}} ... on ReopenedEvent{createdAt actor{login}} \
... on LabeledEvent{createdAt actor{login} label{name}} ... on UnlabeledEvent{createdAt actor{login} label{name}} \
... on IssueComment{createdAt author{login} url} ... on RenamedTitleEvent{createdAt actor{login}} \
... on SubIssueAddedEvent{createdAt subIssue{url}} ... on SubIssueRemovedEvent{createdAt subIssue{url}} \
... on ParentIssueAddedEvent{createdAt parent{url}} ... on ParentIssueRemovedEvent{createdAt parent{url}} \
... on AddedToProjectV2Event{createdAt} ... on RemovedFromProjectV2Event{createdAt} \
... on ProjectV2ItemStatusChangedEvent{createdAt previousStatus status}} \
fragment TP on PullRequestTimelineItems{__typename \
... on ClosedEvent{createdAt actor{login}} ... on ReopenedEvent{createdAt actor{login}} \
... on LabeledEvent{createdAt actor{login} label{name}} ... on UnlabeledEvent{createdAt actor{login} label{name}} \
... on IssueComment{createdAt author{login} url} ... on RenamedTitleEvent{createdAt actor{login}} \
... on AddedToProjectV2Event{createdAt} ... on RemovedFromProjectV2Event{createdAt} \
... on ProjectV2ItemStatusChangedEvent{createdAt previousStatus status} \
... on MergedEvent{createdAt actor{login}} ... on PullRequestReview{submittedAt author{login} state url} \
... on PullRequestCommit{commit{abbreviatedOid committedDate}} \
... on HeadRefForcePushedEvent{createdAt actor{login} afterCommit{abbreviatedOid}} \
... on ReadyForReviewEvent{createdAt} ... on ConvertToDraftEvent{createdAt}}";

/// A repository the diff reads, and whether it is a discovered midstream
/// fork rather than a followed repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Followed {
    pub name: String,
    pub midstream: bool,
}

/// What one read covers: the repositories, and whose comments are flagged.
#[derive(Debug)]
pub struct Scope {
    pub repositories: Vec<Followed>,
    pub operator: String,
}

/// The `follow` list, then the midstream forks of `midstream`'s owner
/// ([module docs](self)), each repository once, in that order.
pub fn discover(
    client: &impl Rest,
    follow: &[String],
    midstream: Option<&Midstream>,
) -> Result<Vec<Followed>> {
    let mut seen = BTreeSet::new();
    let mut found = vec![];
    for name in follow {
        validate_repository(name).context("follow")?;
        if seen.insert(name.to_ascii_lowercase()) {
            found.push(Followed {
                name: name.clone(),
                midstream: false,
            });
        }
    }
    let Some(midstream) = midstream else {
        return Ok(found);
    };
    // A fork is one GitHub says has a parent; the public listing decides
    // the visibility, and an archived fork has nothing to follow.
    let forks = midstream.list(client, true, |repository, topic| {
        repository["fork"] == true
            && repository["private"] != true
            && repository["archived"] != true
            && (topic || repository["custom_properties"][ORIGIN_PROPERTY] == ORIGIN_FORK)
    })?;
    for name in forks {
        if seen.insert(name.to_ascii_lowercase()) {
            found.push(Followed {
                name,
                midstream: true,
            });
        }
    }
    Ok(found)
}

/// A repository's watermark, and the list read last with its ETag and
/// numbers, for a conditional read of the same list: a 304 says the numbers
/// are still these.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Watermark {
    /// The committed watermark; none until the repository's first commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Where the first read started, kept until the first commit so that
    /// every read before it covers the same window: one computed from the
    /// clock again would start later and drop changes the earlier read
    /// listed, which a commit would then acknowledge unseen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub numbers: Vec<u64>,
}

/// The caller's state between reads, keyed by lower-case repository.
#[derive(Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    #[serde(default)]
    pub repositories: BTreeMap<String, Watermark>,
    /// Open pull requests (`owner/repo#N`, lower case) whose head checks
    /// were running, or settled but have not yet been committed.
    #[serde(default)]
    pub pending: BTreeSet<String>,
}

impl State {
    pub fn from_json(value: &Value) -> Result<Self> {
        ensure!(
            value["schema"] == STATE_SCHEMA,
            "not a changes state (expected {STATE_SCHEMA})"
        );
        let mut value = value.clone();
        value.as_object_mut().map(|o| o.remove("schema"));
        serde_json::from_value(value).context("changes state")
    }

    pub fn to_json(&self) -> Result<Value> {
        let mut value = serde_json::to_value(self)?;
        value["schema"] = json!(STATE_SCHEMA);
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    Opened,
    Closed,
    Reopened,
    Merged,
    Edited,
    Labelled,
    Commented,
    Reviewed,
    Pushed,
    /// The head's checks settled after a read saw them running.
    Checked,
    Linked,
    Project,
    Draft,
    /// Updated for a reason the timeline does not show.
    Updated,
}

impl Change {
    /// The change's name in JSON and in the summary.
    pub fn name(self) -> &'static str {
        match self {
            Self::Opened => "opened",
            Self::Closed => "closed",
            Self::Reopened => "reopened",
            Self::Merged => "merged",
            Self::Edited => "edited",
            Self::Labelled => "labelled",
            Self::Commented => "commented",
            Self::Reviewed => "reviewed",
            Self::Pushed => "pushed",
            Self::Checked => "checked",
            Self::Linked => "linked",
            Self::Project => "project",
            Self::Draft => "draft",
            Self::Updated => "updated",
        }
    }
}

impl Serialize for Change {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    Issue,
    PullRequest,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LabelChange {
    pub name: String,
    pub added: bool,
    pub at: String,
    pub by: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Comment {
    pub by: String,
    pub at: String,
    pub url: String,
    /// The configured operator wrote it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub operator: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Review {
    pub by: String,
    pub at: String,
    pub state: String,
    pub url: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub operator: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Push {
    pub commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub forced: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LinkChange {
    /// `sub-issue` or `parent`.
    pub kind: &'static str,
    pub added: bool,
    pub url: String,
    pub at: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ProjectChange {
    /// `added`, `removed` or `status`.
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    pub at: String,
}

/// One changed issue or pull request, with every change since the
/// repository's watermark.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Item {
    #[serde(rename = "ref")]
    pub reference: String,
    pub url: String,
    pub kind: Kind,
    pub title: String,
    pub state: String,
    pub author: String,
    pub updated_at: String,
    pub changes: BTreeSet<Change>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opened: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reopened: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged: Option<String>,
    /// The body's or the title's last edit in the window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edited: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<LabelChange>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<Comment>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reviews: Vec<Review>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pushes: Vec<Push>,
    /// An open pull request's head check state, as GitHub rolls it up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<LinkChange>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub project: Vec<ProjectChange>,
    /// Converted to a draft (`true`) or marked ready for review.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RepositoryDiff {
    pub repository: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub midstream: bool,
    /// Where the read started: the watermark less [`SKEW`].
    pub since: String,
    pub items: Vec<Item>,
}

#[derive(Debug, Serialize)]
pub struct Diff {
    pub schema: &'static str,
    /// When the read started; the watermark a commit moves to.
    pub now: String,
    /// Sorted by name; every repository read, changed or not.
    pub repositories: Vec<RepositoryDiff>,
    pub requests: usize,
    /// Conditional reads GitHub answered 304, which it does not count.
    pub revalidated: usize,
}

/// Counts the requests a read spends, and refuses every write.
struct Counted<'a, C> {
    inner: &'a C,
    requests: Cell<usize>,
    revalidated: Cell<usize>,
}

impl<C: Rest> Rest for Counted<'_, C> {
    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
        self.requests.set(self.requests.get() + 1);
        let response = self.inner.get(url, validator)?;
        if response.status == 304 {
            self.revalidated.set(self.revalidated.get() + 1);
        }
        Ok(response)
    }

    fn send(&self, method: &str, url: &str, _: &Value) -> Result<(u16, Value)> {
        bail!("changes is read-only, and does not {method} {url}")
    }
}

impl<C: Transport> Transport for Counted<'_, C> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.requests.set(self.requests.get() + 1);
        self.inner.graphql(query, variables)
    }
}

/// Every repository of `scope` since its watermark in `state`, at `now`.
/// Updates the state's ETags, pending checks and first-window starts, but
/// moves no watermark: [`Diff::commit`] does.
pub fn read<C: Rest + Transport>(
    client: &C,
    scope: &Scope,
    state: &mut State,
    now: OffsetDateTime,
) -> Result<Diff> {
    let client = Counted {
        inner: client,
        requests: Cell::new(0),
        revalidated: Cell::new(0),
    };
    let mut repositories = vec![];
    let in_scope: BTreeSet<_> = scope
        .repositories
        .iter()
        .map(|r| r.name.to_ascii_lowercase())
        .collect();
    // State is shared across caller checkouts. Only this scope's entries
    // consume its cap, but another checkout still needs its discovery paths.
    let mut pending_count = state
        .pending
        .iter()
        .filter(|reference| {
            reference
                .rsplit_once('#')
                .is_some_and(|(repository, _)| in_scope.contains(repository))
        })
        .count();
    for followed in &scope.repositories {
        let name = &followed.name;
        let key = name.to_ascii_lowercase();
        let mark = state.repositories.entry(key.clone()).or_default();
        let since = match (&mark.since, &mark.start) {
            (Some(since), _) => {
                let since =
                    parse_time(since).with_context(|| format!("watermark of {name}"))? - SKEW;
                format_time(since)?
            }
            (None, Some(start)) => {
                parse_time(start).with_context(|| format!("first window of {name}"))?;
                start.clone()
            }
            (None, None) => {
                let start = format_time(now - FIRST_WINDOW)?;
                mark.start = Some(start.clone());
                start
            }
        };
        let url = format!(
            "{API_ROOT}repos/{name}/issues?state=all&sort=updated&direction=asc&since={since}&per_page={PAGE_SIZE}"
        );
        let validator = mark
            .etag
            .as_deref()
            .filter(|_| mark.url.as_deref() == Some(url.as_str()))
            .map(|etag| ("If-None-Match", etag));
        let listed = list(&client, &url, validator)
            .with_context(|| format!("list {name} updated since {since}"))?;
        // A 304 says the list is as it was: the same numbers, read in
        // detail again, as the watermark has not moved past them.
        let changed: BTreeSet<u64> = match listed {
            Listed::Unchanged => mark.numbers.iter().copied().collect(),
            Listed::Changed { numbers, etag } => {
                mark.url = Some(url);
                mark.etag = etag;
                mark.numbers = numbers.iter().copied().collect();
                numbers
            }
        };
        let prefix = format!("{key}#");
        let pending: BTreeSet<u64> = state
            .pending
            .iter()
            .filter_map(|r| r.strip_prefix(&prefix)?.parse().ok())
            .collect();
        let numbers: BTreeSet<u64> = changed.union(&pending).copied().collect();
        let mut items = vec![];
        for (number, mut item) in details(&client, name, &numbers, &since, &scope.operator)? {
            let running = item.kind == Kind::PullRequest
                && item.state == "open"
                && item
                    .checks
                    .as_deref()
                    .is_some_and(|c| RUNNING_CHECKS.contains(&c));
            let reference = format!("{prefix}{number}");
            if running {
                // Followed until they settle, the first PENDING_LIMIT of
                // them; the rest were reported with their running checks.
                if pending_count < PENDING_LIMIT && state.pending.insert(reference) {
                    pending_count += 1;
                }
            } else if pending.contains(&number) && item.checks.is_some() {
                // Keep the discovery path until commit: this PR may
                // have aged out of the REST window while checks ran.
                item.changes.insert(Change::Checked);
            } else if state.pending.remove(&reference) {
                pending_count -= 1;
            }
            if changed.contains(&number) && item.changes.is_empty() {
                item.changes.insert(Change::Updated);
            }
            if !item.changes.is_empty() {
                items.push(item);
            }
        }
        repositories.push(RepositoryDiff {
            repository: name.clone(),
            midstream: followed.midstream,
            since,
            items,
        });
    }
    repositories.sort_by_key(|r| r.repository.to_ascii_lowercase());
    Ok(Diff {
        schema: SCHEMA,
        now: format_time(now)?,
        repositories,
        requests: client.requests.get(),
        revalidated: client.revalidated.get(),
    })
}

enum Listed {
    Unchanged,
    Changed {
        numbers: BTreeSet<u64>,
        etag: Option<String>,
    },
}

/// The numbers of the issues and pull requests `url` lists, every page,
/// and its ETag when it is one page short of [`PAGE_SIZE`]: a later page's
/// change leaves the first page's ETag as it was, and a full page can gain
/// a successor the same way, so a longer list is read whole each time.
/// Unchanged when the validator matched.
fn list(client: &impl Rest, url: &str, validator: Option<(&str, &str)>) -> Result<Listed> {
    let mut numbers = BTreeSet::new();
    let mut etag = None;
    let mut next = Some(url.to_owned());
    for page in 0..LIST_PAGES {
        let Some(url) = next.take() else { break };
        let response = client.get(&url, validator.filter(|_| page == 0))?;
        if response.status == 304 {
            ensure!(page == 0, "a later page was not read unconditionally");
            return Ok(Listed::Unchanged);
        }
        ensure!(
            response.status == 200,
            "GET {url}: GitHub HTTP status {}",
            response.status
        );
        let listed = response
            .body
            .as_array()
            .context("the issue list is not a list")?;
        for issue in listed {
            numbers.insert(issue["number"].as_u64().context("an issue has no number")?);
        }
        next = response.link.as_deref().and_then(next_link);
        if let Some(url) = next.as_ref().filter(|u| !u.starts_with(API_ROOT)) {
            bail!("the next page {url} is outside {API_ROOT}");
        }
        etag = response
            .etag
            .filter(|_| page == 0 && next.is_none() && listed.len() < PAGE_SIZE);
    }
    ensure!(
        next.is_none(),
        "more than {LIST_PAGES} pages of updated issues"
    );
    Ok(Listed::Changed { numbers, etag })
}

/// One request's query: each of `numbers` under its own alias.
fn detail_query(numbers: &[u64]) -> String {
    let mut query = String::from(
        "query($owner:String!,$name:String!,$since:DateTime!){repository(owner:$owner,name:$name){",
    );
    for (k, number) in numbers.iter().enumerate() {
        let _ = write!(query, "i{k}:issueOrPullRequest(number:{number}){{...F}} ");
    }
    query.push_str("}} ");
    query.push_str(DETAIL_FRAGMENTS);
    query
}

/// `numbers` of `repository` in detail, classified, in number order.
fn details(
    client: &impl Transport,
    repository: &str,
    numbers: &BTreeSet<u64>,
    since: &str,
    operator: &str,
) -> Result<Vec<(u64, Item)>> {
    let (owner, name) = repository
        .split_once('/')
        .with_context(|| format!("{repository} is not owner/repo"))?;
    let numbers: Vec<u64> = numbers.iter().copied().collect();
    let mut items = vec![];
    for batch in numbers.chunks(DETAIL_BATCH) {
        let data = client.graphql(
            &detail_query(batch),
            json!({"owner": owner, "name": name, "since": since}),
        )?;
        for (k, number) in batch.iter().enumerate() {
            let node = &data["repository"][format!("i{k}")];
            ensure!(
                node["number"].as_u64() == Some(*number),
                "{repository}#{number} was listed but not read"
            );
            items.push((
                *number,
                classify(node, repository, since, operator)
                    .with_context(|| format!("{repository}#{number}"))?,
            ));
        }
    }
    Ok(items)
}

/// Whether `value`'s login names the operator.
fn by_operator(login: &str, operator: &str) -> bool {
    login.eq_ignore_ascii_case(operator)
}

fn login(value: &Value) -> String {
    value["login"].as_str().unwrap_or_default().to_owned()
}

/// An item as [`DETAIL_FRAGMENTS`] reads it, with every change at or after
/// `since` ([module docs](self)); `changes` stays empty when nothing in the
/// window explains its update.
pub fn classify(node: &Value, repository: &str, since: &str, operator: &str) -> Result<Item> {
    let cutoff = parse_time(since)?;
    let after = |value: &Value, field: &str| -> Result<Option<String>> {
        match &value[field] {
            Value::Null => Ok(None),
            Value::String(at) => Ok((parse_time(at)? >= cutoff).then(|| at.clone())),
            _ => bail!("invalid GitHub {field}"),
        }
    };
    let kind = match node["__typename"].as_str() {
        Some("Issue") => Kind::Issue,
        Some("PullRequest") => Kind::PullRequest,
        _ => bail!("not an issue or pull request"),
    };
    let number = node["number"].as_u64().context("missing GitHub number")?;
    let mut item = Item {
        reference: format!("{repository}#{number}"),
        url: text(node, "url")?.to_owned(),
        kind,
        title: text(node, "title")?.to_owned(),
        state: text(node, "state")?.to_ascii_lowercase(),
        author: login(&node["author"]),
        updated_at: text(node, "updatedAt")?.to_owned(),
        ..Item::default()
    };
    if let Some(at) = after(node, "createdAt")? {
        item.changes.insert(Change::Opened);
        item.opened = Some(at);
    }
    if let Some(at) = after(node, "lastEditedAt")? {
        item.changes.insert(Change::Edited);
        item.edited = Some(at);
    }
    if kind == Kind::PullRequest && item.state == "open" {
        item.checks = node["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["state"]
            .as_str()
            .map(str::to_owned);
    }
    let timeline = &node["timelineItems"];
    ensure!(
        timeline["pageInfo"]["hasNextPage"] == false,
        "more than one page of timeline events since {since}"
    );
    let events = timeline["nodes"]
        .as_array()
        .context("missing GitHub timelineItems")?;
    for event in events {
        let at = |field: &str| text(event, field).map(str::to_owned);
        match text(event, "__typename")? {
            "ClosedEvent" => {
                item.changes.insert(Change::Closed);
                item.closed = Some(at("createdAt")?);
            }
            "ReopenedEvent" => {
                item.changes.insert(Change::Reopened);
                item.reopened = Some(at("createdAt")?);
            }
            "MergedEvent" => {
                item.changes.insert(Change::Merged);
                item.merged = Some(at("createdAt")?);
            }
            "RenamedTitleEvent" => {
                item.changes.insert(Change::Edited);
                item.edited = Some(at("createdAt")?);
            }
            kind @ ("LabeledEvent" | "UnlabeledEvent") => {
                item.changes.insert(Change::Labelled);
                item.labels.push(LabelChange {
                    name: text(&event["label"], "name")?.to_owned(),
                    added: kind == "LabeledEvent",
                    at: at("createdAt")?,
                    by: login(&event["actor"]),
                });
            }
            "IssueComment" => {
                let by = login(&event["author"]);
                item.changes.insert(Change::Commented);
                item.comments.push(Comment {
                    operator: by_operator(&by, operator),
                    by,
                    at: at("createdAt")?,
                    url: at("url")?,
                });
            }
            "PullRequestReview" => {
                let by = login(&event["author"]);
                item.changes.insert(Change::Reviewed);
                item.reviews.push(Review {
                    operator: by_operator(&by, operator),
                    by,
                    at: at("submittedAt")?,
                    state: at("state")?,
                    url: at("url")?,
                });
            }
            "PullRequestCommit" => {
                item.changes.insert(Change::Pushed);
                item.pushes.push(Push {
                    commit: text(&event["commit"], "abbreviatedOid")?.to_owned(),
                    at: event["commit"]["committedDate"].as_str().map(str::to_owned),
                    forced: false,
                });
            }
            "HeadRefForcePushedEvent" => {
                item.changes.insert(Change::Pushed);
                item.pushes.push(Push {
                    commit: event["afterCommit"]["abbreviatedOid"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    at: Some(at("createdAt")?),
                    forced: true,
                });
            }
            kind @ ("SubIssueAddedEvent"
            | "SubIssueRemovedEvent"
            | "ParentIssueAddedEvent"
            | "ParentIssueRemovedEvent") => {
                let (field, name) = if kind.starts_with("Sub") {
                    ("subIssue", "sub-issue")
                } else {
                    ("parent", "parent")
                };
                item.changes.insert(Change::Linked);
                item.links.push(LinkChange {
                    kind: name,
                    added: kind.ends_with("AddedEvent"),
                    url: text(&event[field], "url")?.to_owned(),
                    at: at("createdAt")?,
                });
            }
            "AddedToProjectV2Event" | "RemovedFromProjectV2Event" => {
                let added = text(event, "__typename")? == "AddedToProjectV2Event";
                item.changes.insert(Change::Project);
                item.project.push(ProjectChange {
                    kind: if added { "added" } else { "removed" },
                    from: None,
                    to: None,
                    at: at("createdAt")?,
                });
            }
            "ProjectV2ItemStatusChangedEvent" => {
                item.changes.insert(Change::Project);
                item.project.push(ProjectChange {
                    kind: "status",
                    from: event["previousStatus"].as_str().map(str::to_owned),
                    to: event["status"].as_str().map(str::to_owned),
                    at: at("createdAt")?,
                });
            }
            "ReadyForReviewEvent" | "ConvertToDraftEvent" => {
                item.changes.insert(Change::Draft);
                item.draft = Some(text(event, "__typename")? == "ConvertToDraftEvent");
            }
            other => bail!("unexpected timeline event {other}"),
        }
    }
    // GitHub can return the state transition's timestamp without its event.
    if item.closed.is_none()
        && let Some(at) = after(node, "closedAt")?
    {
        item.changes.insert(Change::Closed);
        item.closed = Some(at);
    }
    // A merge GitHub recorded without its event, or before the timeline.
    if kind == Kind::PullRequest
        && node["merged"] == true
        && item.merged.is_none()
        && let Some(at) = after(node, "mergedAt")?
    {
        item.changes.insert(Change::Merged);
        item.merged = Some(at);
    }
    Ok(item)
}

impl Diff {
    /// Move every read repository's watermark to this read's time.
    pub fn commit(&self, state: &mut State) {
        for repository in &self.repositories {
            let key = repository.repository.to_ascii_lowercase();
            let mark = state.repositories.entry(key).or_default();
            mark.since = Some(self.now.clone());
            mark.start = None;
            for item in &repository.items {
                if item.changes.contains(&Change::Checked) {
                    state.pending.remove(&item.reference.to_ascii_lowercase());
                }
            }
        }
    }

    /// At most [`SUMMARY_LINES`] lines: the totals, then a line per changed
    /// repository with its items counted by change, the rest counted on a
    /// last line.
    pub fn summary(&self) -> String {
        let changed: Vec<_> = self
            .repositories
            .iter()
            .filter(|r| !r.items.is_empty())
            .collect();
        let items: usize = changed.iter().map(|r| r.items.len()).sum();
        let mut out = format!(
            "changes: {items} items in {} of {} repositories; {} requests, {} revalidated\n",
            changed.len(),
            self.repositories.len(),
            self.requests,
            self.revalidated
        );
        let shown = if changed.len() < SUMMARY_LINES {
            changed.len()
        } else {
            SUMMARY_LINES - 2
        };
        for repository in &changed[..shown] {
            // Items by change, and the operator's comments and reviews.
            let mut counts: BTreeMap<Change, usize> = BTreeMap::new();
            let mut operator = 0;
            for item in &repository.items {
                for change in &item.changes {
                    *counts.entry(*change).or_default() += 1;
                }
                operator += item.comments.iter().filter(|c| c.operator).count()
                    + item.reviews.iter().filter(|r| r.operator).count();
            }
            let mut counts: Vec<_> = counts
                .iter()
                .map(|(change, n)| format!("{} {n}", change.name()))
                .collect();
            if operator > 0 {
                counts.push(format!("operator {operator}"));
            }
            let _ = writeln!(
                out,
                "{}: {} items; {}",
                repository.repository,
                repository.items.len(),
                counts.join(", ")
            );
        }
        if shown < changed.len() {
            let rest = &changed[shown..];
            let _ = writeln!(
                out,
                "+{} more repositories with {} items",
                rest.len(),
                rest.iter().map(|r| r.items.len()).sum::<usize>()
            );
        }
        out
    }
}

#[cfg(test)]
mod tests;
