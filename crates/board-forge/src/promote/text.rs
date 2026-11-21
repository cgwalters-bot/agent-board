//! Midstream bookkeeping in a fork pull request's text (agent-board#1002).
//! A run in a midstream fork ties its pull request to the fork's source issue
//! with one standalone line, [`source_line`], which upstream readers would see
//! as a link into the fork. Publication removes that line and the generated
//! dispatch footer; everything left must already be publishable, or promotion holds.
//! Prose is never rewritten, and the removal grants no approval: the fork
//! body that the operator approved, provenance included, is unchanged.
use super::{Held, referenced_numbers};
use crate::{poll::validate_repository, stack};
use anyhow::Result;
use std::collections::BTreeSet;

/// The canonical keyword of a source line; [`FIXES`] marks a slice that
/// completes its issue, so that the handoff marks it Done on merge.
const PART_OF: &str = "Part of";

const FIXES: &str = "Fixes";

/// Punctuation the one-time [`repair`] accepts after a reference, as people
/// end a sentence; a source line itself carries at most a period.
const TRAILING: &[char] = &['.', ',', ';', '!'];

/// The canonical standalone line tying a midstream pull request to issue
/// `number` of its fork `repo`.
pub(crate) fn source_line(repo: &str, number: u64) -> String {
    format!("{PART_OF} {}", issue_url(repo, number))
}

/// The sentences a dispatch brief gets for an implement or fix run in the
/// midstream fork `fork`, so that what it writes can be promoted unchanged.
/// `source` is an implement run's issue; a fix run keeps the line it has. A
/// source staged outside the fork (one with issues disabled) is named as it
/// is, never as an invented fork issue: promotion holds that for a person.
pub(crate) fn midstream_brief(fork: &str, source: Option<(&str, u64)>) -> String {
    let link = match source {
        Some((repo, number)) if repo.eq_ignore_ascii_case(fork) => format!(
            "Link the source issue only as this line of the description, on its own: `{}` (`{FIXES}` in place of `{PART_OF}` where the pull request completes it).",
            source_line(repo, number)
        ),
        Some((repo, number)) => format!(
            "Its source issue is outside the fork: name it only as this line of the description, on its own: `{}`.",
            source_line(repo, number)
        ),
        None => format!(
            "Keep the description's one source line, `{PART_OF} {}{fork}/issues/N` (or `{FIXES}`), a full URL on its own line.",
            stack::GITHUB_WEB
        ),
    };
    let other = match source {
        Some((repo, _)) if !repo.eq_ignore_ascii_case(fork) => " other",
        _ => "",
    };
    format!(
        " This run works in the midstream fork {fork}; once promoted, upstream readers see only its pull request. Keep the pull request a draft. {link} Use no bare `#N` and no{other} tracker or forge-internal reference in the title, description or commit messages."
    )
}

fn issue_url(repo: &str, number: u64) -> String {
    format!("{}{repo}/issues/{number}", stack::GITHUB_WEB)
}

/// The issue of `repo` that `line` ties to, and whether it closes it, where
/// the whole line, less surrounding whitespace, is `Part of URL` or `Fixes
/// URL` with the issue's full URL (any case, one space, at most a trailing
/// period, as people end a sentence: composefs-rs#66).
fn standalone(line: &str, repo: &str) -> Option<(u64, bool)> {
    let line = line.trim();
    let line = line.strip_suffix('.').unwrap_or(line);
    let prefix = format!("{}{repo}/issues/", stack::GITHUB_WEB);
    [(PART_OF, false), (FIXES, true)]
        .into_iter()
        .find_map(|(keyword, closes)| {
            let rest = strip_prefix_ignore_case(line, keyword)?.strip_prefix(' ')?;
            let digits = strip_prefix_ignore_case(rest, &prefix)?;
            Some((number(digits)?, closes))
        })
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.get(..prefix.len())
        .filter(|p| p.eq_ignore_ascii_case(prefix))
        .map(|_| &text[prefix.len()..])
}

/// A positive issue number written plainly: digits only, no leading zero.
fn number(digits: &str) -> Option<u64> {
    (!digits.is_empty() && !digits.starts_with('0') && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

/// Match only the current wrapper's terminal block: the apply receipt, visible
/// footer and three gh-aw comments, all naming the same tracker run. These are
/// format checks, not proof of authorship or approval.
fn dispatch_footer<'a>(repo: &str, body: &'a str) -> Option<&'a str> {
    const TRACKER: &str = "cgwalters-forge/tracker";
    const TRACKER_ID: &str = "1389152030";
    const RUN: &str = "https://github.com/cgwalters-forge/tracker/actions/runs/";
    const WORKFLOW: &str = "agentic-job";

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Execution {
        version: u64,
        repo_id: String,
        repository: String,
        aw_workflow_id: String,
        run_id: String,
        run_attempt: String,
        run_url: String,
        publishing_job: PublishingJob,
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PublishingJob {
        key: String,
        check_run_id: String,
    }

    fn last_line(body: &str) -> (&str, &str) {
        let body = body.trim_end_matches([' ', '\t', '\r', '\n']);
        body.rsplit_once('\n')
            .map(|(prefix, line)| (prefix.strip_suffix('\r').unwrap_or(prefix), line))
            .unwrap_or(("", body))
    }

    let (rest, execution) = last_line(body);
    let (rest, workflow) = last_line(rest);
    let (rest, workflow_id) = last_line(rest);
    let (rest, footer) = last_line(rest);
    let (author, receipt) = last_line(rest);
    let search_repo = repo.replace('/', "%2F");
    let search = format!(
        "https://github.com/search?q=repo%3A{search_repo}+%22gh-aw-workflow-id%3A+{WORKFLOW}%22&type=pullrequests"
    );
    let run_id = footer
        .strip_prefix("> Generated by [dispatch](")?
        .strip_prefix(RUN)?
        .strip_suffix(&format!(") · [◷]({search})"))?;
    number(run_id)?;
    let (receipt_run, stamp) = receipt
        .strip_prefix("<!-- agentic-job-applied: ")?
        .strip_suffix(" -->")?
        .split_once('/')?;
    let (position, digest) = stamp.split_once('/')?;
    if receipt_run != run_id
        || (position != "0" && number(position).is_none())
        || digest.len() != 16
        || !digest
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    let run = format!("{RUN}{run_id}");
    if workflow_id != format!("<!-- gh-aw-workflow-id: {WORKFLOW} -->")
        || workflow
            != format!(
                "<!-- gh-aw-agentic-workflow: dispatch, id: {run_id}, workflow_id: {WORKFLOW}, run: {run} -->"
            )
    {
        return None;
    }
    let execution: Execution = serde_json::from_str(
        execution
            .strip_prefix("<!-- gh-aw-execution: ")?
            .strip_suffix(" -->")?,
    )
    .ok()?;
    if execution.version != 1
        || execution.repo_id != TRACKER_ID
        || execution.repository != TRACKER
        || execution.aw_workflow_id != WORKFLOW
        || execution.run_id != run_id
        || execution.run_url != run
        || execution.publishing_job.key != "apply"
    {
        return None;
    }
    number(&execution.run_attempt)?;
    number(&execution.publishing_job.check_run_id)?;

    // The context scan sees only the prefix. Check the whole input's line
    // endings before trusting the suffix boundaries.
    let separated = author.is_empty() || author.ends_with('\n');
    (separated && !has_bare_cr(body) && closed(author)).then(|| author.trim_end())
}

/// `body` less its dispatch footer and one standalone source line for
/// `source`, or unchanged, byte for byte, without either. More than one
/// source line, or one naming another issue,
/// holds: which to remove would be a guess. So does a body without one where
/// a line still names the source issue's URL, saying why, since [`check`]
/// would hold it only as a reference into the midstream (composefs-rs#66).
pub(super) fn strip(repo: &str, body: &str, source: u64) -> Result<String> {
    let original = body;
    let body = dispatch_footer(repo, body).unwrap_or(body);
    let lines: Vec<(usize, (u64, bool))> = body
        .split('\n')
        .enumerate()
        .filter_map(|(i, line)| standalone(line, repo).map(|found| (i, found)))
        .collect();
    let canonical = source_line(repo, source);
    // A closing reference is held as one by `check`, more precisely.
    hold!(
        !lines.is_empty() || !names_issue(body, repo, source) || stack::closes(body, repo, source),
        "source line not recognised; put `{canonical}` on a line of its own"
    );
    for (at, _) in &lines {
        hold!(
            top_level(body, *at),
            "source metadata in code or HTML needs manual publication repair"
        );
    }
    hold!(
        lines.len() <= 1,
        "text has {} source lines; keep one `{canonical}`",
        lines.len()
    );
    let Some(&(at, (number, _))) = lines.first() else {
        return Ok(body.to_owned());
    };
    hold!(
        number == source,
        "source line names {repo}#{number}, not the linked source {repo}#{source}"
    );
    let published = without(body, &BTreeSet::from([at]));
    hold!(
        !stack::introduces_closing_reference(original, &published),
        "removing source metadata would introduce a closing reference; manual publication repair required"
    );
    Ok(published)
}

/// Whether `text` has issue `number` of `repo`'s full URL, any case, not
/// followed by another digit.
fn names_issue(text: &str, repo: &str, number: u64) -> bool {
    let lower = text.to_ascii_lowercase();
    let url = issue_url(repo, number).to_ascii_lowercase();
    lower.match_indices(&url).any(|(i, _)| {
        !lower[i + url.len()..]
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_digit())
    })
}

/// Why a persistent repair holds a body that fails [`closed`].
pub(super) const UNCLOSED: &str = "unclosed or ambiguous code or HTML needs manual repair";

/// Whether line `at` of `body` is top-level Markdown, so that a matching
/// source line there is bookkeeping rather than part of an approved example.
pub(super) fn top_level(body: &str, at: usize) -> bool {
    contexts(body).0.get(at).copied().unwrap_or(false)
}

/// Whether every code or HTML context in `body` is closed by its end, and
/// none is ambiguous: a persistent repair holds otherwise, wherever its
/// references are.
pub(super) fn closed(body: &str) -> bool {
    contexts(body).1
}

/// For each line of `body`, whether it is top-level Markdown; then whether
/// all contexts closed (see [`closed`]). Without a Markdown parser, anything
/// that could put a line in code or HTML counts as not: an open fence, its
/// own indentation (a tab or four spaces), or a backtick, `<`, `[` or `|`
/// earlier in its paragraph, since a code span, inline HTML, link text or
/// table can span lines. Fenced code closed earlier does not matter. HTML
/// does, wherever it is: elements stay open across blank lines and hide
/// fences, and telling where one ends takes a parser, so every line after a
/// line starting with `<`, or a tag-like `<` outside code spans, counts as
/// HTML. So does every line after an indented fence, or a fence or HTML in a
/// list or quote, which may end where a parser would not.
fn contexts(body: &str) -> (Vec<bool>, bool) {
    // Markdown recognizes bare CR as a line ending; our callers index lines
    // by LF. Do not authorize any edit on ambiguous boundaries.
    if has_bare_cr(body) {
        return (Vec::new(), false);
    }
    let mut fence: Option<(char, usize)> = None;
    let mut inline = false;
    let mut ambiguous = false;
    let mut lines = Vec::new();
    for line in body.split('\n') {
        let trimmed = line.trim_start();
        let blank = line.trim_matches([' ', '\t', '\r']).is_empty();
        let indent = &line[..line.len() - trimmed.len()];
        let top = indent.len() < 4 && indent.bytes().all(|b| b == b' ');
        lines.push(!ambiguous && fence.is_none() && !inline && top);
        if ambiguous {
            continue;
        }
        if let Some((mark, open)) = fence {
            let closes = fence_mark(trimmed).is_some_and(|(m, run)| {
                m == mark
                    && run >= open
                    && trimmed[run..].trim_matches([' ', '\t', '\r']).is_empty()
            });
            if top && closes {
                fence = None;
            }
        } else if blank {
            inline = false;
        } else if let Some(found) = fence_mark(trimmed) {
            ambiguous = !indent.is_empty();
            fence = Some(found);
            inline = false;
        } else if nested_block(trimmed) || trimmed.starts_with('<') || has_tag(line, inline) {
            ambiguous = true;
        } else {
            inline |= line.contains(['`', '<', '[', '|']);
        }
    }
    (lines, fence.is_none() && !ambiguous)
}

fn has_bare_cr(body: &str) -> bool {
    body.split_inclusive('\n')
        .any(|line| line.strip_suffix("\r\n").unwrap_or(line).contains('\r'))
}

/// Whether `line` may hold raw HTML: a `<` starting a tag, closing tag,
/// comment or declaration, outside code spans only where the line alone
/// pairs its backticks (single, unescaped, an even number of them, and no
/// span `continued` from earlier in its paragraph).
fn has_tag(line: &str, continued: bool) -> bool {
    let paired = !continued
        && !line.contains('\\')
        && line.matches('`').count().is_multiple_of(2)
        && !line.contains("``");
    let text = match paired {
        true => line.split('`').step_by(2).collect::<Vec<_>>().join(" "),
        false => line.to_owned(),
    };
    text.match_indices('<').any(|(i, _)| {
        text[i + 1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, '/' | '!' | '?'))
    })
}

/// The mark and length of a code fence starting `text`: three or more
/// backticks or tildes, where a backtick fence's info string has no backtick
/// (`` ```x``` `` is a code span).
fn fence_mark(text: &str) -> Option<(char, usize)> {
    let mark = text.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let run = text.chars().take_while(|c| *c == mark).count();
    (run >= 3 && (mark == '~' || !text[run..].contains('`'))).then_some((mark, run))
}

/// Whether `text` opens a fence or HTML block after list item or block quote
/// markers.
fn nested_block(text: &str) -> bool {
    let mut rest = text;
    loop {
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        let marker = match rest.as_bytes().get(digits) {
            Some(b'.' | b')') if digits > 0 => digits + 1,
            Some(b'-' | b'*' | b'+' | b'>') if digits == 0 => 1,
            _ => return rest != text && (fence_mark(rest).is_some() || rest.starts_with('<')),
        };
        rest = rest[marker..].trim_start();
    }
}

/// `text` less its leading and trailing blank lines and trailing whitespace;
/// unlike `trim`, the first line kept keeps its indentation (code).
pub(super) fn trim_blank_lines(text: &str) -> &str {
    let start: usize = text
        .split_inclusive('\n')
        .take_while(|line| line.trim().is_empty())
        .map(str::len)
        .sum();
    text[start..].trim_end()
}

/// `body` without the lines numbered in `drop`, leaving one blank line where
/// a dropped paragraph was, and no surrounding blank lines.
fn without(body: &str, drop: &BTreeSet<usize>) -> String {
    let blank = |line: &str| line.trim().is_empty();
    let mut kept: Vec<&str> = Vec::new();
    let mut gap = false;
    for (i, line) in body.split('\n').enumerate() {
        if drop.contains(&i) {
            gap = kept.last().is_none_or(|l| blank(l));
            continue;
        }
        if gap && blank(line) {
            gap = false;
            continue;
        }
        gap = false;
        kept.push(line);
    }
    trim_blank_lines(&kept.join("\n")).to_owned()
}

/// Holds `text` (a title, or a body as it would be published) that would
/// change meaning or point into the midstream once upstream: a bare `#N`,
/// a closing reference to the fork, or any reference into the fork's owner
/// other than `upstream` itself (the fork, its tracker, other forks).
pub(super) fn check(repo: &str, upstream: &str, text: &str) -> Result<()> {
    // A qualified reference keeps its meaning; any bare reference changes
    // repository. Be conservative even in Markdown examples: never rewrite.
    for (i, _) in text.match_indices('#') {
        if text.as_bytes().get(i + 1).is_some_and(u8::is_ascii_digit) {
            let prefix = &text[..i];
            let qualifier = prefix
                .rsplit(|c: char| !(c.is_ascii_alphanumeric() || "-._/".contains(c)))
                .next()
                .unwrap_or_default();
            let qualified = validate_repository(qualifier).is_ok();
            hold!(
                qualified,
                "text has a bare #N reference that changes repository upstream"
            );
        }
    }
    for n in referenced_numbers(text) {
        hold!(
            !stack::closes(text, repo, n),
            "text has a closing reference to {repo}#{n}"
        );
    }
    if let Some(internal) = internal_reference(repo, upstream, text) {
        hold!(
            false,
            "text references {internal}, inside the midstream, which upstream readers cannot follow"
        );
    }
    Ok(())
}

/// The first `OWNER/NAME` in `text` under `repo`'s owner, at a word start
/// (a URL's path included), that is not `upstream`.
fn internal_reference(repo: &str, upstream: &str, text: &str) -> Option<String> {
    let owner = repo.split('/').next().unwrap_or(repo).to_ascii_lowercase();
    let lower = text.to_ascii_lowercase();
    let name_char = |c: char| c.is_ascii_alphanumeric() || "-_.".contains(c);
    lower
        .match_indices(&format!("{owner}/"))
        .filter(|(i, _)| !lower[..*i].ends_with(name_char))
        .map(|(i, found)| {
            let rest = &text[i + found.len()..];
            let name = &rest[..rest.find(|c: char| !name_char(c)).unwrap_or(rest.len())];
            format!(
                "{}{}",
                &text[i..i + found.len()],
                name.trim_end_matches('.')
            )
        })
        .find(|at| !at.eq_ignore_ascii_case(upstream))
}

/// What the one-time repair does with a midstream draft written before
/// agent-board#1002.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Repair {
    /// Already publishable as it is.
    Unchanged,
    /// Its body, with source bookkeeping normalized.
    Rewrite(String),
    /// Left for a person: why.
    Skip(String),
}

/// A standalone bookkeeping line: a keyword and one issue reference.
enum Bookkeeping {
    /// An issue of the fork itself, and whether the keyword closes it.
    Source(u64, bool),
    /// A repository under the fork's owner other than the fork and upstream,
    /// such as the tracker: a forge-internal reference to drop.
    Internal,
}

/// `line` as bookkeeping: the whole line, less surrounding whitespace, is
/// `Part of` or a closing keyword (optionally with a colon), then `#N`,
/// `OWNER/REPO#N` or an issue URL, then at most one [`TRAILING`] mark.
/// References outside the fork's owner, upstream's included, are prose.
fn bookkeeping(line: &str, repo: &str, upstream: &str) -> Option<Bookkeeping> {
    let line = line.trim();
    let (keyword, reference) = match strip_prefix_ignore_case(line, "part of") {
        Some(rest) => (
            "part of",
            rest.strip_prefix(':').unwrap_or(rest).strip_prefix(' ')?,
        ),
        None => {
            let (keyword, rest) = line.split_once(' ')?;
            (keyword.strip_suffix(':').unwrap_or(keyword), rest)
        }
    };
    if keyword != "part of" && !stack::CLOSING.contains(&keyword.to_ascii_lowercase().as_str()) {
        return None;
    }
    let reference = reference.strip_suffix(TRAILING).unwrap_or(reference);
    let (target, digits) = match reference.strip_prefix('#') {
        Some(digits) => (repo, digits),
        None => match strip_prefix_ignore_case(reference, stack::GITHUB_WEB) {
            Some(path) => {
                let (target, digits) = path.rsplit_once("/issues/")?;
                (target, digits)
            }
            None => reference.rsplit_once('#')?,
        },
    };
    let number = number(digits)?;
    validate_repository(target).ok()?;
    let owner = |r: &str| r.split('/').next().unwrap_or(r).to_ascii_lowercase();
    if target.eq_ignore_ascii_case(repo) {
        Some(Bookkeeping::Source(number, keyword != "part of"))
    } else if owner(target) == owner(repo) && !target.eq_ignore_ascii_case(upstream) {
        Some(Bookkeeping::Internal)
    } else {
        None
    }
}

/// Normalize a draft's source bookkeeping: its standalone lines naming one
/// fork issue become that issue's one canonical line (`Fixes` where they
/// close it), and standalone forge-internal reference lines are dropped.
/// Everything else stays as written; the result must pass the publication
/// checks, or the draft is skipped whole for a person, as is any draft whose
/// source is ambiguous, missing or only in prose.
pub(super) fn repair(repo: &str, upstream: &str, title: &str, body: &str) -> Repair {
    let lines: Vec<(usize, Bookkeeping)> = body
        .split('\n')
        .enumerate()
        .filter_map(|(i, line)| bookkeeping(line, repo, upstream).map(|b| (i, b)))
        .collect();
    if lines.iter().any(|(at, _)| !top_level(body, *at)) {
        return Repair::Skip("source metadata in code or HTML needs manual repair".into());
    }
    if !closed(body) {
        return Repair::Skip(UNCLOSED.into());
    }
    let sources: BTreeSet<(u64, bool)> = lines
        .iter()
        .filter_map(|(_, b)| match b {
            Bookkeeping::Source(n, closes) => Some((*n, *closes)),
            Bookkeeping::Internal => None,
        })
        .collect();
    let numbers: BTreeSet<u64> = sources.iter().map(|(n, _)| *n).collect();
    let Some(&source) = numbers.first() else {
        return Repair::Skip("no standalone source line names an issue of the fork".into());
    };
    if numbers.len() > 1 {
        return Repair::Skip(format!(
            "source lines name {} fork issues; which is the source is for a person",
            numbers.len()
        ));
    }
    if sources.len() > 1 {
        return Repair::Skip(format!(
            "source lines both close and are part of {repo}#{source}"
        ));
    }
    let closes = sources.contains(&(source, true));
    let first = lines
        .iter()
        .find(|(_, b)| matches!(b, Bookkeeping::Source(..)))
        .map(|(i, _)| *i);
    let drop: BTreeSet<usize> = lines
        .iter()
        .map(|(i, _)| *i)
        .filter(|i| Some(*i) != first)
        .collect();
    let canonical = match closes {
        true => format!("{FIXES} {}", issue_url(repo, source)),
        false => source_line(repo, source),
    };
    let mut rewritten: Vec<String> = body.split('\n').map(str::to_owned).collect();
    if let Some(first) = first {
        rewritten[first] = canonical;
    }
    let normalized = rewritten.join("\n");
    let rewritten = match drop.is_empty() {
        true => normalized.clone(),
        false => without(&normalized, &drop),
    };
    let checked = (|| -> Result<()> {
        hold!(
            !stack::introduces_closing_reference(&normalized, &rewritten),
            "removing source metadata would introduce a closing reference; manual repair required"
        );
        check(repo, upstream, title)?;
        let tied: Vec<u64> = referenced_numbers(&rewritten)
            .into_iter()
            .filter(|n| stack::closes(&rewritten, repo, *n) || stack::part_of(&rewritten, repo, *n))
            .collect();
        hold!(
            tied == [source],
            "cannot tie fork PR to one issue deterministically"
        );
        let published = strip(repo, &rewritten, source)?;
        hold!(!published.trim().is_empty(), "empty body");
        check(repo, upstream, &published)
    })();
    match checked {
        Err(error) => Repair::Skip(format!("{error:#}")),
        Ok(()) if rewritten == body => Repair::Unchanged,
        Ok(()) => Repair::Rewrite(rewritten),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "cgwalters-forge/composefs-rs";
    const UPSTREAM: &str = "composefs/composefs-rs";

    fn url(n: u64) -> String {
        issue_url(REPO, n)
    }

    #[test]
    fn publication_text_table() {
        // (body, source, published text or hold reason)
        let cases: Vec<(String, u64, Result<&str, String>)> = vec![
            (format!("Work.\n\nPart of {}", url(61)), 61, Ok("Work.")),
            (format!("Work.\n\nFixes {}", url(61)), 61, Ok("Work.")),
            (
                format!("  part OF {}  \r\n\r\nWork.\r\n", url(61)),
                61,
                Ok("Work."),
            ),
            (
                format!("Intro.\n\nPart of {}\n\nMore.", url(61)),
                61,
                Ok("Intro.\n\nMore."),
            ),
            (
                format!(
                    "Work.\n\nPart of {}\nFixes composefs/composefs-rs#9",
                    url(61)
                ),
                61,
                Ok("Work.\n\nFixes composefs/composefs-rs#9"),
            ),
            // Tags in code spans and comparisons are prose; any raw HTML
            // earlier holds, since only a parser can tell where it ends.
            (
                format!("Use `<code>` if a < b.\n\nPart of {}", url(61)),
                61,
                Ok("Use `<code>` if a < b."),
            ),
            (
                format!("x `a\nb` <details> `c\n\nPart of {}", url(61)),
                61,
                Err("source metadata in code or HTML needs manual publication repair".into()),
            ),
            (
                format!("Use \\`<details>\\` here.\n\nPart of {}", url(61)),
                61,
                Err("source metadata in code or HTML needs manual publication repair".into()),
            ),
            (
                format!("Use <code>x</code>.\n\nPart of {}", url(61)),
                61,
                Err("source metadata in code or HTML needs manual publication repair".into()),
            ),
            (
                format!("<code>\n\nPart of {}\n\n</code>", url(61)),
                61,
                Err("source metadata in code or HTML needs manual publication repair".into()),
            ),
            // No source line: byte for byte, whitespace included.
            ("  Work.\n\n".into(), 61, Ok("  Work.\n\n")),
            // A sentence's period is still a source line (composefs-rs#66).
            (format!("Work.\n\nPart of {}.", url(61)), 61, Ok("Work.")),
            (format!("Work.\n\nFixes {}. ", url(61)), 61, Ok("Work.")),
            // Other punctuation or a list marker makes it prose, held as an
            // unrecognised source line where it names the source issue.
            (
                format!("Work.\n\nPart of {}!", url(61)),
                61,
                Err(format!(
                    "source line not recognised; put `Part of {}` on a line of its own",
                    url(61)
                )),
            ),
            (
                format!("Work.\n\n- Part of {}", url(61).to_uppercase()),
                61,
                Err(format!(
                    "source line not recognised; put `Part of {}` on a line of its own",
                    url(61)
                )),
            ),
            (format!("Work.\n\nPart of {}..", url(62)), 61, Ok("checked")),
            // `check` holds a closing reference as one.
            (format!("Work.\n\n- Fixes {}", url(61)), 61, Ok("checked")),
            (
                format!("Work.\n\n- Part of {}0", url(61)),
                61,
                Ok("checked"),
            ),
            (
                format!("Part of {}\nPart of {}", url(61), url(61)),
                61,
                Err(format!(
                    "text has 2 source lines; keep one `Part of {}`",
                    url(61)
                )),
            ),
            (
                format!("Work.\n\nPart of {}", url(62)),
                61,
                Err(format!(
                    "source line names {REPO}#62, not the linked source {REPO}#61"
                )),
            ),
        ];
        for (body, source, expected) in cases {
            let got = strip(REPO, &body, source).map_err(|e| format!("{e:#}"));
            match expected {
                Ok("checked") => {
                    assert_eq!(got.as_deref(), Ok(body.as_str()), "{body:?}");
                    assert!(check(REPO, UPSTREAM, &body).is_err(), "{body:?}");
                }
                expected => assert_eq!(got, expected.map(str::to_owned), "{body:?}"),
            }
        }
    }

    #[test]
    fn midstream_brief_table() {
        let tracker = "https://github.com/cgwalters-forge/tracker/issues/12";
        for (source, has, lacks) in [
            // An implement run in the fork links its fork issue canonically.
            (
                Some((REPO, 61)),
                format!("`Part of {}`", url(61)),
                tracker.to_owned(),
            ),
            // Staged in the tracker (issues disabled): named as it is, never
            // as an invented fork issue.
            (
                Some(("cgwalters-forge/tracker", 12)),
                format!(
                    "is outside the fork: name it only as this line of the description, on its own: `Part of {tracker}`"
                ),
                format!("{REPO}/issues/12"),
            ),
            // A fix run keeps the line it has.
            (
                None,
                format!("`Part of https://github.com/{REPO}/issues/N`"),
                tracker.to_owned(),
            ),
        ] {
            let brief = midstream_brief(REPO, source);
            for want in [
                &format!(" This run works in the midstream fork {REPO};"),
                "Keep the pull request a draft.",
                "tracker or forge-internal reference in the title, description or commit messages.",
                &has,
            ] {
                assert!(brief.contains(want), "{source:?}: {want}: {brief}");
            }
            assert!(!brief.contains(&lacks), "{source:?}: {brief}");
            // A staged source line is the one tracker reference allowed.
            let staged = source.is_some_and(|(repo, _)| repo != REPO);
            assert_eq!(brief.contains("no other tracker"), staged, "{brief}");
        }
    }

    #[test]
    fn check_table() {
        for (text, expected) in [
            ("Work.", None),
            ("Fixes composefs/composefs-rs#9", None),
            (
                "See https://github.com/composefs/composefs-rs/issues/9",
                None,
            ),
            (
                "See https://github.com/composefs/composefs-rs/pull/1#issuecomment-2",
                None,
            ),
            (
                "Implement #61",
                Some("text has a bare #N reference that changes repository upstream"),
            ),
            (
                "Fixes #61",
                Some("text has a bare #N reference that changes repository upstream"),
            ),
            (
                "Closes cgwalters-forge/composefs-rs#61",
                Some("text has a closing reference to cgwalters-forge/composefs-rs#61"),
            ),
            (
                "See https://github.com/cgwalters-forge/composefs-rs/issues/61.",
                Some(
                    "text references cgwalters-forge/composefs-rs, inside the midstream, which upstream readers cannot follow",
                ),
            ),
            (
                "Part of cgwalters-forge/composefs-rs#61",
                Some(
                    "text references cgwalters-forge/composefs-rs, inside the midstream, which upstream readers cannot follow",
                ),
            ),
            (
                "Tracked in Cgwalters-Forge/tracker#12.",
                Some(
                    "text references Cgwalters-Forge/tracker, inside the midstream, which upstream readers cannot follow",
                ),
            ),
            // Another owner whose name ends like the midstream's is no reference.
            ("See other-cgwalters-forge/x#1", None),
        ] {
            let got = check(REPO, UPSTREAM, text).err().map(|e| format!("{e:#}"));
            assert_eq!(got.as_deref(), expected, "{text:?}");
        }
    }

    #[test]
    fn repair_table() {
        let canonical = source_line(REPO, 61);
        let fixes = format!("Fixes {}", url(61));
        let cases: Vec<(&str, String, Repair)> = vec![
            (
                "bare Fixes",
                "Work.\n\nFixes #61".into(),
                Repair::Rewrite(format!("Work.\n\n{fixes}")),
            ),
            (
                "bare Part of with tracker line",
                "Work.\n\nPart of #61\nPart of cgwalters-forge/tracker#12".into(),
                Repair::Rewrite(format!("Work.\n\n{canonical}")),
            ),
            (
                "qualified repo#N",
                "Work.\n\nPart of: cgwalters-forge/composefs-rs#61.".into(),
                Repair::Rewrite(format!("Work.\n\n{canonical}")),
            ),
            (
                "duplicate source lines",
                format!("Part of #61\n\nWork.\n\nPart of {}", url(61)),
                Repair::Rewrite(format!("{canonical}\n\nWork.")),
            ),
            (
                "upstream link kept",
                "Work.\n\nFixes #61\nFixes composefs/composefs-rs#9".into(),
                Repair::Rewrite(format!(
                    "Work.\n\n{fixes}\nFixes composefs/composefs-rs#9"
                )),
            ),
            (
                "canonical is idempotent",
                format!("Work.\n\n{canonical}"),
                Repair::Unchanged,
            ),
            (
                "none",
                "Work.".into(),
                Repair::Skip("no standalone source line names an issue of the fork".into()),
            ),
            (
                "upstream only is not a source",
                "Work.\n\nFixes composefs/composefs-rs#9".into(),
                Repair::Skip("no standalone source line names an issue of the fork".into()),
            ),
            (
                "two source issues",
                "Work.\n\nPart of #61\nPart of #62".into(),
                Repair::Skip(
                    "source lines name 2 fork issues; which is the source is for a person".into(),
                ),
            ),
            (
                "mixed kinds",
                "Work.\n\nPart of #61\nFixes #61".into(),
                Repair::Skip(format!(
                    "source lines both close and are part of {REPO}#61"
                )),
            ),
            (
                "bare reference in prose",
                "Builds on #60.\n\nPart of #61".into(),
                Repair::Skip(
                    "text has a bare #N reference that changes repository upstream".into(),
                ),
            ),
            (
                "tracker reference in prose",
                "See cgwalters-forge/tracker#12 for why.\n\nPart of #61".into(),
                Repair::Skip("text references cgwalters-forge/tracker, inside the midstream, which upstream readers cannot follow".into()),
            ),
            (
                "only bookkeeping",
                "Part of #61".into(),
                Repair::Skip("empty body".into()),
            ),
            (
                "staged tracker source",
                "Work.\n\nPart of https://github.com/cgwalters-forge/tracker/issues/12".into(),
                Repair::Skip("no standalone source line names an issue of the fork".into()),
            ),
        ];
        for (name, body, expected) in cases {
            let got = repair(REPO, UPSTREAM, "Implement the slice", &body);
            assert_eq!(got, expected, "{name}");
            if let Repair::Rewrite(after) = got {
                // Idempotent, and what publication then sends has no fork link.
                assert_eq!(
                    repair(REPO, UPSTREAM, "Implement the slice", &after),
                    Repair::Unchanged,
                    "{name}"
                );
                let published = strip(REPO, &after, 61).unwrap();
                check(REPO, UPSTREAM, &published).unwrap();
                assert!(!published.contains(REPO), "{name}: {published}");
            }
        }
        assert_eq!(
            repair(REPO, UPSTREAM, "Implement #61", "Work.\n\nPart of #61"),
            Repair::Skip("text has a bare #N reference that changes repository upstream".into()),
            "a title is never rewritten"
        );
    }
}
