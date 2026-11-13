//! First-party publication evidence, not authority recovered from prose.
use super::*;
use crate::pass::{ClosingPull, Facts, Promotion};
use serde::{Deserialize, Serialize};

pub(super) const SCHEMA: &str = "agent-board/promoted/v1";
pub(super) const FENCE: &str = "agent-board-promoted";
pub(super) const STATUS: &str = "<!-- agent-board-promote-status -->";

/// A confirmed publication: what the pass hands off, and what a publication
/// retry skips or updates without rebuilding the published head.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub(super) schema: String,
    pub(super) fork_repository: String,
    pub(super) fork_pr: u64,
    pub(super) approved_oid: String,
    pub(super) published_oid: String,
    pub(super) published_branch: String,
    pub(super) upstream_repository: String,
    pub(super) upstream_pr: u64,
    pub(super) source_repository: String,
    pub(super) source_issue: u64,
    pub(super) completes_source: bool,
}

pub(super) fn source(repo: &str, pull: &Value) -> Result<u64> {
    let body = pull["body"].as_str().context("missing source linkage")?;
    let issues: Vec<_> = referenced_numbers(body)
        .into_iter()
        .filter(|n| crate::stack::closes(body, repo, *n) || crate::stack::part_of(body, repo, *n))
        .collect();
    ensure!(
        issues.len() == 1,
        "handoff needs one deterministic source issue"
    );
    Ok(issues[0])
}

pub(super) fn success(
    repo: &str,
    pull: &Value,
    publication: &Value,
    upstream: &str,
    n: u64,
) -> Result<String> {
    let head = pull["head"]["sha"]
        .as_str()
        .context("missing approved head")?;
    let record = Record {
        schema: SCHEMA.into(),
        fork_repository: repo.into(),
        fork_pr: pull["number"].as_u64().context("missing fork number")?,
        approved_oid: head.into(),
        published_oid: publication["head"]["sha"]
            .as_str()
            .context("missing published head")?
            .into(),
        published_branch: publication["head"]["ref"]
            .as_str()
            .context("missing published branch")?
            .into(),
        upstream_repository: upstream.into(),
        upstream_pr: n,
        source_repository: repo.into(),
        source_issue: source(repo, pull)?,
        completes_source: crate::stack::closes(
            pull["body"].as_str().context("missing source linkage")?,
            repo,
            source(repo, pull)?,
        ),
    };
    Ok(format!(
        "Published approved head {head} as https://github.com/{upstream}/pull/{n}. Fork PR stays open; Status/scheduling uses verified upstream state.\n\n{STATUS}\n\n```{FENCE}\n{}\n```",
        serde_json::to_string_pretty(&record)?
    ))
}

/// Enrich the existing per-pass facts. No new store, client or mutation path.
/// Ambiguous source prose skips only that PR, with a diagnostic for the
/// supervisor. Receipt authentication and live identity failures still refuse
/// the read: unverified publication evidence is never authority.
pub(crate) fn enrich(
    client: &(impl Rest + Transport),
    config: &Config,
    items: &mut BTreeMap<String, Facts>,
) -> Result<Vec<String>> {
    let mut skipped = Vec::new();
    let mut unknown = BTreeSet::new();
    let Some(settings) = config.promote.as_ref().filter(|_| enabled(config)) else {
        return Ok(skipped);
    };
    for (repo, target) in &settings.forks {
        if !items
            .values()
            .any(|f| f.open && f.issue.repository.eq_ignore_ascii_case(repo))
        {
            continue;
        }
        let metadata = get(client, &format!("repos/{repo}"))?;
        repository(&metadata, repo)?;
        ensure!(
            observed_network(&metadata, &target.upstream)?,
            "handoff fork-network mismatch"
        );
        // Source timelines retain historical cross-references after fork
        // closure or body edits. Bound relevant evidence, not the fork's entire
        // PR history; an incomplete source timeline still fails closed.
        let mut candidates: BTreeMap<u64, BTreeSet<String>> = BTreeMap::new();
        for facts in items
            .values()
            .filter(|f| f.open && f.issue.repository.eq_ignore_ascii_case(repo))
        {
            for event in pages(
                client,
                &format!("repos/{repo}/issues/{}/timeline", facts.issue.number),
            )? {
                let issue = &event["source"]["issue"];
                if event["event"] != "cross-referenced" || !issue["pull_request"].is_object() {
                    continue;
                }
                let url = issue["html_url"]
                    .as_str()
                    .context("missing handoff cross-reference URL")?;
                let at = crate::parse_github_item(url, None, crate::github::DEFAULT_HOST)?;
                if at.repository.eq_ignore_ascii_case(repo) {
                    candidates
                        .entry(at.number)
                        .or_default()
                        .insert(crate::pass::key(&facts.issue));
                }
            }
        }
        let mut seen = BTreeSet::new();
        let mut sources = BTreeSet::new();
        for (n, linked) in candidates {
            let pull = get(client, &format!("repos/{repo}/pulls/{n}"))?;
            ensure!(pull["number"] == n, "handoff fork number mismatch");
            if !pull["user"]["login"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&config.bot))
            {
                continue;
            }
            // GitHub represents an absent description as null. It is no
            // source linkage, not a failure to read the board.
            let body = pull["body"].as_str().unwrap_or_default();
            let n = pull["number"]
                .as_u64()
                .filter(|n| *n > 0)
                .context("invalid handoff fork PR")?;
            let comments = pages(client, &format!("repos/{repo}/issues/{n}/comments"))?;
            let receipts: Vec<_> = comments
                .iter()
                .filter(|c| {
                    c["user"]["login"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(&config.bot))
                        && c["body"]
                            .as_str()
                            .is_some_and(|b| b.contains(STATUS) || b.contains(FENCE))
                })
                .collect();
            // Timeline candidates remain relevant even after a body edit
            // removes its links. Receipt content selects verification, never
            // authority.
            ensure!(receipts.len() <= 1, "duplicate handoff records");
            // Authenticate and decode a surviving receipt before looking up
            // the live body's source. Retargeting that body off-board must
            // not erase the original issue's handoff from the pass.
            let record = receipts
                .first()
                .map(|receipt| read_record(client, config, repo, n, receipt, comments.len()))
                .transpose()?;
            // Abandoned attempts have no publication evidence. A first-party
            // pending/legacy marker above must still verify or fail closed.
            if record.is_none() && pull["state"] == "closed" {
                continue;
            }
            let issue = match source(repo, &pull) {
                Ok(issue) => issue,
                Err(error) => {
                    // Missing prose cannot erase a surviving publication or
                    // turn a historical cross-reference into spare capacity.
                    // These facts authorize only a hold, never completion.
                    unknown.extend(linked);
                    if let Some(record) = &record {
                        let at = crate::parse_github_item(
                            &format!("{}#{}", record.source_repository, record.source_issue),
                            None,
                            crate::github::DEFAULT_HOST,
                        )?;
                        unknown.insert(crate::pass::key(&at));
                    }
                    skipped.push(format!(
                        "needs-supervisor: promotion handoff {repo}#{n}: {error:#}; skipped"
                    ));
                    continue;
                }
            };
            if let Some(record) = &record {
                ensure!(
                    record.schema == SCHEMA
                        && record.fork_repository == *repo
                        && record.fork_pr == n
                        && record.source_repository == *repo
                        && record.source_issue == issue
                        && record.completes_source == crate::stack::closes(body, repo, issue)
                        && record.upstream_repository == target.upstream
                        && record.upstream_pr > 0
                        && oid(&record.approved_oid)
                        && oid(&record.published_oid)
                        && pull["head"]["sha"] == record.approved_oid,
                    "handoff identity mismatch"
                );
            }
            let at = crate::parse_github_item(
                &format!("{repo}#{issue}"),
                None,
                crate::github::DEFAULT_HOST,
            )?;
            let Some(facts) = items.get_mut(&crate::pass::key(&at)) else {
                continue;
            };
            if !facts.open {
                continue;
            }
            repository(&pull["base"]["repo"], repo)?;
            let Some(record) = record else {
                ensure!(
                    seen.insert(n) && sources.insert(issue),
                    "conflicting handoff fork PRs"
                );
                ensure!(pull["state"] == "open", "closed fork has no handoff record");
                facts.promotion = Some(Promotion::Open(ClosingPull {
                    number: n,
                    repository: repo.clone(),
                    author: Some(config.bot.clone()),
                    updated: None,
                }));
                continue;
            };
            let head_repo = pull["head"]["repo"]["full_name"]
                .as_str()
                .context("missing handoff head repository")?;
            validate_repository(head_repo)?;
            validate_branch(&record.published_branch)?;
            validate_branch(
                pull["head"]["ref"]
                    .as_str()
                    .context("missing handoff branch")?,
            )?;
            let head_metadata = get(client, &format!("repos/{head_repo}"))?;
            repository(&head_metadata, head_repo)?;
            ensure!(
                observed_network(&head_metadata, &target.upstream)?,
                "handoff head-network mismatch"
            );
            let upstream = get(
                client,
                &format!("repos/{}/pulls/{}", target.upstream, record.upstream_pr),
            )?;
            repository(&upstream["base"]["repo"], &target.upstream)?;
            ensure!(
                upstream["number"] == record.upstream_pr
                    && upstream["head"]["sha"] == record.published_oid
                    && upstream["head"]["repo"]["full_name"] == head_repo
                    && upstream["head"]["ref"] == record.published_branch
                    && upstream["base"]["ref"] == target.base
                    && upstream["user"]["login"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(&config.bot))
                    && source(repo, &upstream)? == issue,
                "live upstream handoff identity mismatch"
            );
            ensure!(
                crate::stack::closes(
                    upstream["body"].as_str().context("missing upstream body")?,
                    repo,
                    issue
                ) == record.completes_source,
                "upstream handoff linkage kind mismatch"
            );
            let partial_merge = !record.completes_source
                && upstream["state"] == "closed"
                && upstream["merged"] == true;
            if !partial_merge {
                ensure!(
                    seen.insert(n) && sources.insert(issue),
                    "conflicting handoff fork PRs"
                );
            }
            match (upstream["state"].as_str(), upstream["merged"].as_bool()) {
                (Some("open"), Some(false)) if upstream.get("merged_at") == Some(&Value::Null) => {
                    // Verified publication remains active even after staleness or
                    // fork closure. None is deliberately not a stale timestamp.
                    facts.promotion = Some(Promotion::Open(ClosingPull {
                        number: record.upstream_pr,
                        repository: target.upstream.clone(),
                        author: Some(config.bot.clone()),
                        updated: None,
                    }));
                }
                (Some("closed"), Some(true)) if upstream["merged_at"].as_str().is_some() => {
                    super::at(&upstream["merged_at"])?;
                    if record.completes_source {
                        facts.promotion = Some(Promotion::Merged);
                    }
                }
                _ => anyhow::bail!("upstream handoff is closed-unmerged or incomplete"),
            }
        }
    }
    for at in unknown {
        if let Some(facts) = items.get_mut(&at) {
            facts.promotion = Some(Promotion::Unknown);
        }
    }
    Ok(skipped)
}

fn at_time(value: &Value, field: &str) -> Result<OffsetDateTime> {
    at(&value[field])
}

fn read_record(
    client: &impl Transport,
    config: &Config,
    repo: &str,
    n: u64,
    receipt: &Value,
    comments: usize,
) -> Result<Record> {
    authenticate(client, config, repo, n, receipt, comments)?;
    let body = receipt["body"].as_str().context("missing handoff body")?;
    decode(body)?.context("no machine handoff record (readable receipt is not authority)")
}

/// The bot wrote the receipt and nobody else edited it, by the forge's own
/// edit evidence for all `comments` of fork PR `n`. A REST login alone would
/// let a collaborator's edit of the bot's comment steer a retry.
pub(super) fn authenticate(
    client: &impl Transport,
    config: &Config,
    repo: &str,
    n: u64,
    receipt: &Value,
    comments: usize,
) -> Result<()> {
    let item = crate::parse_github_item(&format!("{repo}#{n}"), None, crate::github::DEFAULT_HOST)?;
    let metadata = crate::plan::comment_metadata(client, &item, true)?;
    ensure!(
        metadata.len() == comments,
        "incomplete handoff comment evidence"
    );
    let id = receipt["id"]
        .as_u64()
        .filter(|n| *n > 0)
        .context("invalid handoff comment id")?;
    let observed = metadata
        .iter()
        .find(|m| crate::plan::comment_id(&m.url) == Some(id))
        .context("missing handoff edit evidence")?;
    ensure!(
        observed.url == format!("https://github.com/{repo}/pull/{n}#issuecomment-{id}")
            && observed.created == at_time(receipt, "created_at")?
            && observed.changed == at_time(receipt, "updated_at")?
            && observed
                .author
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(&config.bot))
            && receipt["user"]["login"].as_str() == observed.author.as_deref()
            && if observed.edited {
                observed
                    .editor
                    .as_deref()
                    .is_some_and(|s| s.eq_ignore_ascii_case(&config.bot))
            } else {
                observed.editor.is_none()
            },
        "untrusted handoff author/edit evidence"
    );
    Ok(())
}

/// The canonical machine record in a receipt body, or none when the body has
/// no record fence. A fence that does not decode is an error, never absence.
pub(super) fn decode(body: &str) -> Result<Option<Record>> {
    let Some(value) = crate::plan::fenced_json_as(body, FENCE, "handoff record")? else {
        return Ok(None);
    };
    let record: Record = serde_json::from_value(value).context("invalid handoff record")?;
    canonical(body, FENCE, &record)?;
    Ok(Some(record))
}

/// `record` is `body`'s one `fence` block, byte for byte as the bot writes
/// it: a Value must not normalize duplicate keys into apparently valid authority.
pub(super) fn canonical(body: &str, fence: &str, record: &impl Serialize) -> Result<()> {
    let wire = format!("```{fence}\n{}\n```", serde_json::to_string_pretty(record)?);
    ensure!(
        body.contains(&wire) && body.matches(&format!("```{fence}")).count() == 1,
        "noncanonical {fence} record"
    );
    Ok(())
}
