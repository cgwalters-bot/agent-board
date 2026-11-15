//! One-time, explicit repair of legacy draft bodies; never part of publication.
use super::*;
use serde_json::json;

const MAX_REPAIRS: usize = 10;

/// Bare references can silently name unrelated upstream issues. This opt-in
/// migration changes only bot-owned open drafts in metadata-admitted forks;
/// ambiguous sources hold rather than guessing from branch names or PR numbers.
/// A zero write cap previews; a positive cap applies at most ten body edits.
pub fn repair_drafts<C: Rest + Transport>(
    client: &C,
    config: &Config,
    max_writes: usize,
) -> Result<Vec<String>> {
    ensure!(
        max_writes <= MAX_REPAIRS,
        "draft repair allows at most {MAX_REPAIRS} writes"
    );
    let mut completed = Vec::new();
    let result = pass::promotion_reads(client, config, |client, lines| {
        if max_writes > 0 {
            let identity = get(client, "user")?;
            ensure!(
                identity["login"]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case(&config.bot)),
                "repair credential must authenticate as configured bot"
            );
        }
        let mut written = 0;
        for repo in targets(client, config)?.keys() {
            let mut pulls = pages(
                client,
                &format!("repos/{repo}/pulls?state=open&sort=created&direction=asc"),
            )?;
            pulls.sort_by_key(|p| p["number"].as_u64());
            let mut seen = BTreeSet::new();
            for pull in pulls {
                let number = pull["number"]
                    .as_u64()
                    .filter(|n| *n > 0)
                    .context("invalid repair pull number")?;
                ensure!(seen.insert(number), "duplicate repair pull {repo}#{number}");
                if pull["state"] != "open"
                    || pull["draft"] != true
                    || !pull["user"]["login"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(&config.bot))
                    || !pull["base"]["repo"]["full_name"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(repo))
                {
                    continue;
                }
                // GitHub reports an empty description as null: nothing to repair.
                let Some(text) = pull["body"].as_str() else {
                    continue;
                };
                if bare_refs(text).is_empty() {
                    continue;
                }
                let source = match source_issue(repo, &pull) {
                    Ok(source) => source,
                    Err(error) => {
                        lines.push(format!("hold repair {repo}#{number}: {error:#}"));
                        continue;
                    }
                };
                // Prove this is an issue, not a PR or a dangling reference.
                if let Err(error) = crate::upstream::issue(client, repo, source.number) {
                    lines.push(format!("hold repair {repo}#{number}: {error:#}"));
                    continue;
                }
                let repaired = match repaired_body(text, repo, source.number) {
                    Ok(body) => body,
                    Err(error) => {
                        lines.push(format!("hold repair {repo}#{number}: {error:#}"));
                        continue;
                    }
                };
                let count = bare_refs(text).len();
                if max_writes == 0 {
                    lines.push(format!("would repair {repo}#{number}: replace/drop {count} bare reference(s); source https://github.com/{repo}/issues/{}", source.number));
                    continue;
                }
                if written == max_writes {
                    lines.push("repair held by --max-writes; rerun to continue".into());
                    return Ok(());
                }
                let path = format!("repos/{repo}/pulls/{number}");
                let fresh = get(client, &path)?;
                for key in [
                    "number",
                    "state",
                    "draft",
                    "user",
                    "base",
                    "body",
                    "updated_at",
                ] {
                    ensure!(
                        fresh[key] == pull[key],
                        "repair {repo}#{number} {key} changed; retry"
                    );
                }
                let (status, _) =
                    client.send("PATCH", &format!("{API}{path}"), &json!({"body":repaired}))?;
                ensure!(status == 200, "repair {repo}#{number}: HTTP {status}");
                written += 1;
                let line = format!(
                    "repaired {repo}#{number}: replace/drop {count} bare reference(s); source https://github.com/{repo}/issues/{}",
                    source.number
                );
                completed.push(line.clone());
                lines.push(line);
            }
        }
        Ok(())
    });
    // The CLI prints ordinary errors too. Preserve earlier completed edits in
    // that error chain if a later read/revalidation/write fails.
    if completed.is_empty() {
        result
    } else {
        result.with_context(|| completed.join("\n"))
    }
}

pub(super) fn bare_refs(text: &str) -> Vec<std::ops::Range<usize>> {
    text.match_indices('#')
        .filter_map(|(i, _)| {
            let end = i + 1 + text[i + 1..].bytes().take_while(u8::is_ascii_digit).count();
            let prefix = text[..i]
                .rsplit(|c: char| !(c.is_ascii_alphanumeric() || "-._/".contains(c)))
                .next()
                .unwrap_or_default();
            (end > i + 1 && validate_repository(prefix).is_err()).then_some(i..end)
        })
        .collect()
}

fn repaired_body(text: &str, repo: &str, source: u64) -> Result<String> {
    // A publication hold can be conservative about arbitrary #digits; a
    // persistent edit cannot. Only whole bookkeeping lines are repairable.
    // Leave code/examples and ambiguous inline references for a human.
    ensure!(text::closed(text), text::UNCLOSED);
    let mut repaired = String::new();
    for (at, line) in text.split_inclusive('\n').enumerate() {
        if bare_refs(line).is_empty() {
            repaired.push_str(line);
            continue;
        }
        ensure!(
            text::top_level(text, at),
            "bare references in code or HTML need manual repair"
        );
        let words: Vec<_> = line.split_whitespace().collect();
        let (keyword, reference) = match words.as_slice() {
            [part, of, reference]
                if part.eq_ignore_ascii_case("part") && of.eq_ignore_ascii_case("of") =>
            {
                ("part of".to_owned(), *reference)
            }
            [keyword, reference] => (
                keyword.trim_end_matches(':').to_ascii_lowercase(),
                *reference,
            ),
            _ => anyhow::bail!("bare references outside standalone bookkeeping need manual repair"),
        };
        let number = reference
            .trim_end_matches(['.', ',', ';', '!', '?'])
            .strip_prefix('#')
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|s| s.parse::<u64>().ok());
        ensure!(
            line.starts_with(|c: char| c.is_ascii_alphabetic())
                && number.is_some()
                && [
                    "fix",
                    "fixes",
                    "fixed",
                    "close",
                    "closes",
                    "closed",
                    "resolve",
                    "resolves",
                    "resolved",
                    "part of",
                    "refs",
                    "references",
                    "see"
                ]
                .contains(&keyword.as_str()),
            "bare references outside standalone bookkeeping need manual repair"
        );
    }
    ensure!(
        !crate::stack::introduces_closing_reference(text, &repaired),
        "removing bare references would introduce a closing reference; manual repair required"
    );
    // Normalize existing source metadata too, without duplicating its URL.
    // Its own paragraph, so that the appended line is never inline context.
    let pull = json!({"body":format!("{repaired}\n\nPart of {repo}#{source}")});
    let cleaned = source_text(repo, &pull)?;
    let keyword = if crate::stack::closes(text, repo, source) {
        "Fixes"
    } else {
        "Part of"
    };
    let line = format!("{keyword} https://github.com/{repo}/issues/{source}");
    Ok(match text::trim_blank_lines(&cleaned) {
        "" => line,
        kept => format!("{kept}\n\n{line}"),
    })
}
