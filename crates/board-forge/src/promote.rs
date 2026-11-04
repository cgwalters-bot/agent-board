//! Standalone promotion evidence preview. No mutation or publication evaluation.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::BTreeSet;
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
pub const REMAINING: &str = "promote: candidate only; remaining gates: policy freshness/loosening, stack, human-text provenance, distance, DCO, publication; no writes implemented; bot-fork heads require recorded staging verification";

pub fn enabled(config: &Config) -> bool {
    config.promote.as_ref().is_some_and(|p| !p.forks.is_empty())
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
    pass::preview_reads(client, |client| {
        let mut lines = Vec::new();
        let promote = config.promote.as_ref().context("promote configuration")?;
        for (repo, target) in &promote.forks {
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
                    Ok(command) => lines.push(format!("candidate promote {repo}#{number} head={head} {command}; staging verification required for bot-fork head")),
                    Err(error) => lines.push(format!("hold promote {repo}#{number} head={head} command=unverified: {error:#}")),
                }
            }
        }
        lines.push(REMAINING.into());
        Ok(lines)
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
        let rows = response
            .body
            .as_array()
            .context("listing is not an array")?;
        ensure!(rows.len() <= PAGE_SIZE, "oversized listing");
        let more = pagination(response.link.as_deref())?;
        ensure!(!more || !rows.is_empty(), "empty intermediate page");
        values.extend(rows.iter().cloned());
        if !more {
            ensure!(rows.len() < PAGE_SIZE, "listing may be truncated");
            return Ok(values);
        }
    }
    anyhow::bail!("listing exceeds {MAX_PAGES} pages")
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
) -> Result<String> {
    ensure!(network, "fork-network mismatch");
    ensure!(pull["state"] == "open", "pull is not open");
    repository(&pull["base"]["repo"], repo)?;
    ensure!(
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
    ensure!(
        (owner.eq_ignore_ascii_case(&config.bot) || head_repo.eq_ignore_ascii_case(repo))
            && (branch.starts_with("bot/") || branch.starts_with("dispatch/")),
        "not a bot-owned branch"
    );
    let head_metadata = get(client, &format!("repos/{head_repo}"))?;
    repository(&head_metadata, head_repo)?;
    ensure!(
        in_network(&head_metadata, upstream),
        "head fork-network mismatch"
    );
    let reference = format!("refs/heads/{branch}");
    let encoded: String = url::form_urlencoded::byte_serialize(reference.as_bytes()).collect();
    let activity = pages(
        client,
        &format!("repos/{head_repo}/activity?ref={encoded}&direction=desc"),
    )?;
    let head = pull["head"]["sha"].as_str().context("missing head")?;
    let pushed = push_time(&activity, &reference, head, now)?;
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
    ensure!(pull["draft"] == false, "draft");
    let labels = pull["labels"].as_array().context("missing labels")?;
    for label in labels {
        ensure!(label["name"].is_string(), "missing label name");
    }
    ensure!(
        !labels.iter().any(|l| l["name"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("needs-human"))),
        "needs-human"
    );
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
                command = Some((id, created, human_text));
            } else {
                stale = Some(format!(
                    "stale command=https://github.com/{repo}/pull/{number}#issuecomment-{id} at={} push={}",
                    created.format(&Rfc3339)?,
                    pushed.format(&Rfc3339)?
                ));
            }
        }
    }
    let (id, created, human_text) = command.with_context(|| {
        stale.unwrap_or_else(|| "no operator command after observed push".into())
    })?;
    let evidence = format!(
        "command=https://github.com/{repo}/pull/{number}#issuecomment-{id} at={} human-text={human_text} push={}",
        created.format(&Rfc3339)?,
        pushed.format(&Rfc3339)?
    );
    (|| {
        let mut previous = None;
        let mut ids = BTreeSet::new();
        for review in &reviews {
            let id = review["id"].as_u64().context("missing review id")?;
            ensure!(ids.insert(id), "duplicate review evidence");
            let author = review["user"]["login"]
                .as_str()
                .context("missing review author")?;
            let submitted = at(&review["submitted_at"])?;
            ensure!(
                submitted <= now && previous.is_none_or(|p| submitted >= p),
                "out-of-order reviews"
            );
            previous = Some(submitted);
            if !author.eq_ignore_ascii_case(&config.operator) {
                continue;
            }
            let state = review["state"].as_str().context("missing review state")?;
            // REST dates the original submission, not the dismissal. Without a
            // proved dismissal time no command can be proved newer than it.
            ensure!(
                state != "DISMISSED",
                "operator dismissal has no proved cancellation time"
            );
            ensure!(
                state != "CHANGES_REQUESTED" || submitted < created,
                "operator change request cancels command"
            );
            ensure!(
                matches!(state, "APPROVED" | "COMMENTED" | "CHANGES_REQUESTED"),
                "unknown operator review state"
            );
        }
        Ok(evidence.clone())
    })()
    .with_context(|| evidence)
}

/// Complete authenticated chain, including force-pushes back to an old oid.
fn push_time(
    events: &[Value],
    reference: &str,
    head: &str,
    now: OffsetDateTime,
) -> Result<OffsetDateTime> {
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
    at(&events[0]["timestamp"])
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

#[cfg(test)]
mod tests;
