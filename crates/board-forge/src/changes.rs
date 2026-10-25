//! What changed on a project since a time, as release notes: every line a
//! link to an issue or pull request, read from the forge rather than written
//! by an agent, so a status update's facts are neither stale nor unlinked
//! (agent-board#277).
//!
//! The board covers the repositories its items are in. Pull requests seldom
//! are items, so those merged or opened in the window in a covered
//! repository are searched for too. Outside the board, a merge is news
//! whoever wrote it, a new pull request only when the bot (the token's
//! viewer) or the operator wrote it, and an app's or dependency bot's
//! pull requests are counted on one last line of their section rather
//! than listed. A project item is always listed, whoever wrote it.
//!
//! Each item appears at most once, in the first section it fits:
//! **Landed**, a pull request merged in the window, with the issues it
//! closes; **Closed**, an issue or pull request closed in the window without
//! a merge, unless a landed pull request closes it; **Opened**, created in
//! the window; **Held**, a hold label ([`HOLD_LABELS`]) added or removed in
//! the window, `(released)` when none is left. Lines sort by repository and
//! number. Removed members and project-field edits are not read, and
//! draft issues have nothing to link. A label, hold event or closing
//! reference list longer than one page refuses the read rather than guess.
//!
//! A status update puts a short summary on top, with every `owner/repo#N` in
//! it linked, and attaches each note to the line of the item it names. A short
//! `repo#N` links too when exactly one covered repository has that name. A
//! note for an item not listed goes under **Also**, still as a link. Landed
//! groups its lines by repository once it is long. When the body
//! would exceed its limit, the sections with the most lines (the later on a
//! tie, so Landed goes last) drop their un-noted lines first and end with
//! `+K more`; [`Changes::markdown_within`] trims the plain list the same way.
//!
//! [`read`] can leave out private repositories (`changes --public-only`), for
//! a list that goes somewhere public: every query reads each repository's
//! `isPrivate`, so this costs no request, and a private repository is then
//! neither listed, searched, nor named in a landing's `closes`.
use crate::github::{ReadBudget, SNAPSHOT_REQUEST_LIMIT, Transport, connection, text};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The window a project's first status update covers.
pub const FIRST_WINDOW: time::Duration = time::Duration::days(7);
/// Labels whose change puts an item under **Held**.
pub const HOLD_LABELS: &[&str] = &["needs-human", "needs-design"];

const ITEMS_QUERY: &str = "query($id:ID!,$cursor:String,$since:DateTime!){node(id:$id){... on ProjectV2{items(first:100,after:$cursor){nodes{content{__typename \
... on Issue{number title url state createdAt closedAt repository{nameWithOwner isPrivate} labels(first:100){nodes{name} pageInfo{hasNextPage}} timelineItems(first:100,since:$since,itemTypes:[LABELED_EVENT,UNLABELED_EVENT]){nodes{__typename ... on LabeledEvent{createdAt label{name}} ... on UnlabeledEvent{createdAt label{name}}} pageInfo{hasNextPage}}} \
... on PullRequest{number title url state createdAt closedAt merged mergedAt repository{nameWithOwner isPrivate} labels(first:100){nodes{name} pageInfo{hasNextPage}} timelineItems(first:100,since:$since,itemTypes:[LABELED_EVENT,UNLABELED_EVENT]){nodes{__typename ... on LabeledEvent{createdAt label{name}} ... on UnlabeledEvent{createdAt label{name}}} pageInfo{hasNextPage}} closingIssuesReferences(first:25){nodes{number url repository{nameWithOwner isPrivate}} pageInfo{hasNextPage}}}\
}} pageInfo{hasNextPage endCursor}}}}}";

/// A covered repository's pull requests by search qualifier; only what a
/// Landed or Opened line needs.
const SEARCH_QUERY: &str = "query($q:String!,$cursor:String){search(query:$q,type:ISSUE,first:100,after:$cursor){nodes{__typename \
... on PullRequest{number title url state createdAt closedAt merged mergedAt author{__typename login} repository{nameWithOwner isPrivate} closingIssuesReferences(first:25){nodes{number url repository{nameWithOwner isPrivate}} pageInfo{hasNextPage}}}\
} pageInfo{hasNextPage endCursor}}}";
/// The qualifiers each covered repository is searched with.
const SEARCHES: &[&str] = &["merged", "created"];
/// Logins of dependency bots that post as ordinary users.
const DEPENDENCY_BOTS: &[&str] = &["renovate", "dependabot"];
const VIEWER_QUERY: &str = "query{viewer{login}}";
/// Landed lines past which they are grouped by repository.
const GROUP_LANDED: usize = 8;

const UPDATES_QUERY: &str = "query($id:ID!,$cursor:String){node(id:$id){... on ProjectV2{statusUpdates(first:100,after:$cursor){nodes{createdAt} pageInfo{hasNextPage endCursor}}}}}";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Section {
    Landed,
    Closed,
    Opened,
    Held,
}

impl Section {
    fn heading(self) -> &'static str {
        match self {
            Self::Landed => "Landed",
            Self::Closed => "Closed",
            Self::Opened => "Opened",
            Self::Held => "Held",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Link {
    #[serde(rename = "ref")]
    pub reference: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Line {
    #[serde(flatten)]
    pub link: Link,
    pub title: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub closes: Vec<Link>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub released: bool,
}

/// Searched pull requests by apps and dependency bots, counted, not listed.
#[derive(Debug, Default, Serialize)]
pub struct Bots {
    pub count: usize,
    pub repositories: BTreeSet<String>,
}

#[derive(Debug, Serialize)]
pub struct Changes {
    pub since: String,
    /// The window's end, if it has one; otherwise it runs to the read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    #[serde(flatten)]
    pub sections: BTreeMap<Section, Vec<Line>>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub bots: BTreeMap<Section, Bots>,
    /// The repositories the board covers, which short refs resolve against.
    #[serde(skip)]
    pub repositories: BTreeSet<String>,
}

pub fn parse_time(value: &str) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339)
        .with_context(|| format!("invalid time {value:?}: expected RFC 3339"))
}

/// When the project's latest status update was posted, if it has one.
pub fn last_update(t: &impl Transport, project: &str) -> Result<Option<OffsetDateTime>> {
    let budget = ReadBudget::new(t, SNAPSHOT_REQUEST_LIMIT);
    let (updates, complete) = connection(
        &budget,
        UPDATES_QUERY,
        json!({"id": project}),
        "/node/statusUpdates",
    )?;
    ensure!(complete, "incomplete status update history");
    updates
        .iter()
        .map(|u| text(u, "createdAt").and_then(parse_time))
        .collect::<Result<Vec<_>>>()
        .map(|times| times.into_iter().max())
}

/// The project's changes since `since`: its items, then a search of each
/// covered repository, all within one request budget. New pull requests
/// outside the board are listed when the token's viewer or `operator` wrote
/// them. With `public_only`, nothing of a private repository is. With
/// `until`, the window ends there rather than at the read.
pub fn read(
    t: &impl Transport,
    project: &str,
    since: OffsetDateTime,
    until: Option<OffsetDateTime>,
    operator: Option<&str>,
    public_only: bool,
) -> Result<Changes> {
    ensure!(
        until.is_none_or(|until| until > since),
        "the window must end after it starts"
    );
    let since = format_time(since)?;
    let until = until.map(format_time).transpose()?;
    // GitHub's search takes a closed range of times, or an open end.
    let range = match &until {
        Some(until) => format!("{since}..{until}"),
        None => format!(">={since}"),
    };
    let budget = ReadBudget::new(t, SNAPSHOT_REQUEST_LIMIT);
    let viewer = budget.graphql(VIEWER_QUERY, json!({}))?;
    let mut authors = BTreeSet::from([text(&viewer["viewer"], "login")?.to_ascii_lowercase()]);
    authors.extend(operator.map(str::to_ascii_lowercase));
    let (items, complete) = connection(
        &budget,
        ITEMS_QUERY,
        json!({"id": project, "since": since}),
        "/node/items",
    )?;
    ensure!(complete, "incomplete project items");
    // Before `covered`, so that a private repository is not searched either.
    let items = if public_only {
        without_private(items, "/content")?
    } else {
        items
    };
    let mut searched = vec![];
    for repo in covered(&items) {
        // Names come from GitHub, but they go into query text.
        crate::poll::validate_repository(&repo)?;
        for qualifier in SEARCHES {
            let (found, complete) = connection(
                &budget,
                SEARCH_QUERY,
                json!({"q": format!("repo:{repo} is:pr {qualifier}:{range}")}),
                "/search",
            )?;
            ensure!(complete, "incomplete pull request search in {repo}");
            searched.extend(found);
        }
    }
    let searched = if public_only {
        without_private(searched, "")?
    } else {
        searched
    };
    classify_window(&items, &searched, &since, until.as_deref(), &authors)
}

/// Whether a repository, as the queries read it, is public. A visibility
/// that was not read counts as private: this decides what is published.
fn public(repository: &Value) -> Result<bool> {
    repository["isPrivate"]
        .as_bool()
        .map(|p| !p)
        .with_context(|| {
            format!(
                "unread visibility of {}",
                repository["nameWithOwner"]
                    .as_str()
                    .unwrap_or("a repository")
            )
        })
}

/// `values` less the issues and pull requests (at `pointer` in each) of
/// private repositories, and less the private issues a pull request closes.
/// Anything else is left for [`classify`] to judge.
fn without_private(values: Vec<Value>, pointer: &str) -> Result<Vec<Value>> {
    let mut kept = Vec::with_capacity(values.len());
    for mut value in values {
        let Some(content) = value.pointer_mut(pointer) else {
            kept.push(value);
            continue;
        };
        if !matches!(
            content["__typename"].as_str(),
            Some("Issue" | "PullRequest")
        ) {
            kept.push(value);
            continue;
        }
        if !public(&content["repository"])? {
            continue;
        }
        if let Some(nodes) = content
            .pointer_mut("/closingIssuesReferences/nodes")
            .and_then(Value::as_array_mut)
        {
            let shown = nodes
                .iter()
                .map(|n| public(&n["repository"]))
                .collect::<Result<Vec<_>>>()?;
            let mut shown = shown.into_iter();
            nodes.retain(|_| shown.next().unwrap_or(false));
        }
        kept.push(value);
    }
    Ok(kept)
}

/// Whether a pull request's author is an app or a dependency bot.
fn by_bot(content: &Value) -> bool {
    let author = &content["author"];
    let login = author["login"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    author["__typename"] == "Bot"
        || login.ends_with("[bot]")
        || DEPENDENCY_BOTS.contains(&login.as_str())
}

/// The repositories of the project's issues and pull requests.
fn covered(items: &[Value]) -> BTreeSet<String> {
    items
        .iter()
        .filter_map(|i| i["content"]["repository"]["nameWithOwner"].as_str())
        .map(str::to_owned)
        .collect()
}

fn format_time(at: OffsetDateTime) -> Result<String> {
    Ok(at
        .to_offset(time::UtcOffset::UTC)
        .replace_nanosecond(0)?
        .format(&Rfc3339)?)
}

fn has(labels: impl IntoIterator<Item = impl AsRef<str>>, wanted: &[&str]) -> bool {
    labels
        .into_iter()
        .any(|l| wanted.iter().any(|w| l.as_ref().eq_ignore_ascii_case(w)))
}

fn link(content: &Value) -> Result<Link> {
    let repo = text(&content["repository"], "nameWithOwner")?;
    let number = content["number"]
        .as_u64()
        .context("missing GitHub number")?;
    Ok(Link {
        reference: format!("{repo}#{number}"),
        url: text(content, "url")?.to_owned(),
    })
}

/// Sort and identity key: refs compare without case, as GitHub's names do.
fn key(reference: &str) -> (String, u64) {
    let (repo, number) = reference.rsplit_once('#').unwrap_or((reference, ""));
    (repo.to_ascii_lowercase(), number.parse().unwrap_or(0))
}

/// A nested connection's nodes, whole: one page is all [`ITEMS_QUERY`] reads.
fn whole<'a>(content: &'a Value, field: &str) -> Result<&'a [Value]> {
    let conn = &content[field];
    ensure!(
        conn["pageInfo"]["hasNextPage"] == false,
        "too many GitHub {field} on {} to list",
        content["url"].as_str().unwrap_or("a project item")
    );
    conn["nodes"]
        .as_array()
        .map(Vec::as_slice)
        .with_context(|| format!("missing GitHub {field}"))
}

/// Project item pages, as [`ITEMS_QUERY`] reads them, and pull requests
/// [`SEARCH_QUERY`] found, into sections; `authors` (lowercase) are those
/// whose new pull requests outside the board are listed.
pub fn classify(
    items: &[Value],
    searched: &[Value],
    since: &str,
    authors: &BTreeSet<String>,
) -> Result<Changes> {
    classify_window(items, searched, since, None, authors)
}

/// [`classify`] in a window that ends at `until`, when given: what happened
/// after it is as if it had not happened yet. Labels are read as they are
/// now, so a hold released after the end may already show as released.
pub fn classify_window(
    items: &[Value],
    searched: &[Value],
    since: &str,
    until: Option<&str>,
    authors: &BTreeSet<String>,
) -> Result<Changes> {
    let cutoff = parse_time(since)?;
    let end = until.map(parse_time).transpose()?;
    // Whether `field` is a time in the window.
    let after = |content: &Value, field: &str| -> Result<bool> {
        match &content[field] {
            Value::Null => Ok(false),
            Value::String(at) => {
                let at = parse_time(at)?;
                Ok(at > cutoff && end.is_none_or(|end| at <= end))
            }
            _ => bail!("invalid GitHub {field}"),
        }
    };
    let mut found = vec![];
    let mut seen = BTreeSet::new();
    for item in items {
        let content = &item["content"];
        match content["__typename"].as_str() {
            Some("Issue" | "PullRequest") => (),
            Some("DraftIssue") => continue,
            _ => bail!("cannot list redacted project items"),
        }
        let line = Line {
            link: link(content)?,
            title: text(content, "title")?.to_owned(),
            closes: vec![],
            released: false,
        };
        ensure!(
            seen.insert(key(&line.link.reference)),
            "duplicate project content"
        );
        let labels: Vec<_> = whole(content, "labels")?
            .iter()
            .map(|l| text(l, "name"))
            .collect::<Result<_>>()?;
        let mut hold_changed = false;
        for event in whole(content, "timelineItems")? {
            if after(event, "createdAt")? && has([text(&event["label"], "name")?], HOLD_LABELS) {
                hold_changed = true;
            }
        }
        // A reopened item keeps no closure, whatever its closedAt says.
        let state = text(content, "state")?;
        let merged = state == "MERGED" && content["merged"] == true && after(content, "mergedAt")?;
        let section = if merged {
            Section::Landed
        } else if after(content, "closedAt")? && state != "OPEN" {
            Section::Closed
        } else if after(content, "createdAt")? {
            Section::Opened
        } else if hold_changed {
            Section::Held
        } else {
            continue;
        };
        // Only a merge closes anything; an open pull request's references are intent.
        let closes = if merged {
            whole(content, "closingIssuesReferences")?
                .iter()
                .map(link)
                .collect::<Result<_>>()?
        } else {
            vec![]
        };
        let released = section == Section::Held && !has(&labels, HOLD_LABELS);
        found.push((
            section,
            Line {
                closes,
                released,
                ..line
            },
        ));
    }
    // A pull request that is also an item, or that both searches found, is
    // told once.
    let mut bots: BTreeMap<Section, Bots> = BTreeMap::new();
    for content in searched {
        if content["__typename"] != "PullRequest" {
            continue;
        }
        let link = link(content)?;
        if !seen.insert(key(&link.reference)) {
            continue;
        }
        let merged = text(content, "state")? == "MERGED"
            && content["merged"] == true
            && after(content, "mergedAt")?;
        let bot = by_bot(content);
        // Open at the window's end: now, or, in a window that ends, not yet
        // merged or closed then, whatever it is now.
        let open = match end {
            None => content["state"] == "OPEN",
            Some(end) => match &content["closedAt"] {
                Value::Null => true,
                Value::String(at) => parse_time(at)? > end,
                _ => bail!("invalid GitHub closedAt"),
            },
        };
        let section = if merged {
            Section::Landed
        } else if open && after(content, "createdAt")? {
            Section::Opened
        } else {
            continue;
        };
        if bot {
            let counted = bots.entry(section).or_default();
            counted.count += 1;
            counted
                .repositories
                .insert(text(&content["repository"], "nameWithOwner")?.to_owned());
            continue;
        }
        let author = content["author"]["login"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if section == Section::Opened && !authors.contains(&author) {
            continue;
        }
        let closes = if merged {
            whole(content, "closingIssuesReferences")?
                .iter()
                .map(self::link)
                .collect::<Result<_>>()?
        } else {
            vec![]
        };
        let title = text(content, "title")?.to_owned();
        found.push((
            section,
            Line {
                link,
                title,
                closes,
                released: false,
            },
        ));
    }
    // An issue a landed pull request closes is told on that line.
    let closed_by_landing: BTreeSet<_> = found
        .iter()
        .filter(|(s, _)| *s == Section::Landed)
        .flat_map(|(_, l)| l.closes.iter().map(|c| key(&c.reference)))
        .collect();
    let mut sections: BTreeMap<Section, Vec<Line>> = BTreeMap::new();
    for (section, line) in found {
        if section == Section::Closed && closed_by_landing.contains(&key(&line.link.reference)) {
            continue;
        }
        sections.entry(section).or_default().push(line);
    }
    for lines in sections.values_mut() {
        lines.sort_by_key(|l| key(&l.link.reference));
    }
    Ok(Changes {
        since: since.to_owned(),
        until: until.map(str::to_owned),
        sections,
        bots,
        repositories: covered(items),
    })
}

/// Third-party text inside a list line: no links, HTML, mentions or line breaks of its own.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' | '[' | ']' | '<' | '>' | '@' => {
                out.push('\\');
                out.push(c);
            }
            '\r' | '\n' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

fn markdown_link(link: &Link) -> String {
    format!("[{}]({})", link.reference, link.url)
}

fn issue_url(reference: &str) -> Option<String> {
    let (repo, number) = parse_ref(reference)?;
    Some(format!("https://github.com/{repo}/issues/{number}"))
}

/// `owner/repo#N`, as a note's key or a summary's reference names an item.
pub fn parse_ref(reference: &str) -> Option<(&str, u64)> {
    let (repo, number) = reference.split_once('#')?;
    crate::poll::validate_repository(repo).ok()?;
    let valid = !number.is_empty()
        && !number.starts_with('0')
        && number.bytes().all(|b| b.is_ascii_digit());
    Some((repo, number.parse().ok().filter(|_| valid)?))
}

/// `ref` as a full `owner/repo#N`: as it is, or a short `repo#N` that names
/// exactly one of `repositories`.
fn resolve(reference: &str, repositories: &BTreeSet<String>) -> Option<String> {
    if parse_ref(reference).is_some() {
        return Some(reference.to_owned());
    }
    let (name, number) = reference.split_once('#')?;
    if !is_short(reference) {
        return None;
    }
    let mut matching = repositories.iter().filter(|r| {
        r.split_once('/')
            .is_some_and(|(_, n)| n.eq_ignore_ascii_case(name))
    });
    match (matching.next(), matching.next()) {
        (Some(repo), None) => Some(format!("{repo}#{number}")),
        _ => None,
    }
}

/// `repo#N`, a short ref, by its form alone.
fn is_short(reference: &str) -> bool {
    reference.split_once('#').is_some_and(|(name, number)| {
        (1..=100).contains(&name.len())
            && name != "."
            && name != ".."
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            && !number.starts_with('0')
            && !number.is_empty()
            && number.bytes().all(|b| b.is_ascii_digit())
    })
}

/// Whether a note may be keyed `reference`: a full or a short ref.
pub fn is_ref(reference: &str) -> bool {
    parse_ref(reference).is_some() || is_short(reference)
}

/// Characters a GitHub login or repository name may hold, plus the slash.
fn ref_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"._-/".contains(&b)
}

/// `summary` with every bare ref `resolve` makes full made a link: to the
/// item's URL when `known` has it, else to its issue URL, which GitHub
/// redirects for a pull request. One already in a link's text, or part of a
/// longer word or URL, stays as it is.
fn linkify(
    summary: &str,
    resolve: &dyn Fn(&str) -> Option<String>,
    known: &dyn Fn(&str) -> Option<String>,
) -> String {
    let bytes = summary.as_bytes();
    let mut out = String::with_capacity(summary.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'#' {
            i += 1;
            continue;
        }
        let mut start = i;
        while start > copied && ref_char(bytes[start - 1]) {
            start -= 1;
        }
        let mut end = i + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        let candidate = &summary[start..end];
        let boundary_before = start == 0 || !(bytes[start - 1] == b':' || bytes[start - 1] == b'[');
        let boundary_after = end == bytes.len()
            || !(bytes[end].is_ascii_alphanumeric() || b"_]#/".contains(&bytes[end]));
        if boundary_before
            && boundary_after
            && let Some(full) = resolve(candidate)
            && let Some(url) = known(&full).or_else(|| issue_url(&full))
        {
            out.push_str(&summary[copied..start]);
            let _ = write!(out, "[{candidate}]({url})");
            copied = end;
        }
        i = end;
    }
    out.push_str(&summary[copied..]);
    out
}

/// One section's lines, each with whether a note is attached, and the
/// repository it is grouped under, if the section is grouped.
struct Block {
    heading: &'static str,
    lines: Vec<(String, bool)>,
    groups: Option<Vec<String>>,
    /// The bots' count, last and never trimmed.
    tail: Option<String>,
    hidden: usize,
}

impl Block {
    /// Lines shown with `hidden` removed: un-noted lines go first, last first.
    fn shown(&self) -> impl Iterator<Item = (usize, &str)> {
        let mut dropped = BTreeSet::new();
        let order = self
            .lines
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, (_, noted))| !noted)
            .chain(
                self.lines
                    .iter()
                    .enumerate()
                    .rev()
                    .filter(|(_, (_, noted))| *noted),
            );
        dropped.extend(order.take(self.hidden).map(|(i, _)| i));
        self.lines
            .iter()
            .enumerate()
            .filter(move |(i, _)| !dropped.contains(i))
            .map(|(i, (l, _))| (i, l.as_str()))
    }

    fn render(&self) -> String {
        let mut out = format!("**{}**\n", self.heading);
        let mut group = None;
        for (i, line) in self.shown() {
            match &self.groups {
                Some(groups) => {
                    if group != Some(&groups[i]) {
                        group = Some(&groups[i]);
                        let _ = writeln!(out, "- {}", groups[i]);
                    }
                    let _ = writeln!(out, "  - {line}");
                }
                None => {
                    let _ = writeln!(out, "- {line}");
                }
            }
        }
        if self.hidden > 0 {
            let _ = writeln!(out, "- +{} more", self.hidden);
        }
        if let Some(tail) = &self.tail {
            let _ = writeln!(out, "- {tail}");
        }
        out
    }
}

impl Changes {
    /// The whole list, as `agent-board changes` prints it.
    pub fn markdown(&self) -> String {
        // Without notes or a limit, nothing can refuse it.
        self.render(None, &BTreeMap::new(), usize::MAX)
            .unwrap_or_default()
    }

    /// The list in at most `limit` characters, trimmed as [`Self::status_body`]
    /// trims: the sections with the most lines lose their last lines first,
    /// each ending with `+K more`.
    pub fn markdown_within(&self, limit: usize) -> Result<String> {
        self.render(None, &BTreeMap::new(), limit)
    }

    /// A status update body of at most `limit` characters: `summary` on
    /// top, then the sections, `notes` (keyed `owner/repo#N`) attached to
    /// their lines or listed under **Also**.
    pub fn status_body(
        &self,
        summary: Option<&str>,
        notes: &BTreeMap<String, String>,
        limit: usize,
    ) -> Result<String> {
        self.render(summary, notes, limit)
    }

    fn render(
        &self,
        summary: Option<&str>,
        notes: &BTreeMap<String, String>,
        limit: usize,
    ) -> Result<String> {
        let mut keyed = BTreeMap::new();
        for (r, n) in notes {
            ensure!(is_ref(r), "invalid note reference {r:?}");
            // A short ref no covered repository resolves stays plain text.
            let line = match resolve(r, &self.repositories) {
                Some(full) => {
                    let url = issue_url(&full)
                        .with_context(|| format!("invalid note reference {r:?}"))?;
                    let link = Link {
                        reference: r.clone(),
                        url,
                    };
                    (key(&full), markdown_link(&link))
                }
                None => ((r.to_ascii_lowercase(), 0), r.clone()),
            };
            ensure!(
                keyed.insert(line.0, (line.1, n.as_str())).is_none(),
                "more than one note for {r}"
            );
        }
        let mut notes = keyed;
        // A note goes on its item's own line; on a landing that closes the
        // item only when the item has no line of its own.
        let own: BTreeSet<_> = self
            .sections
            .values()
            .flatten()
            .map(|l| key(&l.link.reference))
            .collect();
        let mut blocks = vec![];
        let sections: BTreeSet<_> = self.sections.keys().chain(self.bots.keys()).collect();
        for section in sections {
            let lines = self
                .sections
                .get(section)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let tail = self.bots.get(section).map(|b| {
                let repos: Vec<_> = b.repositories.iter().map(String::as_str).collect();
                format!(
                    "{} dependency and bot pull requests in {}",
                    b.count,
                    repos.join(", ")
                )
            });
            let repos: Vec<_> = lines.iter().map(|l| key(&l.link.reference).0).collect();
            let grouped = *section == Section::Landed
                && lines.len() > GROUP_LANDED
                && repos.iter().any(|r| *r != repos[0]);
            let groups = grouped.then(|| {
                lines
                    .iter()
                    .map(|l| {
                        l.link
                            .reference
                            .rsplit_once('#')
                            .map_or(l.link.reference.clone(), |(r, _)| r.to_owned())
                    })
                    .collect()
            });
            let lines = lines
                .iter()
                .map(|l| {
                    let mut text = format!("{} {}", markdown_link(&l.link), escape(&l.title));
                    if !l.closes.is_empty() {
                        let closes: Vec<_> = l.closes.iter().map(markdown_link).collect();
                        let _ = write!(text, " (closes {})", closes.join(", "));
                    }
                    if l.released {
                        text.push_str(" (released)");
                    }
                    let note = notes.remove(&key(&l.link.reference)).or_else(|| {
                        l.closes
                            .iter()
                            .map(|c| key(&c.reference))
                            .filter(|k| !own.contains(k))
                            .find_map(|k| notes.remove(&k))
                    });
                    if let Some((_, note)) = note {
                        let _ = write!(text, " — {}", note.trim());
                    }
                    (text, note.is_some())
                })
                .collect();
            blocks.push(Block {
                heading: section.heading(),
                lines,
                groups,
                tail,
                hidden: 0,
            });
        }
        let empty = blocks.is_empty();
        if !notes.is_empty() {
            let lines = notes
                .into_values()
                .map(|(link, note)| (format!("{link} — {}", note.trim()), true))
                .collect();
            blocks.push(Block {
                heading: "Also",
                lines,
                groups: None,
                tail: None,
                hidden: 0,
            });
        }
        let known = |r: &str| {
            let k = key(r);
            self.sections
                .values()
                .flatten()
                .flat_map(|l| std::iter::once(&l.link).chain(&l.closes))
                .find(|l| key(&l.reference) == k)
                .map(|l| l.url.clone())
        };
        let mut head = vec![];
        if let Some(summary) = summary {
            let resolve = |r: &str| resolve(r, &self.repositories);
            head.push(format!("{}\n", linkify(summary.trim(), &resolve, &known)));
        }
        if empty {
            head.push(match &self.until {
                Some(until) => format!("No changes from {} to {until}\n", self.since),
                None => format!("No changes since {}\n", self.since),
            });
        }
        loop {
            let mut parts = head.clone();
            parts.extend(blocks.iter().map(Block::render));
            let body = parts.join("\n");
            if body.chars().count() <= limit {
                return Ok(body);
            }
            let Some(longest) = blocks
                .iter_mut()
                .filter(|b| b.hidden < b.lines.len())
                .max_by_key(|b| b.lines.len() - b.hidden)
            else {
                bail!("status body exceeds {limit} characters even with every line trimmed");
            };
            longest.hidden += 1;
        }
    }
}

#[cfg(test)]
mod tests;
