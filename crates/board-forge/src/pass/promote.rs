//! Stage 5 of a pass (docs/pass.md#order-and-effects): a pull request the
//! operator approved in one of our forks opened in the project the fork
//! tracks. It replaces homegit's `bot-promote-due` and `bot-pr promote`
//! (docs/architecture.md#promote).
//!
//! The approving review of the pull request's current head is the whole
//! signal; where it goes is configuration, never the body. Before opening
//! anything the stage checks the upstream's contribution-policy record, the
//! stack the pull request may sit on, the branch against the upstream base
//! and, where the upstream wants DCO, adds the operator's sign-off to the
//! commits that lack it over the git-data API. Every refusal is one line
//! and, where it needs the operator, one notice on the fork pull request,
//! said once. Everything the stage needs to know on the next pass is on the
//! forge: the fork pull request's receipt and the notice markers.
//!
//! Nothing here runs `git`: reads and writes go through [`Rest`] and
//! [`Transport`], within the pass's budget, and so replay. The ref update
//! is GraphQL's `updateRefs` with the approved head as its expected old
//! value, a lease the REST ref update lacks, so a push by anyone else is
//! never overwritten.
use super::*;
use crate::FOOTER;
use crate::github::NotFound;
use crate::plan::{fenced_json, issue_comments, newest_plan, no_plan};

/// The default `promote.policyPath`: `OWNER/REPO.md` records under it.
pub(super) const POLICY_PATH: &str = "upstream-policy";
/// The fence of the stage's JSON on a fork pull request, and the word in
/// its hidden notice markers.
pub const PROMOTE_MARKER: &str = "agent-board-promote";
/// The schema of that JSON.
pub const RECEIPT_SCHEMA: &str = "agent-board/promote/v1";
/// The line in the approving review's body that says the operator wrote the
/// pull request's text, where the policy wants a person's words.
pub const HUMAN_TEXT: &str = "--human-text";
/// Pages of a fork's open pull requests read, 100 each; more is unread.
const PULL_PAGES: usize = 3;
/// Pages of one pull request's reviews or comments read, 100 each.
const REVIEW_PAGES: usize = 3;
const COMMENT_PAGES: usize = 5;
/// Commits a series may have: a longer one is not proved and not promoted.
const SERIES_LIMIT: usize = 100;
/// Commits of a policy record's history read; a longer history is held.
const POLICY_HISTORY_LIMIT: usize = 20;
/// Characters of a policy record quoted in a notice.
const QUOTE_LIMIT: usize = 2000;
/// Requests one promotion takes besides two a commit it signs: the pull
/// request and ref read again, the record, the head repository's id, the
/// ref update and its read, the ref read before the pull request, the pull
/// request, the receipt, the close, and room for a page more of each.
const PROMOTE_REQUESTS: usize = 12;
/// Verdicts, loosest to strictest, as policy records name them.
const VERDICTS: [&str; 4] = ["bot-ok", "human-text", "human-only", "no-go"];
/// Files a policy record must list, by the start of their name, and words
/// in a name that make any file one, wherever they are in [`POLICY_DIRS`].
const POLICY_NAMES: &[&str] = &[
    "contributing",
    "agents",
    "claude",
    "gemini",
    "copilot-instructions",
    "pull_request_template",
    "dco",
];
const POLICY_WORDS: &[&str] = &["ai", "llm"];
const POLICY_DIRS: &[&str] = &["", ".github", "docs", "doc"];
/// Notice reasons, each said once per approval and head.
const HEAD_MOVED: &str = "head-moved";
const HELD: &str = "held";
const NEEDS_TEXT: &str = "needs-text";
const REFUSED: &str = "refused";
/// Pull requests of the upstream whose checks say whether it wants DCO.
const DCO_PULLS: usize = 5;

/// The issues a fork pull request closes, whose plans may stack it.
const CLOSING_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) { pullRequest(number: $number) {
    closingIssuesReferences(first: 20) { nodes { number repository { nameWithOwner } } totalCount }
  } }
}"#;

/// The pull requests, open and closed, that close the lower slice's issue.
const LOWER_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) { issue(number: $number) {
    closedByPullRequestsReferences(first: 50, includeClosedPrs: true) {
      nodes { number state url headRefName headRepository { nameWithOwner } author { login } repository { nameWithOwner } }
      totalCount
    }
  } }
}"#;

/// Who last edited a pull request's body and renamed its title, and when.
const TEXT_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) { pullRequest(number: $number) {
    lastEditedAt editor { login }
    timelineItems(last: 1, itemTypes: [RENAMED_TITLE_EVENT]) { nodes { ... on RenamedTitleEvent { createdAt actor { login } } } }
  } }
}"#;

/// The branch moved to the signed head only while it is at the approved
/// one: `beforeOid` is a lease, and GitHub rejects the update otherwise.
const UPDATE_REF: &str = r#"mutation($repo: ID!, $name: GitRefname!, $before: GitObjectID!, $after: GitObjectID!) {
  updateRefs(input: {repositoryId: $repo, refUpdates: [{name: $name, beforeOid: $before, afterOid: $after, force: true}]}) { clientMutationId }
}"#;

/// A branch, directory or path as it may go into an API path: plain ref
/// characters, no `..`, not led by `-` or `/`.
pub(super) fn validate_ref(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 200
            && !name.contains("..")
            && !name.starts_with(['-', '/'])
            && !name.ends_with('/')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)),
        "{name:?} is not a plain branch or path name"
    );
    Ok(())
}

/// The first seven characters of a sha, as lines name commits.
fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

fn login<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

// ---- The fork pull request and its approval ----

/// An open pull request of the bot in a fork, as promotion reads it.
#[derive(Clone, Debug)]
struct Pull {
    fork: String,
    number: u64,
    title: String,
    body: String,
    head: String,
    branch: String,
    /// The repository the head branch is in: the fork, or the bot's fork of it.
    head_repo: String,
}

impl Pull {
    fn name(&self) -> String {
        format!("{}#{}", self.fork, self.number)
    }

    /// `OWNER:BRANCH`, as an upstream pull request names its head.
    fn head_ref(&self) -> String {
        let owner = self.head_repo.split('/').next().unwrap_or_default();
        format!("{owner}:{}", self.branch)
    }

    fn from_json(fork: &str, value: &Value) -> Result<Self> {
        let number = value["number"]
            .as_u64()
            .context("pull request has no number")?;
        let head = &value["head"];
        let head_repo = text(head, "/repo/full_name");
        validate_repository(head_repo)
            .with_context(|| format!("{fork}#{number}'s head repository"))?;
        let branch = text(head, "/ref");
        validate_ref(branch).with_context(|| format!("{fork}#{number}'s head branch"))?;
        let sha = text(head, "/sha");
        ensure!(is_sha(sha), "{fork}#{number} has no head sha");
        Ok(Self {
            fork: fork.into(),
            number,
            title: text(value, "/title").into(),
            body: value["body"].as_str().unwrap_or_default().into(),
            head: sha.into(),
            branch: branch.into(),
            head_repo: head_repo.into(),
        })
    }
}

fn is_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The operator's approving review.
#[derive(Clone, Debug)]
struct Approval {
    id: u64,
    url: String,
    /// The head it approved.
    commit: String,
    body: String,
    at: OffsetDateTime,
}

impl Approval {
    /// Whether its body says the operator wrote the text ([`HUMAN_TEXT`]).
    fn human_text(&self) -> bool {
        self.body.lines().any(|l| l.trim() == HUMAN_TEXT)
    }
}

/// The operator's latest decision on a pull request: the last of their
/// reviews that approved, requested changes or was dismissed. A comment
/// review after an approval leaves it; a change request or a dismissal
/// cancels it. Anyone else's reviews say nothing.
fn decision(reviews: &[Value], operator: &str) -> Result<Option<Approval>> {
    let mut last: Option<(OffsetDateTime, &Value)> = None;
    for review in reviews {
        if !login(review, "/user/login").is_some_and(|l| l.eq_ignore_ascii_case(operator))
            || !matches!(
                review["state"].as_str(),
                Some("APPROVED" | "CHANGES_REQUESTED" | "DISMISSED")
            )
        {
            continue;
        }
        let at = OffsetDateTime::parse(text(review, "/submitted_at"), &Rfc3339)
            .context("review has no submission time")?;
        if last.is_none_or(|(when, _)| at >= when) {
            last = Some((at, review));
        }
    }
    let Some((at, review)) = last else {
        return Ok(None);
    };
    if review["state"] != "APPROVED" {
        return Ok(None);
    }
    let commit = text(review, "/commit_id");
    ensure!(is_sha(commit), "the approving review names no commit");
    Ok(Some(Approval {
        id: review["id"].as_u64().context("review has no id")?,
        url: text(review, "/html_url").into(),
        commit: commit.into(),
        body: review["body"].as_str().unwrap_or_default().into(),
        at,
    }))
}

/// A comment on the fork pull request, as read over REST.
#[derive(Clone, Debug)]
struct Note {
    url: String,
    author: Option<String>,
    body: String,
    /// Whether it changed after it was posted. REST names no editor, and
    /// anyone who may edit others' comments could put a marker or a record
    /// into the bot's; the bot never edits its own, so an edited one is not
    /// trusted as the bot's words ([`Note::bots`]).
    edited: bool,
}

impl Note {
    /// Whether it is `bot`'s comment as `bot` posted it.
    fn bots(&self, bot: &str) -> bool {
        !self.edited
            && self
                .author
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(bot))
    }
}

/// The bot's record of its own sign-off push: the approved head became the
/// signed one. Trusted only once both series are read and compared.
#[derive(Clone, Debug, PartialEq)]
struct Record {
    url: String,
    approval: u64,
    approved: String,
    signed: String,
}

/// The newest sign-off record among `notes` by `bot`; a record that cannot
/// be read is an error, since a replay past it could sign twice.
fn record(notes: &[Note], bot: &str) -> Result<Option<Record>> {
    let mut found = None;
    for note in notes {
        if !note.bots(bot) {
            continue;
        }
        let Some(json) = fenced_json(&note.body, PROMOTE_MARKER)
            .with_context(|| format!("record {}", note.url))?
        else {
            continue;
        };
        if json["kind"] != "signoff" {
            continue;
        }
        ensure!(
            json["schema"] == RECEIPT_SCHEMA,
            "record {} is not a {RECEIPT_SCHEMA} record",
            note.url
        );
        let sha = |key: &str| {
            let sha = text(&json, &format!("/{key}"));
            ensure!(is_sha(sha), "record {} has no {key} sha", note.url);
            Ok(sha.to_owned())
        };
        found = Some(Record {
            url: note.url.clone(),
            approval: json["approval"]
                .as_u64()
                .with_context(|| format!("record {} names no approval", note.url))?,
            approved: sha("approved")?,
            signed: sha("signed")?,
        });
    }
    Ok(found)
}

/// The hidden marker of a notice, `<!-- agent-board-promote notice=KEY -->`.
fn marker(key: &str) -> String {
    format!("<!-- {PROMOTE_MARKER} notice={key} -->")
}

/// A notice's key: the reason, the approval and the head, so that the same
/// refusal is said once and a new approval or head says it again.
/// `head-moved` is keyed by the approval alone: it is said once however
/// often the head moves afterwards.
fn notice_key(reason: &str, approval: u64, head: &str) -> String {
    match reason {
        HEAD_MOVED => fnv(&format!("{reason}\n{approval}")),
        _ => fnv(&format!("{reason}\n{approval}\n{head}")),
    }
}

/// The notice already posted with `key`, by its URL: a comment of `bot`
/// with the marker as a line of its own, unedited. Anyone else's comment,
/// or theirs edited into one of the bot's, could paste the marker, since
/// the key is public, and so hold a promotion or silence a notice.
fn noticed(notes: &[Note], bot: &str, key: &str) -> Option<String> {
    let marker = marker(key);
    notes
        .iter()
        .find(|n| n.bots(bot) && n.body.lines().any(|l| l.trim() == marker))
        .map(|n| n.url.clone())
}

// ---- Text ----

/// Whether `text` refers to an issue by a bare `#N` or `GH-N`, which
/// upstream would read as its own issue N. `OWNER/REPO#N` and URLs are
/// left to GitHub.
fn bare_reference(text: &str) -> Option<String> {
    text.split_whitespace().find_map(|word| {
        let word = word.trim_start_matches(['(', '[', '*', '_', '`', '"', '\'']);
        let word = word.trim_end_matches([
            ',', '.', ';', ':', ')', ']', '!', '?', '*', '_', '`', '"', '\'',
        ]);
        let digits = word
            .strip_prefix('#')
            .or_else(|| word.strip_prefix("GH-"))?;
        (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then(|| word.to_owned())
    })
}

/// Who last changed a pull request's text, and when: the description's
/// last edit and the title's last rename.
#[derive(Clone, Debug, Default)]
struct Text {
    edited: Option<OffsetDateTime>,
    editor: Option<String>,
    renamed: Option<OffsetDateTime>,
    renamer: Option<String>,
}

impl Text {
    /// Why the text is not the one the approval covered: an edit or a
    /// rename after the approval by anyone but the operator. The approving
    /// review names the head, not the words, so the words must not have
    /// changed under it.
    fn since(&self, approval: &Approval, operator: &str) -> Option<String> {
        let theirs = |who: &Option<String>| {
            who.as_deref()
                .is_some_and(|w| w.eq_ignore_ascii_case(operator))
        };
        if self.edited.is_some_and(|at| at > approval.at) && !theirs(&self.editor) {
            return Some(format!(
                "its description was edited after the approval by {}",
                self.editor.as_deref().unwrap_or("an unknown account")
            ));
        }
        if self.renamed.is_some_and(|at| at > approval.at) && !theirs(&self.renamer) {
            return Some(format!(
                "its title was changed after the approval by {}",
                self.renamer.as_deref().unwrap_or("an unknown account")
            ));
        }
        None
    }
}

// ---- Commits ----

#[derive(Clone, Debug, PartialEq)]
struct Person {
    name: String,
    email: String,
    date: String,
}

impl Person {
    fn from_json(value: &Value) -> Result<Self> {
        let field = |key: &str| {
            value[key]
                .as_str()
                .map(str::to_owned)
                .with_context(|| format!("commit identity has no {key}"))
        };
        Ok(Self {
            name: field("name")?,
            email: field("email")?,
            date: field("date")?,
        })
    }

    fn json(&self) -> Value {
        json!({"name": self.name, "email": self.email, "date": self.date})
    }
}

/// One commit of a series, as `compare` and `git/commits` show it.
#[derive(Clone, Debug, PartialEq)]
struct Commit {
    sha: String,
    tree: String,
    parents: Vec<String>,
    author: Person,
    committer: Person,
    message: String,
    /// The GitHub account of its author, where GitHub matched one.
    login: Option<String>,
}

impl Commit {
    /// A commit as `compare` lists it, or as `git/commits` answers it
    /// (`commit` then is the answer itself).
    fn from_json(value: &Value, commit: &Value) -> Result<Self> {
        let sha = text(value, "/sha");
        ensure!(is_sha(sha), "commit has no sha");
        let tree = text(commit, "/tree/sha");
        ensure!(is_sha(tree), "commit {} has no tree", short(sha));
        let parents = value["parents"]
            .as_array()
            .with_context(|| format!("commit {} has no parents", short(sha)))?
            .iter()
            .map(|p| text(p, "/sha").to_owned())
            .collect();
        Ok(Self {
            sha: sha.into(),
            tree: tree.into(),
            parents,
            author: Person::from_json(&commit["author"])?,
            committer: Person::from_json(&commit["committer"])?,
            message: commit["message"].as_str().unwrap_or_default().into(),
            login: login(value, "/author/login").map(str::to_owned),
        })
    }
}

/// A branch's commits past the upstream base, oldest first.
#[derive(Clone, Debug, PartialEq)]
struct Series {
    merge_base: String,
    commits: Vec<Commit>,
}

/// Whether a line of a commit message is a trailer, `Token: value`.
fn is_trailer(line: &str) -> bool {
    line.split_once(": ").is_some_and(|(token, value)| {
        !token.is_empty()
            && token
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic())
            && token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !value.trim().is_empty()
    })
}

/// `message` with `trailer` appended as `git interpret-trailers` would: at
/// the end of its trailer block, after a blank line where it has none.
fn with_trailer(message: &str, trailer: &str) -> String {
    let body = message.trim_end_matches(['\n', ' ']);
    let last = body.rsplit("\n\n").next().unwrap_or(body);
    let block = body.contains("\n\n") && last.lines().all(is_trailer);
    match block {
        true => format!("{body}\n{trailer}"),
        false => format!("{body}\n\n{trailer}"),
    }
}

/// How often `message` carries `trailer` as a line.
fn trailers(message: &str, trailer: &str) -> usize {
    message.lines().filter(|l| l.trim() == trailer).count()
}

/// The commits of `before` that lack `trailer`, by index.
fn unsigned(before: &Series, trailer: &str) -> Vec<usize> {
    (before.commits.iter().enumerate())
        .filter(|(_, c)| trailers(&c.message, trailer) == 0)
        .map(|(n, _)| n)
        .collect()
}

/// Proves `after` is `before` plus `trailer` and nothing else: the same
/// merge base and count, each commit the same tree, author and message
/// apart from one trailer added where there was none, parents in order.
fn prove(before: &Series, after: &Series, trailer: &str) -> Result<()> {
    ensure!(
        before.merge_base == after.merge_base,
        "the merge base moved from {} to {}",
        short(&before.merge_base),
        short(&after.merge_base)
    );
    ensure!(
        before.commits.len() == after.commits.len(),
        "{} commits became {}",
        before.commits.len(),
        after.commits.len()
    );
    let mut parent = after.merge_base.clone();
    for (b, a) in before.commits.iter().zip(&after.commits) {
        let at = format!("{} as {}", short(&b.sha), short(&a.sha));
        // A merge rewritten on one parent would lose its other ancestry.
        ensure!(
            b.parents.len() == 1,
            "{at}: a merge commit is not rewritten"
        );
        ensure!(a.tree == b.tree, "{at}: the tree differs");
        ensure!(a.author == b.author, "{at}: the author differs");
        ensure!(a.parents == [parent.clone()], "{at}: the parent differs");
        let expected = match trailers(&b.message, trailer) {
            0 => with_trailer(&b.message, trailer),
            _ => b.message.clone(),
        };
        ensure!(
            a.message.trim_end() == expected.trim_end(),
            "{at}: the message differs beyond the trailer"
        );
        ensure!(
            trailers(&a.message, trailer) == 1,
            "{at}: the trailer is not there once"
        );
        parent = a.sha.clone();
    }
    Ok(())
}

// ---- Policy records ----

/// A contribution-policy record, `upstream-policy/OWNER/REPO.md` in the
/// policy repository: `---` front matter with `verdict`, `sources` (a list
/// of `repo`, `path` and `sha`, the blob id read on the default branch) and
/// other keys, then a body of quotes and rationale.
#[derive(Clone, Debug, PartialEq)]
struct Policy {
    verdict: String,
    sources: Vec<(String, String, String)>,
    body: String,
}

impl Policy {
    fn parse(text: &str) -> Result<Self> {
        let rest = text.strip_prefix("---\n").context("no front matter")?;
        let (front, body) = rest
            .split_once("\n---\n")
            .context("front matter is not closed")?;
        let mut verdict = None;
        let mut sources = Vec::new();
        let mut in_sources = false;
        let mut source: Option<(String, String, String)> = None;
        let mut finish = |source: &mut Option<(String, String, String)>| -> Result<()> {
            if let Some(s) = source.take() {
                ensure!(
                    !s.0.is_empty() && !s.1.is_empty() && is_sha(&s.2),
                    "a source lacks repo, path or sha"
                );
                sources.push(s);
            }
            Ok(())
        };
        for line in front.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(item) = line.strip_prefix("  - ") {
                ensure!(in_sources, "a list outside sources");
                finish(&mut source)?;
                source = Some((String::new(), String::new(), String::new()));
                set_source(&mut source, item)?;
                continue;
            }
            if let Some(field) = line.strip_prefix("    ") {
                ensure!(source.is_some(), "a field outside a source");
                set_source(&mut source, field.trim())?;
                continue;
            }
            finish(&mut source)?;
            in_sources = false;
            let (key, value) = line.split_once(':').context("a line without a key")?;
            let value = value.trim();
            match key.trim() {
                "verdict" => verdict = Some(value.to_owned()),
                "sources" => {
                    ensure!(value.is_empty() || value == "[]", "sources is not a list");
                    in_sources = value.is_empty();
                }
                _ => {}
            }
        }
        finish(&mut source)?;
        let verdict = verdict.context("no verdict")?;
        ensure!(
            VERDICTS.contains(&verdict.as_str()),
            "verdict {verdict:?} is not one of {}",
            VERDICTS.join(", ")
        );
        ensure!(!body.trim().is_empty(), "the record has no body");
        Ok(Self {
            verdict,
            sources,
            body: body.trim().into(),
        })
    }

    /// Its place among [`VERDICTS`], loosest first.
    fn rank(&self) -> usize {
        VERDICTS
            .iter()
            .position(|v| *v == self.verdict)
            .unwrap_or(VERDICTS.len())
    }

    /// Its body as a notice quotes it, within [`QUOTE_LIMIT`].
    fn quote(&self) -> String {
        let mut body: String = self.body.chars().take(QUOTE_LIMIT).collect();
        if body.len() < self.body.len() {
            body.push('…');
        }
        body.lines().map(|l| format!("> {l}\n")).collect()
    }
}

fn set_source(source: &mut Option<(String, String, String)>, field: &str) -> Result<()> {
    let (key, value) = field
        .split_once(':')
        .context("a source field without a key")?;
    let value = value.trim().trim_matches(['"', '\'']).to_owned();
    let Some(source) = source else {
        bail!("a field outside a source");
    };
    match key.trim() {
        "repo" => source.0 = value,
        "path" => source.1 = value,
        "sha" => source.2 = value,
        other => bail!("unknown source field {other}"),
    }
    Ok(())
}

/// Whether a file by this name says how a project takes contributions.
fn policy_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    POLICY_NAMES.iter().any(|p| lower.starts_with(p))
        || lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|word| POLICY_WORDS.contains(&word))
}

/// Base64 as the contents API encodes a file, line breaks included.
fn base64(text: &str) -> Result<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let (mut bits, mut buffer) = (0u32, 0u32);
    for byte in text.bytes() {
        if byte.is_ascii_whitespace() || byte == b'=' {
            continue;
        }
        let value = ALPHABET
            .iter()
            .position(|b| *b == byte)
            .context("not base64")? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

// ---- What one pull request comes to ----

/// Why a pull request is not promoted, for its line and, where it needs
/// the operator, its notice.
#[derive(Clone, Debug, PartialEq)]
struct Refusal {
    reason: &'static str,
    why: String,
    next: String,
    /// The policy record's words, where the policy itself blocks.
    quote: Option<String>,
    /// Whether the fork pull request gets the `needs-human` label.
    label: bool,
}

impl Refusal {
    fn new(reason: &'static str, why: impl Into<String>, next: impl Into<String>) -> Self {
        Self {
            reason,
            why: why.into(),
            next: next.into(),
            quote: None,
            label: false,
        }
    }
}

/// The promotion, read and checked, with nothing written yet.
#[derive(Clone, Debug)]
struct Plan {
    upstream: String,
    base: String,
    series: Series,
    /// The commits to sign, by index; none where the upstream wants no DCO
    /// or every commit carries the trailer.
    to_sign: Vec<usize>,
    /// The sign-off push recovered from a record: nothing is signed again.
    recovered: Option<Record>,
    /// Whether the operator wrote the text: nothing is added to it.
    human_text: bool,
}

enum Verdict {
    Promote(Box<Plan>),
    /// The upstream pull request is open with this head already: link it
    /// and close the fork pull request.
    Done(String),
    /// Not now, for a reason that needs no notice.
    Hold(String),
    Refuse(Box<Refusal>),
}

/// What is read once an upstream a pass.
#[derive(Default)]
struct Upstream {
    archived: bool,
    /// The policy gate: the record, or why it holds.
    policy: Option<std::result::Result<Policy, Refusal>>,
    dco: Option<bool>,
}

#[derive(Default)]
struct Cache {
    upstreams: BTreeMap<String, Upstream>,
    policy_branch: Option<String>,
}

/// A pull request ready to be promoted: approved on its head, or on the
/// head a sign-off record says it had.
struct Ready {
    pull: Pull,
    approval: Approval,
    notes: Vec<Note>,
    recovered: Option<Record>,
}

impl<'a, C: Rest + Transport> Scheduler<'a, C> {
    /// Stage 5: the bot's open pull requests in each configured fork that
    /// the operator approved on their current head, in order of approval,
    /// each checked and the first that passes promoted; the rest wait.
    pub(super) fn promote(&mut self) -> Result<()> {
        let config = self.config;
        let Some(promote) = config.promote.as_ref().filter(|p| !p.forks.is_empty()) else {
            self.line("promote: off".into());
            return Ok(());
        };
        let mut cache = Cache::default();
        let mut readied: Vec<Ready> = Vec::new();
        let (mut open, mut forks) = (0, 0);
        for (fork, settings) in &promote.forks {
            let pulls = match self.fork_pulls(fork, &settings.upstream) {
                Ok(pulls) => pulls,
                Err(error) => {
                    self.report.failures += 1;
                    self.line(format!("promote error: {fork}: {error:#}"));
                    continue;
                }
            };
            forks += 1;
            open += pulls.len();
            for pull in pulls {
                let pull = match pull {
                    Ok(pull) => pull,
                    Err(line) => {
                        self.line(line);
                        continue;
                    }
                };
                let name = pull.name();
                match self.readiness(pull) {
                    Ok(Some(ready)) => readied.push(ready),
                    Ok(None) => {}
                    Err(error) => {
                        self.report.failures += 1;
                        self.line(format!("promote error: {name}: {error:#}"));
                    }
                }
            }
        }
        self.line(format!(
            "promote: {} approved of {open} open pull request(s) of {} in {forks} fork(s)",
            readied.len(),
            config.bot
        ));
        readied.sort_by(|a, b| {
            let key = |r: &Ready| {
                (
                    r.approval.at,
                    r.pull.fork.to_ascii_lowercase(),
                    r.pull.number,
                )
            };
            key(a).cmp(&key(b))
        });
        let mut promoted = false;
        for ready in readied {
            let name = ready.pull.name();
            // Stale readiness: an approval of an older head without a record.
            if ready.approval.commit != ready.pull.head && ready.recovered.is_none() {
                let refusal = Refusal::new(
                    HEAD_MOVED,
                    format!(
                        "{} approved {}, but the head is now {}",
                        config.operator,
                        short(&ready.approval.commit),
                        short(&ready.pull.head)
                    ),
                    "approve the current head to promote it",
                );
                self.refuse(&ready, &refusal);
                continue;
            }
            if promoted {
                self.line(format!(
                    "hold promote {name}: waits; one pull request is promoted a pass"
                ));
                continue;
            }
            let settings = &promote.forks[&ready.pull.fork];
            match self.evaluate(&ready, settings, &mut cache) {
                Ok(Verdict::Promote(plan)) => {
                    promoted = true;
                    self.take(&ready, &plan);
                }
                Ok(Verdict::Done(url)) => self.finish(&ready, &settings.upstream, &url),
                Ok(Verdict::Hold(why)) => self.line(format!("hold promote {name}: {why}")),
                Ok(Verdict::Refuse(refusal)) => self.refuse(&ready, &refusal),
                Err(error) => {
                    self.report.failures += 1;
                    self.line(format!("promote error: {name}: {error:#}"));
                }
            }
        }
        Ok(())
    }

    // ---- Reads ----

    /// A 200 answer's body; anything else is an error.
    fn read(&self, url: &str, what: &str) -> Result<Value> {
        let response = self
            .client
            .get(url, None)
            .with_context(|| format!("reading {what}"))?;
        ensure!(
            response.status == 200,
            "reading {what}: GitHub HTTP status {}",
            response.status
        );
        Ok(response.body)
    }

    /// A 200 answer's body, or none for a 404, which is an answer here.
    fn maybe(&self, url: &str, what: &str) -> Result<Option<Value>> {
        match self.client.get(url, None) {
            Ok(response) if response.status == 200 => Ok(Some(response.body)),
            Ok(response) if response.status == 404 => Ok(None),
            Ok(response) => bail!("reading {what}: GitHub HTTP status {}", response.status),
            Err(error) if error.downcast_ref::<NotFound>().is_some() => Ok(None),
            Err(error) => Err(error.context(format!("reading {what}"))),
        }
    }

    /// Every element of the list at `url`, page by page up to `pages`; more
    /// is an error, as a list read in part could hide the one that matters.
    fn list(&self, url: &str, pages: usize, what: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut next = Some(url.to_owned());
        for _ in 0..pages {
            let Some(url) = next else {
                return Ok(out);
            };
            let response = self
                .client
                .get(&url, None)
                .with_context(|| format!("reading {what}"))?;
            ensure!(
                response.status == 200,
                "reading {what}: GitHub HTTP status {}",
                response.status
            );
            let page = response
                .body
                .as_array()
                .with_context(|| format!("{what} are not a list"))?;
            out.extend(page.iter().cloned());
            next = response.link.as_deref().and_then(next_link);
        }
        ensure!(next.is_none(), "{what} have more than {pages} pages");
        Ok(out)
    }

    /// The bot's open, ready pull requests in `fork` without `needs-human`,
    /// once the fork is a fork of `upstream`: its GitHub parent or network
    /// root, read, not assumed.
    fn fork_pulls(&self, fork: &str, upstream: &str) -> Result<Vec<Result<Pull, String>>> {
        let repo = self.read(&format!("{API_ROOT}repos/{fork}"), fork)?;
        let parent = text(&repo, "/parent/full_name");
        let source = text(&repo, "/source/full_name");
        ensure!(
            parent.eq_ignore_ascii_case(upstream) || source.eq_ignore_ascii_case(upstream),
            "its parent is {} and its network root {}, not {upstream}",
            if parent.is_empty() { "none" } else { parent },
            if source.is_empty() { "none" } else { source }
        );
        let default_branch = text(&repo, "/default_branch");
        let url = format!("{API_ROOT}repos/{fork}/pulls?state=open&per_page=100");
        let listed = self.list(&url, PULL_PAGES, &format!("{fork}'s open pull requests"))?;
        let bot = &self.config.bot;
        let mut pulls = Vec::new();
        for value in &listed {
            let ours = login(value, "/user/login").is_some_and(|l| l.eq_ignore_ascii_case(bot));
            let draft = value["draft"].as_bool().unwrap_or(true);
            let held = value["labels"].as_array().is_some_and(|l| {
                l.iter()
                    .any(|l| text(l, "/name").eq_ignore_ascii_case(NEEDS_HUMAN))
            });
            if !(ours && value["state"] == "open" && !draft && !held) {
                continue;
            }
            // One that cannot be read (its head fork deleted, say) holds
            // itself, not the fork's other pull requests.
            let pull = match Pull::from_json(fork, value) {
                Ok(pull) => pull,
                Err(error) => {
                    let number = value["number"].as_u64().unwrap_or_default();
                    pulls.push(Err(format!(
                        "hold promote {fork}#{number}: it cannot be read: {error:#}"
                    )));
                    continue;
                }
            };
            if pull.head_repo.eq_ignore_ascii_case(fork) && pull.branch == default_branch {
                pulls.push(Err(format!(
                    "hold promote {}: its head is the fork's default branch {default_branch}, which is never rewritten or promoted",
                    pull.name()
                )));
                continue;
            }
            pulls.push(Ok(pull));
        }
        Ok(pulls)
    }

    /// The operator's reviews of `pull`.
    fn reviews(&self, pull: &Pull) -> Result<Vec<Value>> {
        let url = format!(
            "{API_ROOT}repos/{}/pulls/{}/reviews?per_page=100",
            pull.fork, pull.number
        );
        self.list(&url, REVIEW_PAGES, &format!("{}'s reviews", pull.name()))
    }

    /// The comments on `pull`.
    fn notes(&self, pull: &Pull) -> Result<Vec<Note>> {
        let url = format!(
            "{API_ROOT}repos/{}/issues/{}/comments?per_page=100",
            pull.fork, pull.number
        );
        let listed = self.list(&url, COMMENT_PAGES, &format!("{}'s comments", pull.name()))?;
        listed
            .iter()
            .map(|c| {
                Ok(Note {
                    url: text(c, "/html_url").into(),
                    author: login(c, "/user/login").map(str::to_owned),
                    body: c["body"].as_str().unwrap_or_default().into(),
                    edited: c["updated_at"].is_string() && c["updated_at"] != c["created_at"],
                })
            })
            .collect()
    }

    /// Whether `pull` is approved on its head: by the operator's latest
    /// decision, or by their approval of the head a sign-off record says
    /// the bot moved to. An approval of an older head without a record is
    /// ready too, so that the loop says `head-moved` once.
    fn readiness(&self, pull: Pull) -> Result<Option<Ready>> {
        let reviews = self.reviews(&pull)?;
        let Some(approval) = decision(&reviews, &self.config.operator)? else {
            return Ok(None);
        };
        let notes = self.notes(&pull)?;
        let recovered = match approval.commit == pull.head {
            true => None,
            false => record(&notes, &self.config.bot)?.filter(|r| {
                r.approval == approval.id && r.approved == approval.commit && r.signed == pull.head
            }),
        };
        Ok(Some(Ready {
            pull,
            approval,
            notes,
            recovered,
        }))
    }

    /// The commits of `head` past `base` in `repo`, where they branch off,
    /// and how far `head` is behind `base`; `head` may be `OWNER:BRANCH` or
    /// a sha.
    fn series(&self, repo: &str, base: &str, head: &str) -> Result<(Series, u64)> {
        let url = format!("{API_ROOT}repos/{repo}/compare/{base}...{head}?per_page={SERIES_LIMIT}");
        let what = format!("{repo}'s {base}...{head}");
        let compare = self.read(&url, &what)?;
        let ahead = compare["ahead_by"]
            .as_u64()
            .context("compare has no ahead_by")?;
        let behind = compare["behind_by"]
            .as_u64()
            .context("compare has no behind_by")?;
        let commits = compare["commits"]
            .as_array()
            .context("compare has no commits")?
            .iter()
            .map(|c| Commit::from_json(c, &c["commit"]))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            commits.len() as u64 == ahead,
            "{what} has {ahead} commits, of which {} were read",
            commits.len()
        );
        let merge_base = text(&compare, "/merge_base_commit/sha");
        ensure!(is_sha(merge_base), "{what} has no merge base");
        let series = Series {
            merge_base: merge_base.into(),
            commits,
        };
        Ok((series, behind))
    }

    /// The sha a branch's ref points at now.
    fn ref_sha(&self, repo: &str, branch: &str) -> Result<String> {
        let url = format!("{API_ROOT}repos/{repo}/git/ref/heads/{branch}");
        let value = self.read(&url, &format!("{repo}'s {branch}"))?;
        let sha = text(&value, "/object/sha");
        ensure!(is_sha(sha), "{repo}'s {branch} has no sha");
        Ok(sha.into())
    }

    // ---- Evaluation ----

    /// Everything that must hold before `ready` is promoted, read and
    /// checked in the order homegit's `bot-pr promote` checked it, with
    /// nothing written.
    fn evaluate(&self, ready: &Ready, settings: &Fork, cache: &mut Cache) -> Result<Verdict> {
        let config = self.config;
        let promote = config.promote.as_ref().context("no promote")?;
        let signoff = promote.signoff.as_ref().context("no signoff")?;
        let pull = &ready.pull;
        let upstream = &settings.upstream;
        let name = pull.name();
        let refuse = |r: Refusal| Ok(Verdict::Refuse(Box::new(r)));
        // A refusal said already for this approval and head is not tried
        // again: it is the attempts cache, kept on the forge.
        let key = notice_key(REFUSED, ready.approval.id, &pull.head);
        if let Some(url) = noticed(&ready.notes, &config.bot, &key) {
            return Ok(Verdict::Hold(format!(
                "refused already for this approval and head, {url}; a new approval tries again"
            )));
        }
        // The head's repository is the fork, or another fork in the
        // upstream's network, such as the bot's fork of the fork; and the
        // branch is not that repository's default branch, which a sign-off
        // would force-push.
        if !pull.head_repo.eq_ignore_ascii_case(&pull.fork) {
            let repo = self.read(
                &format!("{API_ROOT}repos/{}", pull.head_repo),
                &pull.head_repo,
            )?;
            let source = text(&repo, "/source/full_name");
            let parent = text(&repo, "/parent/full_name");
            if !(source.eq_ignore_ascii_case(upstream) || parent.eq_ignore_ascii_case(upstream)) {
                return refuse(Refusal::new(
                    REFUSED,
                    format!(
                        "its head is in {}, which is not a fork of {upstream}",
                        pull.head_repo
                    ),
                    "push the branch to a fork in the upstream's network",
                ));
            }
            if text(&repo, "/default_branch") == pull.branch {
                return refuse(Refusal::new(
                    REFUSED,
                    format!(
                        "its head is {}'s default branch {}, which is never rewritten or promoted",
                        pull.head_repo, pull.branch
                    ),
                    "open the pull request from a topic branch",
                ));
            }
        }
        // Text: the title and body go upstream as they are, so they must
        // be the ones the approval saw.
        if let Some(why) = self.text(pull)?.since(&ready.approval, &config.operator) {
            return refuse(Refusal::new(
                REFUSED,
                why,
                "read the text as it is now and approve again if it is right",
            ));
        }
        if pull.title.trim().is_empty() || pull.body.trim().is_empty() {
            return refuse(Refusal::new(
                REFUSED,
                "its title or description is empty",
                "write the description the upstream pull request should have, then approve again",
            ));
        }
        if let Some(reference) = bare_reference(&pull.title).or_else(|| bare_reference(&pull.body))
        {
            return refuse(Refusal::new(
                REFUSED,
                format!(
                    "its text refers to {reference}, which {upstream} would read as its own issue"
                ),
                format!("write the reference as OWNER/REPO{reference}, then approve again"),
            ));
        }
        // The stack: a plan's `stacks_on` whose lower slice is not upstream yet.
        if let Some(verdict) = self.stacked(pull, upstream)? {
            return Ok(verdict);
        }
        if self.archived(upstream, cache)? {
            return refuse(Refusal::new(
                REFUSED,
                format!("{upstream} is archived"),
                "nothing; the project takes no pull requests",
            ));
        }
        // The contribution-policy gate, read once an upstream a pass.
        let policy = match self.policy(upstream, promote, cache)? {
            Ok(policy) => policy,
            Err(refusal) => return refuse(refusal),
        };
        let human_text = match policy.verdict.as_str() {
            "human-only" | "no-go" => {
                let mut refusal = Refusal::new(
                    HELD,
                    format!(
                        "{upstream}'s contribution policy is {}: the bot may not open pull requests there",
                        policy.verdict
                    ),
                    "open the pull request yourself if the work should go upstream; the record below says what the verdict rests on",
                );
                refusal.quote = Some(policy.quote());
                return refuse(refusal);
            }
            "human-text" => {
                if let Some(refusal) = self.human_text(ready, upstream)? {
                    return refuse(refusal);
                }
                true
            }
            _ => false,
        };
        // An upstream pull request from this branch, in any state.
        let head_ref = pull.head_ref();
        let url = format!("{API_ROOT}repos/{upstream}/pulls?state=all&head={head_ref}&per_page=10");
        let existing = self.list(
            &url,
            1,
            &format!("{upstream}'s pull requests from {head_ref}"),
        )?;
        // The newest one decides; an open one with this head is the
        // promotion done already, any other one refuses.
        if let Some(other) = existing.first() {
            let url = text(other, "/html_url");
            let open = other["state"] == "open";
            let merged = other["merged_at"].is_string();
            if open && text(other, "/head/sha") == pull.head {
                return Ok(Verdict::Done(url.into()));
            }
            let (why, next) = match (open, merged) {
                (true, _) => (
                    format!(
                        "{url} is open from this branch with head {}, not the approved {}",
                        short(text(other, "/head/sha")),
                        short(&pull.head)
                    ),
                    "see who changed it; the fork pull request is not promoted over it",
                ),
                (false, true) => (
                    format!("{url} from this branch was merged upstream already"),
                    "close this pull request if nothing of it is left",
                ),
                (false, false) => (
                    format!("{url} from this branch was closed upstream; it is not reopened"),
                    "open a new branch for what is left, or reopen it yourself",
                ),
            };
            return refuse(Refusal::new(REFUSED, why, next));
        }
        // The branch against the upstream base.
        let (series, behind) = self.series(upstream, &settings.base, &head_ref)?;
        let Some(compare_head) = series.commits.last().map(|c| c.sha.as_str()) else {
            return refuse(Refusal::new(
                REFUSED,
                format!(
                    "no commits ahead of {upstream} {}; merged already?",
                    settings.base
                ),
                "close this pull request if its work is upstream",
            ));
        };
        ensure!(
            compare_head == pull.head,
            "{name}'s branch is at {}, not its head {}: it moved while being read",
            short(compare_head),
            short(&pull.head)
        );
        if behind > 0 {
            return refuse(Refusal::new(
                REFUSED,
                format!(
                    "{behind} commit(s) behind {upstream} {}; this stage does not rebase",
                    settings.base
                ),
                "rebase the branch onto the upstream base and approve the new head",
            ));
        }
        let trailer = format!("Signed-off-by: {} <{}>", signoff.name, signoff.email);
        // A recovered sign-off push: the head is the record's signed sha,
        // proved against the approved series before anything else trusts it.
        if let Some(record) = &ready.recovered {
            let (before, _) = self.series(&pull.head_repo, &series.merge_base, &record.approved)?;
            if let Err(error) = prove(&before, &series, &trailer) {
                let mut refusal = Refusal::new(
                    HELD,
                    format!(
                        "its head {} is the bot's sign-off push by the record {}, but it does not prove as the approved {} plus the sign-off: {error:#}",
                        short(&pull.head),
                        record.url,
                        short(&record.approved)
                    ),
                    "inspect the branch; push the approved commits back and approve again, or approve this head",
                );
                refusal.label = true;
                return refuse(refusal);
            }
            return Ok(Verdict::Promote(Box::new(Plan {
                upstream: upstream.clone(),
                base: settings.base.clone(),
                series,
                to_sign: vec![],
                recovered: Some(record.clone()),
                human_text,
            })));
        }
        let to_sign = match self.dco(upstream, &settings.base, cache)? {
            true => unsigned(&series, &trailer),
            false => vec![],
        };
        // Signing writes anew every commit from the first one signed, those
        // already signed included, each on one parent: any merge in the
        // series refuses, not only one that lacks the trailer.
        let merge = series.commits.iter().find(|c| c.parents.len() != 1);
        if let (false, Some(commit)) = (to_sign.is_empty(), merge) {
            return refuse(Refusal::new(
                REFUSED,
                format!(
                    "commit {} is a merge commit, which the sign-off cannot rewrite",
                    short(&commit.sha)
                ),
                "rebase the branch into a linear series and approve the new head",
            ));
        }
        for n in &to_sign {
            let commit = &series.commits[*n];
            let ours = commit.login.as_deref().is_some_and(|l| {
                l.eq_ignore_ascii_case(&config.bot) || l.eq_ignore_ascii_case(&config.operator)
            }) || commit.author.email.eq_ignore_ascii_case(&signoff.email);
            if !ours {
                return refuse(Refusal::new(
                    REFUSED,
                    format!(
                        "commit {} by {} <{}> lacks the operator's sign-off, which only the bot's and the operator's commits get",
                        short(&commit.sha),
                        commit.author.name,
                        commit.author.email
                    ),
                    "have its author sign it off, then approve the new head",
                ));
            }
        }
        Ok(Verdict::Promote(Box::new(Plan {
            upstream: upstream.clone(),
            base: settings.base.clone(),
            series,
            to_sign,
            recovered: None,
            human_text,
        })))
    }

    /// The stack `pull` sits on, from the newest plan of each issue it
    /// closes: none, or the verdict its lower slice's state gives.
    fn stacked(&self, pull: &Pull, upstream: &str) -> Result<Option<Verdict>> {
        let (owner, repo) = pull.fork.split_once('/').context("fork")?;
        let data = self.client.graphql(
            CLOSING_QUERY,
            json!({"owner": owner, "name": repo, "number": pull.number}),
        )?;
        let refs = data
            .pointer("/repository/pullRequest/closingIssuesReferences")
            .filter(|r| r.is_object())
            .with_context(|| format!("{} is not a pull request", pull.name()))?;
        let nodes = refs["nodes"]
            .as_array()
            .context("closing issues have no nodes")?;
        ensure!(
            refs["totalCount"].as_u64() == Some(nodes.len() as u64),
            "the issues {} closes could not all be read",
            pull.name()
        );
        let writers = self.writers();
        for node in nodes {
            let issue = issue_ref(node)?;
            let comments = issue_comments(&self.client, &issue)?;
            if no_plan(&comments, &writers) {
                continue;
            }
            let (plan, _) = match newest_plan(&comments, &writers) {
                Ok(found) => found,
                Err(error) => {
                    return Ok(Some(Verdict::Hold(format!(
                        "the newest plan on {} is invalid: {error:#}",
                        issue.display()
                    ))));
                }
            };
            let Some(lower) = plan.stacks_on else {
                continue;
            };
            let lower = ItemRef {
                repository: plan.repo.clone(),
                number: lower,
                ..issue.clone()
            };
            // A lower slice merged upstream lets this issue through; the
            // other issues' stacks must still be checked.
            if let Some(verdict) = self.lower_state(pull, &lower, upstream)? {
                return Ok(Some(verdict));
            }
        }
        Ok(None)
    }

    /// The state of the lower slice `lower`'s pull request in the fork: open
    /// (promote it first), closed with an open upstream pull request (wait
    /// for the merge), merged upstream (go ahead: `None`) or closed with
    /// nothing upstream (rebase by hand).
    fn lower_state(&self, pull: &Pull, lower: &ItemRef, upstream: &str) -> Result<Option<Verdict>> {
        let (owner, repo) = lower.repository.split_once('/').context("lower issue")?;
        let data = self.client.graphql(
            LOWER_QUERY,
            json!({"owner": owner, "name": repo, "number": lower.number}),
        )?;
        let refs = data
            .pointer("/repository/issue/closedByPullRequestsReferences")
            .filter(|r| r.is_object())
            .with_context(|| format!("{} is not an issue", lower.display()))?;
        let nodes = refs["nodes"]
            .as_array()
            .context("pull requests have no nodes")?;
        ensure!(
            refs["totalCount"].as_u64() == Some(nodes.len() as u64),
            "the pull requests of {} could not all be read",
            lower.display()
        );
        let bot = &self.config.bot;
        let parents: Vec<&Value> = nodes
            .iter()
            .filter(|p| {
                login(p, "/author/login").is_some_and(|l| l.eq_ignore_ascii_case(bot))
                    && text(p, "/repository/nameWithOwner").eq_ignore_ascii_case(&pull.fork)
            })
            .collect();
        let held = |why: String, next: &str| {
            Ok(Some(Verdict::Refuse(Box::new(Refusal::new(
                HELD, why, next,
            )))))
        };
        let stacks = format!("it stacks on {}", lower.display());
        if let Some(open) = parents.iter().find(|p| p["state"] == "OPEN") {
            return held(
                format!(
                    "{stacks}, whose pull request {} is still open",
                    text(open, "/url")
                ),
                "promote the lower pull request first",
            );
        }
        let Some(parent) = parents.last() else {
            return Ok(Some(Verdict::Hold(format!(
                "{stacks}, which has no pull request of {bot} in {}",
                pull.fork
            ))));
        };
        let branch = text(parent, "/headRefName");
        validate_ref(branch).context("the lower pull request's head branch")?;
        let head_owner = text(parent, "/headRepository/nameWithOwner")
            .split('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        ensure!(
            is_github_login(&head_owner),
            "the lower pull request's head repository has no owner"
        );
        let head_ref = format!("{head_owner}:{branch}");
        let url = format!("{API_ROOT}repos/{upstream}/pulls?state=all&head={head_ref}&per_page=10");
        let listed = self.list(
            &url,
            1,
            &format!("{upstream}'s pull requests from {head_ref}"),
        )?;
        let parent_url = text(parent, "/url");
        // Merged upstream: the stack is done with, and the behind check
        // below says whether this branch has caught up with that merge.
        if listed.iter().any(|p| p["merged_at"].is_string()) {
            return Ok(None);
        }
        if let Some(open) = listed.iter().find(|p| p["state"] == "open") {
            return held(
                format!(
                    "{stacks}, whose upstream pull request {} is not merged yet",
                    text(open, "/html_url")
                ),
                "wait for the upstream merge; the next pass tries again",
            );
        }
        held(
            format!(
                "{stacks}, whose pull request {parent_url} was closed with nothing merged upstream"
            ),
            "rebase this branch onto the upstream base by hand and approve the new head",
        )
    }

    /// Whether the upstream is archived, read once a pass.
    fn archived(&self, upstream: &str, cache: &mut Cache) -> Result<bool> {
        if !cache.upstreams.contains_key(upstream) {
            let repo = self.read(&format!("{API_ROOT}repos/{upstream}"), upstream)?;
            let archived = repo["archived"]
                .as_bool()
                .with_context(|| format!("{upstream} does not say whether it is archived"))?;
            cache.upstreams.insert(
                upstream.into(),
                Upstream {
                    archived,
                    ..Upstream::default()
                },
            );
        }
        Ok(cache.upstreams.get(upstream).is_some_and(|u| u.archived))
    }

    /// The upstream's contribution-policy record from the policy
    /// repository's default branch, checked once a pass: present, a file,
    /// parsed, never loosened but by the operator in a verified commit, and
    /// current against the upstream's own files. Never written.
    fn policy(
        &self,
        upstream: &str,
        promote: &Promote,
        cache: &mut Cache,
    ) -> Result<std::result::Result<Policy, Refusal>> {
        if let Some(known) = cache
            .upstreams
            .get(upstream)
            .and_then(|u| u.policy.as_ref())
        {
            return Ok(known.clone());
        }
        let repository = promote
            .policy_repository
            .as_deref()
            .context("no policy repository")?;
        if cache.policy_branch.is_none() {
            let repo = self.read(&format!("{API_ROOT}repos/{repository}"), repository)?;
            let branch = text(&repo, "/default_branch");
            validate_ref(branch).with_context(|| format!("{repository}'s default branch"))?;
            cache.policy_branch = Some(branch.into());
        }
        let branch = cache.policy_branch.clone().unwrap_or_default();
        let gate = self.policy_gate(upstream, (repository, &branch, &promote.policy_path))?;
        if let Some(facts) = cache.upstreams.get_mut(upstream) {
            facts.policy = Some(gate.clone());
        }
        Ok(gate)
    }

    /// [`Self::policy`]'s reads: the record `path/UPSTREAM.md` of
    /// `repository` on `branch`, or why it holds.
    fn policy_gate(
        &self,
        upstream: &str,
        (repository, branch, dir): (&str, &str, &str),
    ) -> Result<std::result::Result<Policy, Refusal>> {
        let path = format!("{dir}/{upstream}.md");
        let at = format!("{repository}'s {path}");
        let next = "the policy-check agent writes the record; the next pass tries again";
        let held = |why: String| Refusal::new(HELD, why, next);
        let url = format!("{API_ROOT}repos/{repository}/contents/{path}?ref={branch}");
        let Some(file) = self.maybe(&url, &at)? else {
            return Ok(Err(held(format!(
                "no contribution-policy record for {upstream} at {at} on {branch}"
            ))));
        };
        if file["type"] != "file" {
            return Ok(Err(held(format!("{at} is not a regular file"))));
        }
        let content = base64(text(&file, "/content")).context("record content")?;
        let content = String::from_utf8(content).context("record is not UTF-8")?;
        let policy = match Policy::parse(&content) {
            Ok(policy) => policy,
            Err(error) => return Ok(Err(held(format!("{at} is invalid: {error:#}")))),
        };
        if let Some(why) = self.loosened(repository, &path, branch, &policy)? {
            return Ok(Err(held(why)));
        }
        if let Some(why) = self.stale(upstream, &policy)? {
            return Ok(Err(held(format!("{at} is stale: {why}"))));
        }
        Ok(Ok(policy))
    }

    /// Why the record's history is not trusted: a verdict got looser in a
    /// commit that is not the operator's and verified by GitHub, or the
    /// history is longer than this stage reads.
    fn loosened(
        &self,
        repository: &str,
        path: &str,
        branch: &str,
        now: &Policy,
    ) -> Result<Option<String>> {
        let url =
            format!("{API_ROOT}repos/{repository}/commits?path={path}&sha={branch}&per_page=100");
        let commits = self.list(&url, 1, &format!("{repository}'s history of {path}"))?;
        if commits.len() > POLICY_HISTORY_LIMIT {
            return Ok(Some(format!(
                "{path} has {} commits of history, more than the {POLICY_HISTORY_LIMIT} this stage checks",
                commits.len()
            )));
        }
        ensure!(!commits.is_empty(), "{path} has no history on {branch}");
        // Oldest first; the newest is the record read already.
        let mut ranks: Vec<(String, Option<String>, bool, usize)> = Vec::new();
        for (n, commit) in commits.iter().rev().enumerate() {
            let sha = text(commit, "/sha").to_owned();
            ensure!(is_sha(&sha), "a commit of {path}'s history has no sha");
            // The committer, whose signature GitHub verifies: an author's
            // name and email are whatever the commit says.
            let author = login(commit, "/committer/login").map(str::to_owned);
            let verified = commit
                .pointer("/commit/verification/verified")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let rank = match n + 1 == commits.len() {
                true => now.rank(),
                false => {
                    let url = format!("{API_ROOT}repos/{repository}/contents/{path}?ref={sha}");
                    match self.maybe(&url, &format!("{path} at {}", short(&sha)))? {
                        // Deleted or unreadable then: as strict as can be.
                        None => VERDICTS.len(),
                        Some(file) => {
                            let content =
                                base64(text(&file, "/content")).context("record content")?;
                            String::from_utf8(content)
                                .ok()
                                .and_then(|c| Policy::parse(&c).ok())
                                .map_or(VERDICTS.len(), |p| p.rank())
                        }
                    }
                }
            };
            ranks.push((sha, author, verified, rank));
        }
        let operator = &self.config.operator;
        for pair in ranks.windows(2) {
            let (_, _, _, before) = &pair[0];
            let (sha, author, verified, after) = &pair[1];
            if after < before {
                let by_operator = author
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(operator));
                if !(by_operator && *verified) {
                    return Ok(Some(format!(
                        "{path}'s verdict was loosened from {} to {} in {} committed by {}, not by {operator} in a commit GitHub verified",
                        VERDICTS.get(*before).unwrap_or(&"nothing"),
                        VERDICTS.get(*after).unwrap_or(&"nothing"),
                        short(sha),
                        author.as_deref().unwrap_or("an unknown account")
                    )));
                }
            }
        }
        Ok(None)
    }

    /// Why the record is stale: a source whose file changed on its
    /// default branch, or a contribution file the record does not list in
    /// [`POLICY_DIRS`] of the upstream or its owner's `.github` repository.
    fn stale(&self, upstream: &str, policy: &Policy) -> Result<Option<String>> {
        for (repo, path, sha) in &policy.sources {
            validate_repository(repo).context("a source's repo")?;
            validate_ref(path).context("a source's path")?;
            let url = format!("{API_ROOT}repos/{repo}/contents/{path}");
            let Some(file) = self.maybe(&url, &format!("{repo}'s {path}"))? else {
                return Ok(Some(format!("{repo}'s {path} is gone")));
            };
            if file.is_array() {
                return Ok(Some(format!(
                    "{repo}'s {path} is a directory; only files are checked"
                )));
            }
            if text(&file, "/sha") != sha {
                return Ok(Some(format!(
                    "{repo}'s {path} changed from {} to {}",
                    short(sha),
                    short(text(&file, "/sha"))
                )));
            }
        }
        let owner = upstream.split('/').next().unwrap_or_default();
        let dotgithub = format!("{owner}/.github");
        let mut repos = vec![upstream.to_owned()];
        if self
            .maybe(&format!("{API_ROOT}repos/{dotgithub}"), &dotgithub)?
            .is_some()
        {
            repos.push(dotgithub);
        }
        for repo in &repos {
            for dir in POLICY_DIRS {
                let url = format!("{API_ROOT}repos/{repo}/contents/{dir}");
                let Some(listing) = self.maybe(&url, &format!("{repo}'s {dir}/"))? else {
                    continue;
                };
                let Some(entries) = listing.as_array() else {
                    continue;
                };
                for entry in entries {
                    let name = text(entry, "/name");
                    let path = text(entry, "/path");
                    if entry["type"] == "file" && policy_file(name) {
                        let listed = policy
                            .sources
                            .iter()
                            .any(|(r, p, _)| r.eq_ignore_ascii_case(repo) && p == path);
                        if !listed {
                            return Ok(Some(format!("{repo}'s {path} is not in the record")));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    /// Why a `human-text` upstream does not get `ready`: the approving
    /// review lacks the [`HUMAN_TEXT`] line, or GitHub does not show the
    /// operator as the one who pushed the head, last edited the body and
    /// last renamed the title, or the body still ends with the bot's line.
    fn human_text(&self, ready: &Ready, upstream: &str) -> Result<Option<Refusal>> {
        let operator = &self.config.operator;
        let pull = &ready.pull;
        let mut refusal = Refusal::new(
            NEEDS_TEXT,
            String::new(),
            format!(
                "write the title and description yourself, push the head, take the bot's `{}` line out, and approve with a line `{HUMAN_TEXT}` in the review",
                FOOTER.split(':').next().unwrap_or_default()
            ),
        );
        refusal.label = true;
        let mut problems = Vec::new();
        if !ready.approval.human_text() {
            problems.push(format!("the approval does not say `{HUMAN_TEXT}`"));
        }
        if pull.body.lines().any(|l| l.trim() == FOOTER) {
            problems.push("the description still has the bot's Generated-by line".to_owned());
        }
        let url = format!(
            "{API_ROOT}repos/{}/activity?ref=refs/heads/{}&per_page=100",
            pull.head_repo, pull.branch
        );
        let pushes = self.list(&url, 1, &format!("{}'s pushes", pull.head_repo))?;
        let pusher = |sha: &str| {
            pushes
                .iter()
                .find(|p| text(p, "/after") == sha)
                .and_then(|p| login(p, "/actor/login"))
        };
        // After the bot's own sign-off push the operator's push is the
        // approved head's, and the head is the bot's; the series is proved
        // against the approved one before anything is written.
        let (approved, what) = match &ready.recovered {
            Some(record) => (
                record.approved.as_str(),
                format!("the approved head {}", short(&record.approved)),
            ),
            None => (pull.head.as_str(), "the head".to_owned()),
        };
        if !pusher(approved).is_some_and(|p| p.eq_ignore_ascii_case(operator)) {
            problems.push(format!(
                "GitHub shows {} pushed {what}, not {operator}",
                pusher(approved).unwrap_or("nobody")
            ));
        }
        if ready.recovered.is_some() {
            let bot = &self.config.bot;
            if !pusher(&pull.head).is_some_and(|p| p.eq_ignore_ascii_case(bot)) {
                problems.push(format!(
                    "GitHub shows {} pushed the signed head {}, not {bot}",
                    pusher(&pull.head).unwrap_or("nobody"),
                    short(&pull.head)
                ));
            }
        }
        let text = self.text(pull)?;
        let theirs = |who: &Option<String>| {
            who.as_deref()
                .is_some_and(|w| w.eq_ignore_ascii_case(operator))
        };
        if text.edited.is_none() || !theirs(&text.editor) {
            problems.push(format!(
                "the description was last edited by {}, not {operator}",
                text.editor.as_deref().unwrap_or("nobody")
            ));
        }
        if !theirs(&text.renamer) {
            problems.push(format!(
                "the title was last set by {}, not {operator}",
                text.renamer.as_deref().unwrap_or("the bot")
            ));
        }
        if problems.is_empty() {
            return Ok(None);
        }
        refusal.why = format!(
            "{upstream}'s contribution policy wants a person's text, and {}",
            problems.join("; ")
        );
        Ok(Some(refusal))
    }

    /// Who last changed `pull`'s text, and when ([`Text`]).
    fn text(&self, pull: &Pull) -> Result<Text> {
        let (owner, repo) = pull.fork.split_once('/').context("fork")?;
        let data = self.client.graphql(
            TEXT_QUERY,
            json!({"owner": owner, "name": repo, "number": pull.number}),
        )?;
        let node = data
            .pointer("/repository/pullRequest")
            .filter(|n| n.is_object())
            .with_context(|| format!("{} is not a pull request", pull.name()))?;
        let at = |value: &Value| -> Result<Option<OffsetDateTime>> {
            match value.as_str() {
                None => Ok(None),
                Some(at) => OffsetDateTime::parse(at, &Rfc3339)
                    .map(Some)
                    .with_context(|| format!("{}'s text has no readable time", pull.name())),
            }
        };
        let rename = &node["timelineItems"]["nodes"][0];
        Ok(Text {
            edited: at(&node["lastEditedAt"])?,
            editor: login(node, "/editor/login").map(str::to_owned),
            renamed: at(&rename["createdAt"])?,
            renamer: login(rename, "/actor/login").map(str::to_owned),
        })
    }

    /// Whether the upstream wants DCO sign-offs, from GitHub and never from
    /// its text: a required status check named like `dco` on the base
    /// branch, a `dco` check run on the base head, or one on the head of
    /// one of its last [`DCO_PULLS`] pull requests. Read once a pass.
    fn dco(&self, upstream: &str, base: &str, cache: &mut Cache) -> Result<bool> {
        if let Some(known) = cache.upstreams.get(upstream).and_then(|u| u.dco) {
            return Ok(known);
        }
        let wanted = self.dco_wanted(upstream, base)?;
        if let Some(facts) = cache.upstreams.get_mut(upstream) {
            facts.dco = Some(wanted);
        }
        Ok(wanted)
    }

    /// [`Self::dco`]'s reads.
    fn dco_wanted(&self, upstream: &str, base: &str) -> Result<bool> {
        let dco = |name: &str| name.to_ascii_lowercase().contains("dco");
        let runs = |value: &Value| {
            value["check_runs"].as_array().is_some_and(|runs| {
                runs.iter()
                    .any(|r| dco(text(r, "/name")) || dco(text(r, "/app/slug")))
            })
        };
        let url = format!("{API_ROOT}repos/{upstream}/rules/branches/{base}");
        if let Some(rules) = self.maybe(&url, &format!("{upstream}'s rules for {base}"))? {
            let required = rules.as_array().into_iter().flatten().any(|rule| {
                rule["type"] == "required_status_checks"
                    && rule
                        .pointer("/parameters/required_status_checks")
                        .and_then(Value::as_array)
                        .is_some_and(|checks| checks.iter().any(|c| dco(text(c, "/context"))))
            });
            if required {
                return Ok(true);
            }
        }
        let url = format!("{API_ROOT}repos/{upstream}/commits/{base}/check-runs?per_page=100");
        if runs(&self.read(&url, &format!("{upstream}'s checks on {base}"))?) {
            return Ok(true);
        }
        // A sample, deliberately one page: its next-page link is not a list
        // read in part, as it is to `list`.
        let url = format!(
            "{API_ROOT}repos/{upstream}/pulls?state=all&sort=created&direction=desc&per_page={DCO_PULLS}"
        );
        let what = format!("{upstream}'s last pull requests");
        let sample = self.read(&url, &what)?;
        let sample = sample
            .as_array()
            .with_context(|| format!("{what} are not a list"))?;
        for pull in sample {
            let sha = text(pull, "/head/sha");
            if !is_sha(sha) {
                continue;
            }
            let url = format!("{API_ROOT}repos/{upstream}/commits/{sha}/check-runs?per_page=100");
            if runs(&self.read(&url, &format!("{upstream}'s checks on {}", short(sha)))?) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // ---- Writes ----

    /// The words of a promotion's line.
    fn words(&self, plan: &Plan) -> String {
        match (&plan.recovered, plan.to_sign.len()) {
            (Some(_), _) => "sign-off recovered from its record".into(),
            (None, 0) => "no sign-off needed".into(),
            (None, n) => format!("sign-off on {n} of {} commits", plan.series.commits.len()),
        }
    }

    /// Opens the upstream pull request for `ready` by `plan`: in a preview,
    /// its line; applying, the sign-off commits, their record and the ref
    /// update where the upstream wants DCO, then the pull request, the
    /// receipt on the fork pull request and its close, as one step of
    /// writes taken whole or held whole.
    fn take(&mut self, ready: &Ready, plan: &Plan) {
        let pull = &ready.pull;
        let name = pull.name();
        let words = self.words(plan);
        let line = format!(
            "promote {name} -> {}: would open a PR ({words})",
            plan.upstream
        );
        let writes = 3 + usize::from(!plan.to_sign.is_empty()) * 2;
        if let Some(held) = self.caps.check_many(Kind::Write, writes) {
            self.line(format!("{held}: {line}"));
            return;
        }
        if !self.apply {
            self.caps.count_many(Kind::Write, writes);
            self.line(line);
            return;
        }
        let bounds = self.client.bounds;
        // Every commit from the first one signed is written anew, two
        // requests each with its read of the ref.
        let rewrites = plan
            .to_sign
            .first()
            .map_or(0, |first| plan.series.commits.len() - first);
        let needed = PROMOTE_REQUESTS + 2 * rewrites + WRITE_RESERVE;
        if bounds.left.get() < needed || Instant::now() > bounds.admit_by {
            self.report.failures += 1;
            self.line(format!(
                "hold promote {name}: the pass has {} requests left, fewer than a promotion may take ({needed}), or ran out of time",
                bounds.left.get()
            ));
            return;
        }
        bounds.finishing.set(true);
        let result = self.open(ready, plan);
        bounds.finishing.set(false);
        match result {
            Ok((url, late)) => {
                self.caps.count_many(Kind::Write, writes);
                self.line(format!(
                    "promote {name} -> {}: opened {url} ({words})",
                    plan.upstream
                ));
                if let Some(error) = late {
                    self.report.failures += 1;
                    self.line(format!(
                        "promote error: {name}: after opening {url}: {error:#}; the next pass links and closes it"
                    ));
                }
            }
            // What a failed step wrote is on the fork pull request; it all
            // counts against the cap, as an epic apply's does.
            Err(Opened::Refused(refusal)) => {
                self.caps.count_many(Kind::Write, writes);
                self.report.failures += 1;
                self.refuse(ready, &refusal);
            }
            Err(Opened::Error(error)) => {
                self.caps.count_many(Kind::Write, writes);
                self.report.failures += 1;
                self.line(format!("promote error: {name}: {error:#}"));
            }
        }
    }

    /// The writes of a promotion, in order. The upstream pull request's
    /// URL and any error after it was opened, which the next pass mends.
    fn open(
        &self,
        ready: &Ready,
        plan: &Plan,
    ) -> std::result::Result<(String, Option<anyhow::Error>), Opened> {
        let config = self.config;
        let pull = &ready.pull;
        let promote = config
            .promote
            .as_ref()
            .ok_or_else(|| Opened::error("no promote"))?;
        let signoff = promote
            .signoff
            .as_ref()
            .ok_or_else(|| Opened::error("no signoff"))?;
        // The head and the ref, read again just before anything is written.
        let url = format!("{API_ROOT}repos/{}/pulls/{}", pull.fork, pull.number);
        let fresh = self.read(&url, &pull.name())?;
        let moved = |what: &str, at: &str| {
            Opened::Error(anyhow::anyhow!(
                "{what} is at {}, not the approved {}: it moved while promoting; nothing is opened",
                short(at),
                short(&pull.head)
            ))
        };
        if fresh["state"] != "open" || fresh["draft"] == true {
            return Err(Opened::error(
                "the pull request is no longer open and ready",
            ));
        }
        if text(&fresh, "/head/sha") != pull.head {
            return Err(moved("the pull request's head", text(&fresh, "/head/sha")));
        }
        // The text checked is the listing's, and who edited it was read
        // after the listing: unchanged now, it is the text that goes
        // upstream, and any edit since the listing showed in that read.
        if text(&fresh, "/title") != pull.title
            || fresh["body"].as_str().unwrap_or_default() != pull.body
        {
            return Err(Opened::error(
                "its title or description changed while promoting; nothing is opened, and the next pass checks the text as it is",
            ));
        }
        let at = self.ref_sha(&pull.head_repo, &pull.branch)?;
        if at != pull.head {
            return Err(moved("the branch", &at));
        }
        let mut head = pull.head.clone();
        let mut body = pull.body.trim_end().to_owned();
        let trailer = format!("Signed-off-by: {} <{}>", signoff.name, signoff.email);
        if !plan.to_sign.is_empty() {
            head = self.sign(ready, plan, signoff, &trailer)?;
        }
        // Where a sign-off was added, now or by the recovered push, the
        // body says so before its last line; not where the operator wrote
        // the text, which gets nothing of the bot's.
        let signed = plan.recovered.is_some() || head != pull.head;
        if signed && !plan.human_text {
            let note = format!(
                "`{}` was added to each commit on {}'s approval ({}).",
                trailer.split(" <").next().unwrap_or(&trailer),
                config.operator,
                ready.approval.url
            );
            body = match body.strip_suffix(FOOTER) {
                Some(rest) => format!("{}\n\n{note}\n\n{FOOTER}", rest.trim_end()),
                None => format!("{body}\n\n{note}"),
            };
        }
        // The ref once more, immediately before the pull request; the
        // answer's head is checked too, as a push can land in between.
        let at = self.ref_sha(&pull.head_repo, &pull.branch)?;
        if at != head {
            return Err(Opened::Error(anyhow::anyhow!(
                "the branch is at {}, not {}: it moved while promoting; nothing is opened",
                short(&at),
                short(&head)
            )));
        }
        let url = format!("{API_ROOT}repos/{}/pulls", plan.upstream);
        let request = json!({
            "title": pull.title,
            "body": body,
            "head": pull.head_ref(),
            "base": plan.base,
            "draft": false,
            "maintainer_can_modify": true,
        });
        let (status, answer) = self
            .client
            .send("POST", &url, &request)
            .map_err(|e| Opened::Error(e.context("opening the upstream pull request")))?;
        if status != 201 {
            let message = answer["message"].as_str().unwrap_or("no message");
            return Err(Opened::Refused(Box::new(Refusal::new(
                REFUSED,
                format!(
                    "{} refused the pull request: HTTP status {status}, {message}",
                    plan.upstream
                ),
                "see whether the bot may open pull requests there; a new approval tries again",
            ))));
        }
        let opened = text(&answer, "/html_url").to_owned();
        if opened.is_empty() {
            return Err(Opened::error(
                "the upstream pull request was opened but has no URL",
            ));
        }
        // The request names the head by its mutable `OWNER:BRANCH`, so a
        // push after the last ref read can still be what GitHub opened:
        // the answer's head must be the checked one, or the pull request
        // is closed again and neither receipt nor close follows. A push
        // after GitHub opened it still moves it, as any pull request from
        // a branch; this checks what was published.
        let opened_head = text(&answer, "/head/sha");
        if opened_head != head {
            let number = answer["number"].as_u64().unwrap_or_default();
            let url = format!("{API_ROOT}repos/{}/pulls/{number}", plan.upstream);
            let closed = match self.client.send("PATCH", &url, &json!({"state": "closed"})) {
                Ok((200, _)) => Ok(()),
                Ok((status, _)) => Err(anyhow::anyhow!("GitHub HTTP status {status}")),
                Err(error) => Err(error),
            };
            let why = format!(
                "{opened} opened with head {}, not the approved {}: the branch moved as it was opened",
                short(opened_head),
                short(&head)
            );
            if let Err(error) = closed {
                return Err(Opened::Error(
                    error.context(format!("{why}; closing it again failed")),
                ));
            }
            let mut refusal = Refusal::new(
                REFUSED,
                format!("{why}; it was closed again"),
                "see who pushed to the branch; the closed pull request is not reopened by this stage, so reopen it yourself once its head is right, or promote from a new branch",
            );
            refusal.label = true;
            return Err(Opened::Refused(Box::new(refusal)));
        }
        let late = self.close(ready, &plan.upstream, &opened, &head).err();
        Ok((opened, late))
    }

    /// Adds the operator's sign-off to the commits of `plan` that lack it
    /// over the git-data API: new commits on the same trees, the record on
    /// the fork pull request, then the ref, read before and after. The new
    /// head.
    fn sign(
        &self,
        ready: &Ready,
        plan: &Plan,
        signoff: &Signoff,
        trailer: &str,
    ) -> std::result::Result<String, Opened> {
        let pull = &ready.pull;
        let series = &plan.series;
        let committer = Person {
            name: signoff.name.clone(),
            email: signoff.email.clone(),
            date: self
                .now
                .replace_nanosecond(0)
                .ok()
                .and_then(|t| t.format(&Rfc3339).ok())
                .unwrap_or_default(),
        };
        let mut after = Series {
            merge_base: series.merge_base.clone(),
            commits: Vec::new(),
        };
        let mut parent = series.merge_base.clone();
        for (n, commit) in series.commits.iter().enumerate() {
            let next = match plan.to_sign.contains(&n) {
                false if commit.parents == [parent.clone()] => commit.clone(),
                // A commit kept as it is must still follow the rewritten
                // parent: past the first rewrite every commit is new, with
                // its own committer kept.
                false => self.commit(
                    &pull.head_repo,
                    commit,
                    &parent,
                    &commit.message,
                    &commit.committer,
                )?,
                true => {
                    let message = with_trailer(&commit.message, trailer);
                    self.commit(&pull.head_repo, commit, &parent, &message, &committer)?
                }
            };
            parent = next.sha.clone();
            after.commits.push(next);
        }
        prove(series, &after, trailer).map_err(|e| {
            Opened::Error(e.context("the signed series does not prove; nothing is pushed"))
        })?;
        let signed = after
            .commits
            .last()
            .map(|c| c.sha.clone())
            .unwrap_or_default();
        // The record before the ref moves: a pass cut short between them
        // leaves a head the next pass can prove.
        let record = json!({
            "schema": RECEIPT_SCHEMA, "kind": "signoff",
            "approval": ready.approval.id, "approved": pull.head, "signed": signed,
        });
        let body = format!(
            "Adding {}'s sign-off to {} of {} commits on their approval {}: the approved head {} becomes {}.\n\n```{PROMOTE_MARKER}\n{}\n```\n\n{FOOTER}\n",
            self.config.operator,
            plan.to_sign.len(),
            series.commits.len(),
            ready.approval.url,
            short(&pull.head),
            short(&signed),
            serde_json::to_string_pretty(&record).unwrap_or_default()
        );
        let url = format!(
            "{API_ROOT}repos/{}/issues/{}/comments",
            pull.fork, pull.number
        );
        let (status, _) = self
            .client
            .send("POST", &url, &json!({"body": body}))
            .map_err(|e| Opened::Error(e.context("posting the sign-off record")))?;
        if status != 201 {
            return Err(Opened::error(&format!(
                "posting the sign-off record: GitHub HTTP status {status}"
            )));
        }
        // The update leases the approved head: a push by anyone else since
        // it was read makes GitHub reject it, and is kept. GitHub's answer
        // to a stale lease names no cause, so the ref is read to say which.
        let repo = self.read(
            &format!("{API_ROOT}repos/{}", pull.head_repo),
            &pull.head_repo,
        )?;
        let id = text(&repo, "/node_id");
        if id.is_empty() {
            return Err(Opened::error(&format!("{} has no node id", pull.head_repo)));
        }
        let update = json!({
            "repo": id, "name": format!("refs/heads/{}", pull.branch),
            "before": pull.head, "after": signed,
        });
        if let Err(error) = self.client.graphql(UPDATE_REF, update) {
            let at = self.ref_sha(&pull.head_repo, &pull.branch)?;
            if at != pull.head {
                return Err(Opened::Error(anyhow::anyhow!(
                    "the branch is at {}, not the approved {}: it moved while the commits were signed; nothing is pushed",
                    short(&at),
                    short(&pull.head)
                )));
            }
            return Err(Opened::Error(
                error.context("moving the branch to the signed head"),
            ));
        }
        let at = self.ref_sha(&pull.head_repo, &pull.branch)?;
        if at != signed {
            return Err(Opened::Error(anyhow::anyhow!(
                "the branch is at {} after the update, not the signed {}: someone else wrote to it",
                short(&at),
                short(&signed)
            )));
        }
        Ok(signed)
    }

    /// One commit written over the git-data API: `commit`'s tree and author
    /// on `parent` with `message`, committed by `committer`. The answer is
    /// what GitHub stored, which [`prove`] then checks as a whole.
    fn commit(
        &self,
        repo: &str,
        commit: &Commit,
        parent: &str,
        message: &str,
        committer: &Person,
    ) -> std::result::Result<Commit, Opened> {
        let request = json!({
            "message": message,
            "tree": commit.tree,
            "parents": [parent],
            "author": commit.author.json(),
            "committer": committer.json(),
        });
        let url = format!("{API_ROOT}repos/{repo}/git/commits");
        let (status, answer) = self.client.send("POST", &url, &request).map_err(|e| {
            Opened::Error(e.context(format!("writing commit {} anew", short(&commit.sha))))
        })?;
        if status != 201 {
            return Err(Opened::error(&format!(
                "writing commit {} anew: GitHub HTTP status {status}",
                short(&commit.sha)
            )));
        }
        let sha = text(&answer, "/sha");
        if !is_sha(sha) {
            return Err(Opened::error("the new commit has no sha"));
        }
        Commit::from_json(&answer, &answer).map_err(Opened::Error)
    }

    /// The upstream pull request `url` is open for `ready`'s head: the
    /// receipt on the fork pull request, then its close. Applying a
    /// [`Verdict::Done`], as one step of two writes.
    fn finish(&mut self, ready: &Ready, upstream: &str, url: &str) {
        let name = ready.pull.name();
        let line =
            format!("promote {name} -> {upstream}: {url} is open already; would link and close");
        if let Some(held) = self.caps.check_many(Kind::Write, 2) {
            self.line(format!("{held}: {line}"));
            return;
        }
        if !self.apply {
            self.caps.count_many(Kind::Write, 2);
            self.line(line);
            return;
        }
        // A failed step counts whole, as in `take`: its receipt may be
        // posted though the close failed.
        self.caps.count_many(Kind::Write, 2);
        match self.close(ready, upstream, url, &ready.pull.head) {
            Ok(()) => {
                self.line(format!(
                    "promote {name} -> {upstream}: {url} is open already; linked and closed"
                ));
            }
            Err(error) => {
                self.report.failures += 1;
                self.line(format!("promote error: {name}: {error:#}"));
            }
        }
    }

    /// The receipt on the fork pull request, readable first with the
    /// machine detail alongside, then its close.
    fn close(&self, ready: &Ready, upstream: &str, opened: &str, head: &str) -> Result<()> {
        let pull = &ready.pull;
        let receipt = json!({
            "schema": RECEIPT_SCHEMA, "kind": "promoted",
            "approval": ready.approval.id, "head": head, "upstream": opened,
        });
        let body = format!(
            "Opened upstream as {opened}, from {} at {}. Closing this review draft.\n\n```{PROMOTE_MARKER}\n{}\n```\n\n{FOOTER}\n",
            pull.head_ref(),
            short(head),
            serde_json::to_string_pretty(&receipt).unwrap_or_default()
        );
        let url = format!(
            "{API_ROOT}repos/{}/issues/{}/comments",
            pull.fork, pull.number
        );
        let (status, _) = self
            .client
            .send("POST", &url, &json!({"body": body}))
            .with_context(|| format!("linking {} to {upstream}'s pull request", pull.name()))?;
        ensure!(
            status == 201,
            "linking {}: GitHub HTTP status {status}",
            pull.name()
        );
        let url = format!("{API_ROOT}repos/{}/pulls/{}", pull.fork, pull.number);
        let (status, _) = self
            .client
            .send("PATCH", &url, &json!({"state": "closed"}))
            .with_context(|| format!("closing {}", pull.name()))?;
        ensure!(
            status == 200,
            "closing {}: GitHub HTTP status {status}",
            pull.name()
        );
        Ok(())
    }

    /// A refusal's line and, for a reason that needs the operator, one
    /// notice on the fork pull request, said once per approval and head,
    /// with the `needs-human` label where the operator must act: one write
    /// step, re-reading the comments just before posting.
    fn refuse(&mut self, ready: &Ready, refusal: &Refusal) {
        let pull = &ready.pull;
        let name = pull.name();
        self.line(format!("hold promote {name}: {}", refusal.why));
        let key = notice_key(refusal.reason, ready.approval.id, &pull.head);
        if noticed(&ready.notes, &self.config.bot, &key).is_some() {
            return;
        }
        let line = format!("notice {name}: {}", refusal.reason);
        if let Some(held) = self.caps.check(Kind::Write) {
            self.line(format!("{held}: {line}"));
            return;
        }
        if !self.apply {
            self.caps.count(Kind::Write);
            return;
        }
        if let Err(error) = self.notify(pull, refusal, &key) {
            self.report.failures += 1;
            self.line(format!("promote error: {name}: {line}: {error:#}"));
        }
    }

    /// Posts `refusal`'s notice with `key`'s marker on `pull`, and its
    /// label, unless a pass posted it since the comments were read.
    fn notify(&self, pull: &Pull, refusal: &Refusal, key: &str) -> Result<()> {
        // Two overlapping passes: the second sees the first's notice.
        let notes = self.notes(pull)?;
        if noticed(&notes, &self.config.bot, key).is_some() {
            return Ok(());
        }
        let quote = refusal
            .quote
            .as_deref()
            .map(|q| format!("{q}\n"))
            .unwrap_or_default();
        let body = format!(
            "Not promoted: {}.\n\nNext: {}.\n\n{quote}{FOOTER}\n{}\n",
            refusal.why,
            refusal.next,
            marker(key)
        );
        let url = format!(
            "{API_ROOT}repos/{}/issues/{}/comments",
            pull.fork, pull.number
        );
        self.caps.count(Kind::Write);
        let (status, _) = self.client.send("POST", &url, &json!({"body": body}))?;
        ensure!(
            status == 201,
            "posting the notice: GitHub HTTP status {status}"
        );
        if refusal.label {
            let url = format!(
                "{API_ROOT}repos/{}/issues/{}/labels",
                pull.fork, pull.number
            );
            let (status, _) = self
                .client
                .send("POST", &url, &json!({"labels": [NEEDS_HUMAN]}))?;
            ensure!(
                status == 200,
                "labelling {NEEDS_HUMAN}: GitHub HTTP status {status}"
            );
        }
        Ok(())
    }
}

/// How a promotion's writes ended without an upstream pull request left
/// open.
enum Opened {
    /// The upstream would not take it, or opened another head: a notice.
    Refused(Box<Refusal>),
    Error(anyhow::Error),
}

impl Opened {
    fn error(message: &str) -> Self {
        Self::Error(anyhow::anyhow!("{message}"))
    }
}

impl From<anyhow::Error> for Opened {
    fn from(error: anyhow::Error) -> Self {
        Self::Error(error)
    }
}

#[cfg(test)]
mod tests;
