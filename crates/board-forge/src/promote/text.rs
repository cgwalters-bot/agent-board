//! Midstream bookkeeping in a fork pull request's text (agent-board#1002).
//! A run in a midstream fork ties its pull request to the fork's source issue
//! with one standalone line, [`source_line`], which upstream readers would see
//! as a link into the fork. Publication removes exactly that line and nothing
//! else; everything left must already be publishable, or promotion holds.
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
/// end a sentence; a source line itself carries none.
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
/// URL` with the issue's full URL (any case, one space, no punctuation).
fn standalone(line: &str, repo: &str) -> Option<(u64, bool)> {
    let line = line.trim();
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

/// `body` less its one standalone source line for `source`, or unchanged,
/// byte for byte, without one. More than one, or one naming another issue,
/// holds: which to remove would be a guess.
pub(super) fn strip(repo: &str, body: &str, source: u64) -> Result<String> {
    let lines: Vec<(usize, (u64, bool))> = body
        .split('\n')
        .enumerate()
        .filter_map(|(i, line)| standalone(line, repo).map(|found| (i, found)))
        .collect();
    let canonical = source_line(repo, source);
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
    Ok(without(body, &BTreeSet::from([at])))
}

/// `body` without the lines numbered in `drop`, leaving one blank line where
/// a dropped paragraph was, and no surrounding whitespace.
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
    kept.join("\n").trim().to_owned()
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
    let rewritten = match drop.is_empty() {
        true => rewritten.join("\n"),
        false => without(&rewritten.join("\n"), &drop),
    };
    let checked = (|| -> Result<()> {
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
            // No source line: byte for byte, whitespace included.
            ("  Work.\n\n".into(), 61, Ok("  Work.\n\n")),
            // Punctuation or a list marker makes it prose, not bookkeeping.
            (format!("Work.\n\nPart of {}.", url(61)), 61, Ok("checked")),
            (format!("Work.\n\n- Part of {}", url(61)), 61, Ok("checked")),
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
