//! Agent-drafted reviews of others' pull requests, posted as the bot once the
//! operator approves (tracker#848). A draft is a `review-draft` block, in the
//! body or a comment of a tracker issue labelled `review-draft`, followed by
//! its `## Review` and `## Comments` sections. [`send`] checks the newest
//! draft against the pull request at its `head` and posts it once.
use crate::Footer;
use crate::batch::issue_node;
use crate::github::{API_ROOT, Transport, is_github_login};
use crate::plan::{
    Comment, code_fence, comment, comment_id, fence_spans, has_fence, issue_comments, viewer,
};
use crate::poll::{Rest, check_config, check_keys, next_link, option, validate_repository};
use anyhow::{Context, Result, bail, ensure};
use board_core::ItemRef;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub const DRAFT_FENCE: &str = "review-draft";
/// The label that makes an issue a draft: an operator's `/send` elsewhere,
/// such as on an issue discussing a quoted draft, sends nothing.
pub const DRAFT_LABEL: &str = "review-draft";
/// The first line of the bot's comment that claims, then records, the send.
pub const SENT_MARKER: &str = "<!-- agent-board-review-sent -->";
/// The first line of the bot's comment that says why a draft was not sent,
/// which revokes every approval given before it.
pub const REFUSED_MARKER: &str = "<!-- agent-board-review-refused -->";
pub const SEND_COMMAND: &str = "/send";
pub const EVENTS: &[&str] = &["COMMENT", "REQUEST_CHANGES", "APPROVE"];
/// GitHub lists at most 3000 files of a pull request, 100 a page.
const FILE_PAGES: usize = 30;

const ISSUE_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      url body createdAt lastEditedAt author { login } editor { login } state
      labels(first: 100) { nodes { name } }
    }
  }
}"#;

/// agent-board.json's `operator` and `bot`, and its `review` section, from
/// the caller's default branch: never from the draft or the dispatcher.
#[derive(Debug)]
pub struct Config {
    pub operator: String,
    pub bot: String,
    /// `review.targets`: the repositories a review may go to, `OWNER/*` or
    /// `OWNER/REPO`. The bot's token can review any public pull request.
    pub targets: Vec<String>,
    /// `review.checkbox`: the text of the body's task-list item whose ticking
    /// approves, such as `Send the newest draft as example-bot`.
    pub checkbox: String,
    pub footer: Footer,
}

impl Config {
    pub fn from_json(value: &Value) -> Result<Self> {
        check_config(value)?;
        let login = |key: &str| -> Result<String> {
            value
                .get(key)
                .and_then(Value::as_str)
                .filter(|l| is_github_login(l))
                .map(str::to_owned)
                .with_context(|| format!("agent-board config has no valid {key}"))
        };
        let (operator, bot) = (login("operator")?, login("bot")?);
        // The bot posts the drafting run's text: were it the operator too, an
        // agent's `/send` in its own draft comment would approve the draft.
        ensure!(
            !operator.eq_ignore_ascii_case(&bot),
            "review needs an operator other than the bot"
        );
        let review = value.get("review").context("review is not configured")?;
        check_keys(&[&["targets", "checkbox"]], review, "review")?;
        let targets: Vec<String> = serde_json::from_value(
            review
                .get("targets")
                .cloned()
                .context("review has no targets")?,
        )
        .context("review targets is not a list of strings")?;
        ensure!(!targets.is_empty(), "review targets is empty");
        for target in &targets {
            match target.strip_suffix("/*") {
                Some(owner) => ensure!(
                    is_github_login(owner),
                    "review target {target:?} has an invalid owner"
                ),
                None => validate_repository(target).with_context(|| {
                    format!("review target {target:?} is not OWNER/* or OWNER/REPO")
                })?,
            }
        }
        let checkbox = review
            .get("checkbox")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty() && t.trim() == *t && !t.chars().any(char::is_control))
            .context("review checkbox is not one trimmed line")?
            .to_owned();
        Ok(Self {
            operator,
            bot,
            targets,
            checkbox,
            footer: Footer::from_json(option(value, "generatedBy")?)?,
        })
    }
}

/// What the operator's approval is.
#[derive(Clone, Debug, PartialEq)]
pub enum Approval {
    /// A comment of theirs that is just `/send`, after the newest draft.
    Command,
    /// Their edit that ticked the body's checkbox.
    Edit(Ticked),
}

/// The operator's edit that ticked the body's checkbox, from the
/// `issues: edited` event that triggered the caller's job. The job may run
/// long after the event, so the send is bound to that edit, not to whatever
/// the body is when it runs: a draft or refusal after `at`, or any later edit
/// of the body, even the operator's own, needs a new tick.
#[derive(Clone, Debug, PartialEq)]
pub struct Ticked {
    /// The issue's `updated_at` in the event: no earlier than the edit.
    pub at: OffsetDateTime,
    /// The body the edit left, which the body must still be.
    pub body: String,
}

impl Ticked {
    /// Read the GitHub `issues` webhook payload of `issue`, which must be the
    /// operator's edit taking the checkbox from unticked to ticked.
    pub fn from_event(event: &Value, issue: &ItemRef, config: &Config) -> Result<Self> {
        let text = |pointer: &str| {
            event
                .pointer(pointer)
                .and_then(Value::as_str)
                .with_context(|| format!("the event has no {pointer}"))
        };
        ensure!(
            text("/action")? == "edited",
            "the event is not an issue edit"
        );
        ensure!(
            text("/sender/login")?.eq_ignore_ascii_case(&config.operator),
            "the event's edit is not {}'s",
            config.operator
        );
        ensure!(
            text("/repository/full_name")?.eq_ignore_ascii_case(&issue.repository)
                && event.pointer("/issue/number").and_then(Value::as_u64) == Some(issue.number)
                && event.pointer("/issue/pull_request").is_none(),
            "the event is not of {}",
            issue.display()
        );
        let before = text("/changes/body/from").context("the event's edit kept the body")?;
        let body = text("/issue/body")?;
        ensure!(
            !is_ticked(before, &config.checkbox) && is_ticked(body, &config.checkbox),
            "the event's edit did not tick the checkbox"
        );
        Ok(Self {
            at: OffsetDateTime::parse(text("/issue/updated_at")?, &Rfc3339)
                .context("the event's updated_at")?,
            body: body.to_owned(),
        })
    }
}

/// One inline comment, on the new side of the pull request's diff.
#[derive(Debug, PartialEq)]
pub struct Inline {
    pub path: String,
    pub line: u64,
    pub body: String,
}

#[derive(Debug, PartialEq)]
pub struct Draft {
    pub repository: String,
    pub number: u64,
    pub head: String,
    pub event: String,
    pub body: String,
    pub comments: Vec<Inline>,
}

/// A draft or pull request the review cannot be sent for. Its text is ours
/// (with paths from the draft), never API output, so it may go in the bot's
/// comment on the issue.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// The body's checkbox line, ticked or not.
fn is_checkbox(line: &str, checkbox: &str) -> bool {
    ["- [ ] ", "- [x] ", "- [X] "]
        .iter()
        .any(|p| line.trim_end().strip_prefix(p) == Some(checkbox))
}

/// Whether the body's one checkbox line is ticked. A second line that is the
/// checkbox, such as a quoted example, or one in a fenced code block or an
/// HTML block or comment, which renders as an example or not at all, leaves
/// it unticked: which box the operator ticked would be ambiguous.
fn is_ticked(body: &str, checkbox: &str) -> bool {
    body.lines().filter(|l| is_checkbox(l, checkbox)).count() == 1
        && matches!(
            rendered_lines(body)
                .into_iter()
                .filter(|l| is_checkbox(l, checkbox))
                .collect::<Vec<_>>()
                .as_slice(),
            [line] if !line.starts_with("- [ ] ")
        )
}

/// The lines of `body` outside fenced code blocks, HTML blocks and HTML
/// comments. It errs towards dropping lines: an HTML block of a kind that
/// runs to a blank line is dropped even where CommonMark would read the
/// line as a paragraph's.
fn rendered_lines(body: &str) -> Vec<&str> {
    // CommonMark's HTML blocks that run to their end marker, not a blank line.
    const ENDS: &[(&str, &str)] = &[
        ("<!--", "-->"),
        ("<pre", "</pre>"),
        ("<script", "</script>"),
        ("<style", "</style>"),
        ("<textarea", "</textarea>"),
        ("<![cdata[", "]]>"),
        ("<?", "?>"),
        ("<!", ">"),
    ];
    let mut shown = Vec::new();
    let mut lines = body.lines();
    while let Some(line) = lines.next() {
        let start = line.trim_start_matches(' ');
        let html = (line.len() - start.len() <= 3 && start.starts_with('<'))
            .then(|| start.to_ascii_lowercase());
        if let Some((mark, length)) = code_fence(line) {
            // As in plan::fence_spans: unclosed, it runs to the end.
            for line in lines.by_ref() {
                if code_fence(line).is_some_and(|(m, l)| m == mark && l >= length)
                    && line.trim().trim_start_matches(mark).is_empty()
                {
                    break;
                }
            }
        } else if let Some(start) = html {
            match ENDS.iter().find(|(open, _)| start.starts_with(open)) {
                Some((open, end)) if !start[open.len()..].contains(end) => {
                    for line in lines.by_ref() {
                        if line.to_ascii_lowercase().contains(end) {
                            break;
                        }
                    }
                }
                Some(_) => {}
                None => {
                    for line in lines.by_ref() {
                        if line.trim().is_empty() {
                            break;
                        }
                    }
                }
            }
        } else if line
            .rsplit_once("<!--")
            .is_some_and(|(_, rest)| !rest.contains("-->"))
        {
            // Any comment left open hides the lines to its `-->`, wherever
            // on the line it opens.
            for line in lines.by_ref() {
                if line.contains("-->") {
                    break;
                }
            }
        } else {
            shown.push(line);
        }
    }
    shown
}

/// The header's `key = "value"` lines, a subset of TOML: no escapes, and a
/// `#` comment only after the value.
fn header(block: &str) -> Result<BTreeMap<&str, &str>> {
    let mut fields = BTreeMap::new();
    for line in block.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, rest) = line
            .split_once('=')
            .with_context(|| format!("review-draft line {line:?} is not KEY = \"VALUE\""))?;
        let key = key.trim();
        ensure!(
            ["pr", "head", "event"].contains(&key),
            "unknown review-draft field {key:?}"
        );
        let value = rest
            .trim()
            .strip_prefix('"')
            .and_then(|r| r.split_once('"'))
            .filter(|(value, after)| {
                let after = after.trim();
                !value.contains('\\') && (after.is_empty() || after.starts_with('#'))
            })
            .map(|(value, _)| value)
            .with_context(|| format!("review-draft {key} is not a quoted string"))?;
        ensure!(
            fields.insert(key, value).is_none(),
            "review-draft sets {key} twice"
        );
    }
    Ok(fields)
}

fn trimmed(lines: &[&str]) -> String {
    lines.join("\n").trim().to_owned()
}

/// The one `review-draft` block of `text` and its sections. Lines that are
/// the body's checkbox are not part of the review.
pub fn parse(text: &str, checkbox: &str) -> Result<Draft> {
    let spans = fence_spans(text, DRAFT_FENCE);
    let [(block, end)] = spans.as_slice() else {
        bail!(
            "the draft holds {} review-draft blocks, not one",
            spans.len()
        );
    };
    let fields = header(
        block
            .as_deref()
            .context("the review-draft block is not closed")?,
    )?;
    let field = |key: &str| {
        fields
            .get(key)
            .copied()
            .with_context(|| format!("review-draft has no {key}"))
    };
    let pr = field("pr")?;
    let (repository, number) = pr
        .split_once('#')
        .with_context(|| format!("review-draft pr {pr:?} is not OWNER/REPO#N"))?;
    validate_repository(repository)?;
    let number = number
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .with_context(|| format!("review-draft pr {pr:?} is not OWNER/REPO#N"))?;
    let head = field("head")?;
    ensure!(
        matches!(head.len(), 40 | 64)
            && head
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "review-draft head is not a full lowercase commit SHA"
    );
    let event = field("event")?;
    ensure!(
        EVENTS.contains(&event),
        "review-draft event {event:?} is not one of {}",
        EVENTS.join(", ")
    );

    let mut lines = text
        .lines()
        .skip(*end)
        .filter(|l| !is_checkbox(l, checkbox))
        .skip_while(|l| l.trim().is_empty());
    ensure!(
        lines.next().map(str::trim_end) == Some("## Review"),
        "the draft has no ## Review section after its block"
    );
    let mut body = Vec::new();
    let mut sections: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut in_comments = false;
    // Headings inside a fenced code block, such as a quoted Markdown file,
    // are text.
    let mut fence: Option<(char, usize)> = None;
    for line in lines {
        if let Some((mark, length)) = fence {
            if code_fence(line).is_some_and(|(m, l)| m == mark && l >= length)
                && line.trim().trim_start_matches(mark).is_empty()
            {
                fence = None;
            }
        } else if let Some(open) = code_fence(line) {
            fence = Some(open);
        } else if line.trim_end() == "## Comments" {
            ensure!(!in_comments, "the draft has two ## Comments sections");
            in_comments = true;
            continue;
        } else if let Some(heading) = line.strip_prefix("### ").filter(|_| in_comments) {
            sections.push((heading.trim(), Vec::new()));
            continue;
        }
        match sections.last_mut() {
            Some((_, text)) => text.push(line),
            None if in_comments => ensure!(
                line.trim().is_empty(),
                "the draft has text under ## Comments before its first ### heading"
            ),
            None => body.push(line),
        }
    }
    let comments = sections
        .into_iter()
        .map(|(heading, text)| {
            let place = heading
                .strip_prefix('`')
                .and_then(|h| h.strip_suffix('`'))
                .with_context(|| format!("comment heading {heading:?} is not `PATH:LINE`"))?;
            let (path, line) = place
                .rsplit_once(':')
                .filter(|(path, _)| !path.is_empty())
                .with_context(|| format!("comment heading {heading:?} is not `PATH:LINE`"))?;
            let line = line
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .with_context(|| format!("comment heading {heading:?} has no line number"))?;
            let body = trimmed(&text);
            ensure!(!body.is_empty(), "the comment on {place} is empty");
            Ok(Inline {
                path: path.into(),
                line,
                body,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Draft {
        repository: repository.into(),
        number,
        head: head.into(),
        event: event.into(),
        body: trimmed(&body),
        comments,
    })
}

/// The issue's body, as a comment; whether it is open and labelled a draft.
fn read_issue<T: Transport>(client: &T, issue: &ItemRef) -> Result<(Comment, bool, bool)> {
    let node = &issue_node(client, ISSUE_QUERY, issue)?;
    let labels = node
        .pointer("/labels/nodes")
        .and_then(Value::as_array)
        .context("issue has no labels")?;
    let labelled = labels.iter().any(|l| l["name"] == DRAFT_LABEL);
    let open = node["state"] == "OPEN";
    Ok((comment(node)?, open, labelled))
}

/// The issue's body, as entry 0, then its comments, in order; whether it is
/// open and labelled a draft.
fn read_thread<T: Transport>(client: &T, issue: &ItemRef) -> Result<(Vec<Comment>, bool, bool)> {
    let comments = issue_comments(client, issue)?;
    let (body, open, labelled) = read_issue(client, issue)?;
    let thread = std::iter::once(body).chain(comments).collect();
    Ok((thread, open, labelled))
}

fn by(c: &Comment, logins: &[&str]) -> bool {
    c.author
        .as_deref()
        .is_some_and(|a| logins.iter().any(|l| l.eq_ignore_ascii_case(a)))
}

/// Edited by someone outside `logins`, or by an account GitHub cannot name:
/// anyone with write access may edit others' comments and the body.
fn foreign_edit(c: &Comment, logins: &[&str]) -> bool {
    (c.edited || c.editor.is_some())
        && !c
            .editor
            .as_deref()
            .is_some_and(|e| logins.iter().any(|l| l.eq_ignore_ascii_case(e)))
}

/// The bot's claims and records of a send, by comment id.
fn claims(thread: &[Comment], config: &Config) -> Vec<u64> {
    thread
        .iter()
        .skip(1)
        .filter(|c| by(c, &[&config.bot]) && c.body.lines().next() == Some(SENT_MARKER))
        .filter_map(|c| comment_id(&c.url))
        .collect()
}

/// The entry of the newest draft: the last by position (the body first)
/// from the bot or the operator, by [`crate::plan::newest_fenced`]'s rules,
/// except that a ticked checkbox, which edits the body, cannot make the body's
/// draft newer than a comment's. One edited by anyone else is refused rather
/// than passed over, so an edit that removes a draft cannot revive the one
/// before it.
fn newest_draft(thread: &[Comment], config: &Config) -> Result<usize> {
    let writers = [config.bot.as_str(), config.operator.as_str()];
    let newest = thread
        .iter()
        .rposition(|c| {
            by(c, &writers) && (has_fence(&c.body, DRAFT_FENCE) || foreign_edit(c, &writers))
        })
        .context("no review draft on this issue from the bot or the operator")?;
    let draft = thread.get(newest).context("draft entry")?;
    ensure!(
        !foreign_edit(draft, &writers),
        "the newest draft {} was last edited by {}, who may not write drafts",
        draft.url,
        draft.editor.as_deref().unwrap_or("an unidentified account")
    );
    Ok(newest)
}

/// The operator's approval of the draft at `index`, given after its last
/// edit and after the bot's last refusal: an edit to the draft, or a refusal
/// (such as of a pull request closed then, which may since have reopened),
/// needs a new approval. The last refusal is the latest by time, not by
/// position: a refusal may edit a claim posted before a later `/send`.
fn approved(thread: &[Comment], index: usize, config: &Config, approval: &Approval) -> Result<()> {
    let operator = [config.operator.as_str()];
    let draft = thread.get(index).context("draft entry")?;
    let refusal = thread
        .iter()
        .filter(|c| by(c, &[&config.bot]) && c.body.lines().next() == Some(REFUSED_MARKER))
        .max_by_key(|c| c.changed);
    match approval {
        Approval::Command => ensure!(
            thread.iter().skip(index + 1).any(|c| by(c, &operator)
                && !foreign_edit(c, &operator)
                && c.changed > draft.changed
                && refusal.is_none_or(|r| c.changed > r.changed)
                // The whole comment, not a line of it: a line can sit in a
                // blockquote, list or HTML block that Markdown carries on
                // from the lines before, making it a quoted example.
                && c.body.trim() == SEND_COMMAND),
            "no {SEND_COMMAND} from {} after the newest draft's last edit and the last refusal",
            config.operator
        ),
        Approval::Edit(ticked) => {
            let body = thread.first().context("issue body")?;
            ensure!(
                body.editor
                    .as_deref()
                    .is_some_and(|e| e.eq_ignore_ascii_case(&config.operator))
                    && is_ticked(&body.body, &config.checkbox),
                "the body's last edit is not {}'s ticking its checkbox",
                config.operator
            );
            // A draft in the body is the one ticked when the body still is.
            ensure!(
                body.body == ticked.body,
                "the body changed after the edit that ticked its checkbox; untick and tick it again to approve"
            );
            ensure!(
                index == 0 || draft.changed < ticked.at,
                "the newest draft changed after the checkbox was ticked"
            );
            if let Some(refusal) = refusal {
                ensure!(
                    refusal.changed < ticked.at,
                    "the checkbox was ticked before the last refusal, {}",
                    refusal.url
                );
            }
        }
    }
    Ok(())
}

fn get<R: Rest>(client: &R, url: &str) -> Result<(Value, Option<String>)> {
    let response = client.get(url, None)?;
    ensure!(
        response.status == 200,
        "GET {}: GitHub HTTP status {}",
        url.trim_start_matches(API_ROOT),
        response.status
    );
    Ok((response.body, response.link))
}

/// The new-side lines of a file's patch: the lines a comment may be on.
fn new_lines(patch: &str) -> Result<BTreeSet<u64>> {
    let mut lines = BTreeSet::new();
    let mut next: Option<u64> = None;
    for line in patch.lines() {
        if line.starts_with("@@ ") {
            let start = line
                .split(' ')
                .find_map(|p| p.strip_prefix('+'))
                .and_then(|r| r.split(',').next())
                .and_then(|n| n.parse().ok())
                .with_context(|| format!("hunk header {line:?}"))?;
            next = Some(start);
        } else if let Some(n) = next.as_mut()
            && !line.starts_with('-')
            && !line.starts_with('\\')
        {
            lines.insert(*n);
            *n += 1;
        }
    }
    Ok(lines)
}

/// The pull request's files and their commentable lines, at its current head.
fn diff_lines<R: Rest>(client: &R, pull: &str) -> Result<BTreeMap<String, BTreeSet<u64>>> {
    let mut files = BTreeMap::new();
    let mut url = Some(format!("{pull}/files?per_page=100"));
    for _ in 0..FILE_PAGES {
        let Some(page) = url.take() else { break };
        let (body, link) = get(client, &page)?;
        for file in body
            .as_array()
            .context("pull request files is not a list")?
        {
            let name = file["filename"].as_str().context("file has no filename")?;
            // A binary or very large file has no patch, and so no lines.
            let lines = new_lines(file["patch"].as_str().unwrap_or_default())?;
            files.insert(name.to_owned(), lines);
        }
        url = link.as_deref().and_then(next_link);
    }
    ensure!(
        url.is_none(),
        "the pull request lists more than {} files",
        FILE_PAGES * 100
    );
    Ok(files)
}

fn pull_url(draft: &Draft) -> String {
    format!(
        "{API_ROOT}repos/{}/pulls/{}",
        draft.repository, draft.number
    )
}

/// Check that the draft's pull request is still in its repository, open and
/// at its head: a failure is a [`Refused`].
fn current<R: Rest>(client: &R, draft: &Draft) -> Result<()> {
    let pr = format!("{}#{}", draft.repository, draft.number);
    let (pull, _) = get(client, &pull_url(draft))?;
    // GitHub redirects a renamed or transferred repository's API paths, which
    // could take a review outside the targets.
    if !pull
        .pointer("/base/repo/full_name")
        .and_then(Value::as_str)
        .is_some_and(|n| n.eq_ignore_ascii_case(&draft.repository))
    {
        bail!(Refused(format!("{pr} is now in another repository")));
    }
    if pull["state"] != "open" {
        bail!(Refused(format!("{pr} is closed")));
    }
    let now = pull.pointer("/head/sha").and_then(Value::as_str);
    if now != Some(draft.head.as_str()) {
        bail!(Refused(format!(
            "the head of {pr} moved from {} to {}; the draft needs redrafting against it",
            draft.head,
            now.unwrap_or("an unknown commit")
        )));
    }
    Ok(())
}

/// Parse the draft and check it against its pull request: any failure is a
/// [`Refused`].
fn check<R: Rest>(client: &R, text: &str, config: &Config) -> Result<Draft> {
    let draft = parse(text, &config.checkbox).map_err(|e| Refused(format!("{e:#}")))?;
    let pr = format!("{}#{}", draft.repository, draft.number);
    if !crate::poll::comments::in_scope(&config.targets, &draft.repository) {
        bail!(Refused(format!("{pr} is outside review.targets")));
    }
    current(client, &draft)?;
    let files = diff_lines(client, &pull_url(&draft))?;
    for c in &draft.comments {
        if !files.get(&c.path).is_some_and(|l| l.contains(&c.line)) {
            bail!(Refused(format!(
                "{}:{} is not in the diff of {pr} at {}",
                c.path, c.line, draft.head
            )));
        }
    }
    Ok(draft)
}

/// The review request: `commit_id` pins the comments' lines to `head`.
fn review(draft: &Draft, config: &Config) -> Value {
    let lead = format!("{} approved sending this review.", config.operator);
    let body = if draft.body.is_empty() {
        lead
    } else {
        format!("{lead}\n\n{}", draft.body)
    };
    let comments: Vec<Value> = draft
        .comments
        .iter()
        .map(|c| json!({"path": c.path, "line": c.line, "side": "RIGHT", "body": c.body}))
        .collect();
    json!({"commit_id": draft.head, "event": draft.event, "body": body, "comments": comments})
}

/// Record a refusal: say why in a comment, or in place of the bot's `claim`
/// if it posted one, which revokes the approval. The body, and its box, are
/// left as they are: GitHub has no conditional issue update, so rewriting the
/// body could lose an edit made meanwhile, and a ticked box approves again
/// only by a new edit that ticks it.
fn refuse<C: Transport + Rest>(
    client: &C,
    issue: &ItemRef,
    config: &Config,
    reason: &str,
    claim: Option<u64>,
) -> Result<()> {
    let text = json!({"body": format!(
        "{REFUSED_MARKER}\nNot sent: {reason}.\n\n{}",
        config.footer
    )});
    match claim {
        Some(id) => crate::plan::send(
            client,
            "PATCH",
            &format!("{API_ROOT}repos/{}/issues/comments/{id}", issue.repository),
            &text,
            0,
        )?,
        None => crate::plan::send(
            client,
            "POST",
            &format!(
                "{API_ROOT}repos/{}/issues/{}/comments",
                issue.repository, issue.number
            ),
            &text,
            0,
        )?,
    };
    Ok(())
}

/// Send the newest approved draft on `issue` as one review, or with `apply`
/// false return the requests it would make. Prints nothing, writes nothing,
/// before the approval is found; a [`Refused`] draft then gets the reason in
/// a comment, with `apply` (see [`refuse`]).
///
/// Sent once: the bot claims the issue with a [`SENT_MARKER`] comment before
/// posting, and never posts on an issue with a claim, which after a failure
/// needs the operator to look before removing it. Of racing callers, the
/// earliest claim posts.
pub fn send<C: Transport + Rest>(
    client: &C,
    issue: &ItemRef,
    config: &Config,
    approval: &Approval,
    apply: bool,
) -> Result<Vec<Value>> {
    if apply {
        ensure!(
            viewer(client)?.eq_ignore_ascii_case(&config.bot),
            "review send --apply requires the configured bot's token"
        );
    }
    let (thread, open, labelled) = read_thread(client, issue)?;
    let shown = issue.display();
    ensure!(labelled, "{shown} is not labelled {DRAFT_LABEL}");
    ensure!(
        claims(&thread, config).is_empty(),
        "{shown} already records a sent review"
    );
    ensure!(open, "{shown} is closed");
    let index = newest_draft(&thread, config)?;
    approved(&thread, index, config, approval)?;
    let (body, from) = match (thread.first(), thread.get(index)) {
        (Some(body), Some(from)) => (body, from),
        _ => bail!("issue thread"),
    };
    let issue_url = format!(
        "{API_ROOT}repos/{}/issues/{}",
        issue.repository, issue.number
    );
    let comments_url = format!("{issue_url}/comments");
    let draft = match check(client, &from.body, config) {
        Ok(draft) => draft,
        Err(error) => {
            if apply && let Some(Refused(reason)) = error.downcast_ref::<Refused>() {
                refuse(client, issue, config, reason, None)?;
            }
            return Err(error);
        }
    };
    let request = review(&draft, config);
    let reviews_url = format!(
        "{API_ROOT}repos/{}/pulls/{}/reviews",
        draft.repository, draft.number
    );
    let record = |sent: &str| format!("{SENT_MARKER}\n{sent}\n\n{}", config.footer);
    // The state alone: the body is the operator's and the drafts', and
    // rewriting it could lose an edit made while sending (see [`refuse`]).
    let closing = json!({"state": "closed", "state_reason": "completed"});
    if !apply {
        let link = "REVIEW_URL";
        return Ok(vec![
            json!({"method": "POST", "url": comments_url, "body": {"body": record(&format!("Sending the draft in {}.", from.url))}}),
            json!({"method": "POST", "url": reviews_url, "body": request}),
            json!({"method": "PATCH", "url": "CLAIM_URL", "body": {"body": record(&format!("Sent the draft in {}: {link}", from.url))}}),
            json!({"method": "PATCH", "url": issue_url, "body": closing}),
        ]);
    }
    let (_, posted) = crate::plan::send(
        client,
        "POST",
        &comments_url,
        &json!({"body": record(&format!("Sending the draft in {}.", from.url))}),
        0,
    )?;
    let mine = posted["id"]
        .as_u64()
        .context("the claim has no id; nothing was sent")?;
    let replay = "the claim on the issue stops a replay; check the pull request before removing it";
    let again = issue_comments(client, issue).context(replay)?;
    let earlier: Vec<Comment> = std::iter::once(body.clone()).chain(again).collect();
    ensure!(
        !claims(&earlier, config).iter().any(|id| *id < mine),
        "an earlier claim on {shown} won; nothing was sent"
    );
    // The pull request may have closed or its head moved while its files and
    // the claim were read: `commit_id` pins the comments to `head`, but does
    // not stop a review of a pull request that has moved on.
    if let Err(error) = current(client, &draft) {
        return Err(match error.downcast_ref::<Refused>() {
            Some(Refused(reason)) => {
                refuse(client, issue, config, reason, Some(mine))?;
                error
            }
            None => error.context(replay),
        });
    }
    let (_, sent) = crate::plan::send(client, "POST", &reviews_url, &request, 0).context(replay)?;
    let link = sent["html_url"]
        .as_str()
        .filter(|u| u.starts_with("https://github.com/"))
        .context("the review has no URL")
        .context(replay)?
        .to_owned();
    crate::plan::send(
        client,
        "PATCH",
        &format!(
            "{API_ROOT}repos/{}/issues/comments/{mine}",
            issue.repository
        ),
        &json!({"body": record(&format!("Sent the draft in {}: {link}", from.url))}),
        0,
    )
    .with_context(|| format!("sent {link}, but could not record it"))?;
    crate::plan::send(client, "PATCH", &issue_url, &closing, 0)
        .with_context(|| format!("sent {link}, but could not close {shown}"))?;
    Ok(vec![json!({"review": link, "draft": from.url})])
}

#[cfg(test)]
mod tests;
