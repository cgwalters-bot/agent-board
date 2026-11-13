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
/// Invalid evidence refuses only that PR, never authorizes a promotion, and
/// holds its source for the supervisor. Failed API reads still refuse the read.
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
        let mut claims: BTreeMap<String, Vec<(u64, Promotion)>> = BTreeMap::new();
        for (n, linked) in candidates {
            let mut affected = linked;
            match enrich_pull(client, config, repo, target, n, items, &mut affected) {
                Ok(Some((at, promotion))) => claims.entry(at).or_default().push((n, promotion)),
                Ok(None) => {}
                Err(error) if error.is::<ReadFailure>() => return Err(error),
                Err(error) => {
                    unknown.extend(affected);
                    skipped.push(notice(repo, n, &format!("{error:#}")));
                }
            }
        }
        for (at, mut promotions) in claims {
            if promotions.len() > 1 {
                unknown.insert(at);
                for (n, _) in promotions {
                    skipped.push(notice(repo, n, "conflicting handoff fork PRs"));
                }
            } else if let Some((_, promotion)) = promotions.pop()
                && let Some(facts) = items.get_mut(&at)
            {
                facts.promotion = Some(promotion);
            }
        }
    }
    for at in unknown {
        if let Some(facts) = items.get_mut(&at) {
            facts.promotion = Some(Promotion::Unknown);
        }
    }
    skipped.sort();
    Ok(skipped)
}

fn notice(repo: &str, n: u64, reason: &str) -> String {
    format!("needs-supervisor: promotion handoff {repo}#{n}: {reason}; skipped")
}

// Keep request failures distinguishable from invalid per-PR evidence, including
// requests made inside receipt authentication. Never swallow an unavailable API.
#[derive(Debug)]
struct ReadFailure;

impl std::fmt::Display for ReadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("reading handoff API")
    }
}

impl std::error::Error for ReadFailure {}

struct HandoffRead<'a, T>(&'a T);

impl<T: Rest> Rest for HandoffRead<'_, T> {
    fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
        anyhow::bail!("handoff enrichment is read-only")
    }

    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<crate::poll::RestResponse> {
        let response = self.0.get(url, validator).context(ReadFailure)?;
        if response.status != 200 {
            return Err(anyhow::anyhow!("GET {url}: HTTP {}", response.status))
                .context(ReadFailure);
        }
        Ok(response)
    }
}

impl<T: Transport> Transport for HandoffRead<'_, T> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.0.graphql(query, variables).context(ReadFailure)
    }
}

fn enrich_pull(
    client: &(impl Rest + Transport),
    config: &Config,
    repo: &str,
    target: &crate::pass::promote::Fork,
    n: u64,
    items: &BTreeMap<String, Facts>,
    affected: &mut BTreeSet<String>,
) -> Result<Option<(String, Promotion)>> {
    let client = &HandoffRead(client);
    let pull = get(client, &format!("repos/{repo}/pulls/{n}"))?;
    ensure!(pull["number"] == n, "handoff fork number mismatch");
    if !pull["user"]["login"]
        .as_str()
        .is_some_and(|s| s.eq_ignore_ascii_case(&config.bot))
    {
        return Ok(None);
    }
    // GitHub represents an absent description as null. It is no
    // source linkage, not a failure to read the board.
    let body = pull["body"].as_str().unwrap_or_default();
    if let Ok(issue) = source(repo, &pull) {
        affected.insert(crate::pass::key(&crate::parse_github_item(
            &format!("{repo}#{issue}"),
            None,
            crate::github::DEFAULT_HOST,
        )?));
    }
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
    if let Some(record) = &record {
        let at = crate::parse_github_item(
            &format!("{}#{}", record.source_repository, record.source_issue),
            None,
            crate::github::DEFAULT_HOST,
        )?;
        affected.insert(crate::pass::key(&at));
    }
    // Abandoned attempts have no publication evidence. A first-party
    // pending/legacy marker above must still verify or fail closed.
    if record.is_none() && pull["state"] == "closed" {
        return Ok(None);
    }
    let issue = source(repo, &pull)?;
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
    let at = crate::pass::key(&at);
    let Some(facts) = items.get(&at) else {
        return Ok(None);
    };
    if !facts.open {
        return Ok(None);
    }
    repository(&pull["base"]["repo"], repo)?;
    let Some(record) = record else {
        ensure!(pull["state"] == "open", "closed fork has no handoff record");
        return Ok(Some((
            at,
            Promotion::Open(ClosingPull {
                number: n,
                repository: repo.into(),
                author: Some(config.bot.clone()),
                updated: None,
            }),
        )));
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
    let partial_merge =
        !record.completes_source && upstream["state"] == "closed" && upstream["merged"] == true;
    match (upstream["state"].as_str(), upstream["merged"].as_bool()) {
        (Some("open"), Some(false)) if upstream.get("merged_at") == Some(&Value::Null) => {
            // Verified publication remains active even after staleness or
            // fork closure. None is deliberately not a stale timestamp.
            Ok(Some((
                at,
                Promotion::Open(ClosingPull {
                    number: record.upstream_pr,
                    repository: target.upstream.clone(),
                    author: Some(config.bot.clone()),
                    updated: None,
                }),
            )))
        }
        (Some("closed"), Some(true)) if upstream["merged_at"].as_str().is_some() => {
            super::at(&upstream["merged_at"])?;
            Ok((!partial_merge).then_some((at, Promotion::Merged)))
        }
        _ => anyhow::bail!("upstream handoff is closed-unmerged or incomplete"),
    }
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
