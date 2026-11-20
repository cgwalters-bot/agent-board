//! Publication never moves the approved ref or closes a fork PR. The fork PR's
//! one bot status comment is the receipt a retry reads first: a confirmed
//! publication is skipped or text-updated without rebuilding its head, a
//! partial one resumes from the branch it already pushed, and a candidate
//! that holds never stops the candidates after it.
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::handoff::STATUS;
const STEP_WRITES: usize = 3;
/// The pending receipt's machine record: the head a retry resumes.
const PROMOTING_SCHEMA: &str = "agent-board/promoting/v1";
const PROMOTING_FENCE: &str = "agent-board-promoting";

/// Written before the first upstream write. The publication branch is named
/// for the approved head alone, so a rebuild on a later upstream base would
/// be a different head on the same name: a retry publishes this one instead.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Promoting {
    schema: String,
    approved_oid: String,
    published_oid: String,
    published_branch: String,
    /// Preserve the existing PR and exact old branch head across partial repairs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous: Option<handoff::Record>,
}

/// What the candidate's receipt records, once the bot is known to have
/// written it and nobody else to have edited it.
enum Receipt {
    Confirmed(handoff::Record),
    Pending(Promoting),
    /// A readable status without a machine record; a new attempt replaces it.
    Opaque,
}

/// One candidate's reads decided, before any write.
enum Step {
    /// A report line; the next candidate proceeds.
    Line(String),
    /// The cap cannot cover this candidate's step; the invocation ends.
    Held(String),
    Publish(Box<Prepared>),
}

/// The head and branch to publish, and the upstream PR already on that branch.
struct Publication {
    published: String,
    branch: String,
    /// The upstream base the head was rebuilt on this invocation; push the
    /// branch before publishing. None resumes a branch already pushed.
    base: Option<String>,
    existing: Option<Value>,
    previous: Option<handoff::Record>,
}

struct Prepared {
    fresh: Value,
    /// The head the operator approved; approval and the receipt name it.
    head: String,
    /// `fresh` with the published head and branch.
    publication: Value,
    what: Publication,
    /// The status comment to edit to pending first, else one is created.
    receipt: Option<u64>,
}

/// Explicit caller permission plus --apply; at most one publication and four
/// writes per invocation. The caller's normal transport supplies credentials.
pub fn apply<C: Rest + Transport>(
    client: &C,
    config: &Config,
    now: OffsetDateTime,
    max_writes: usize,
) -> Result<Vec<String>> {
    apply_with(
        client,
        config,
        now,
        max_writes,
        &mut rewrite::Git::default(),
    )
}

pub(super) fn apply_with<C: Rest + Transport>(
    client: &C,
    config: &Config,
    now: OffsetDateTime,
    max_writes: usize,
    git: &mut impl rewrite::Rewrite,
) -> Result<Vec<String>> {
    let Some(settings) = config.promote.as_ref().filter(|_| enabled(config)) else {
        return Ok(vec!["promote: off".into()]);
    };
    ensure!(settings.publish, "promote.publish must be true for --apply");
    ensure!(
        config.forgejo_board().is_none(),
        "promote supports GitHub only"
    );
    ensure!(
        !config.bot.eq_ignore_ascii_case(&config.operator),
        "promote operator and bot must differ"
    );
    pass::promotion_reads(client, config, |client, lines| {
        // Status comments must remain recoverable with the same authenticated
        // writer, not merely an assumed author from trusted configuration.
        let identity = get(client, "user")?;
        ensure!(
            identity["login"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&config.bot)),
            "publication credential must authenticate as configured bot"
        );
        let mut held = Vec::new();
        let mut published = false;
        'forks: for (repo, target) in &targets(client, config)? {
            // Reuse the pass's bounded hint query: ordinary waiting drafts
            // spend no per-PR reads. Hints never replace fresh authorization.
            let candidates = awaiting::candidates(client, config, repo).with_context(|| {
                format!("discover promotion candidates in {repo}; later forks not evaluated")
            })?;
            if candidates.is_empty() {
                continue;
            }
            let mut pulls = pages(
                client,
                &format!("repos/{repo}/pulls?state=open&sort=created&direction=asc"),
            )
            .with_context(|| {
                format!("list promotion candidates in {repo}; later forks not evaluated")
            })?;
            pulls.sort_by_key(|p| p["number"].as_u64());
            let mut seen = BTreeSet::new();
            for pull in &pulls {
                let number = pull["number"]
                    .as_u64()
                    .filter(|n| *n > 0)
                    .context("invalid pull number")?;
                ensure!(seen.insert(number), "duplicate pull {repo}#{number}");
            }
            let pulls: Vec<_> = pulls
                .into_iter()
                .filter(|p| {
                    p["number"]
                        .as_u64()
                        .is_some_and(|n| candidates.contains(&n))
                })
                .collect();
            for (index, pull) in pulls.iter().enumerate() {
                let number = pull["number"].as_u64().context("invalid pull number")?;
                let pending = || {
                    format!(
                        "{repo}#{number}; {} candidate(s) in this fork and later forks not evaluated",
                        candidates.len() - index
                    )
                };
                // A candidate that cannot be published holds alone, before any
                // write; the candidates after it are still read this invocation.
                let prepared =
                    match prepare(client, config, (repo, target), pull, now, max_writes, git) {
                        Ok(Step::Publish(prepared)) => prepared,
                        Ok(Step::Line(line)) => {
                            lines.push(line);
                            continue;
                        }
                        Ok(Step::Held(line)) => {
                            lines.push(line);
                            break 'forks;
                        }
                        Err(error) if pass::budget_spent(&error) => {
                            return Err(error.context(pending()));
                        }
                        Err(error) => {
                            if error.is::<ScopeFailure>() {
                                return Err(error);
                            }
                            let why = format!("{error:#}");
                            lines.push(format!("hold promote {repo}#{number}: {why}"));
                            let head = pull["head"]["sha"].as_str().filter(|h| oid(h));
                            if let Some(head) =
                                head.filter(|_| settings.hold_comments && holds(&error))
                            {
                                held.push((repo.clone(), number, head.to_owned()));
                            }
                            continue;
                        }
                    };
                lines.push(publish(client, (repo, target), git, *prepared).with_context(pending)?);
                published = true;
                break 'forks;
            }
        }
        if settings.hold_comments && !published {
            hold_comments(client, config, &held, max_writes, lines)?;
        }
        Ok(())
    })
}

/// Only the validated head and fixed public text go on the fork. Diagnostics
/// can include private policy data and belong in the promotion log only.
fn hold_body(head: &str) -> String {
    format!(
        "{HOLD_COMMENT}{head} -->\n\nA promote run held head {head} because a promotion requirement was not met. See the promote job log for details."
    )
}

/// Only the exact diagnostic format is editable as a hold. A marker prefixed
/// to a receipt must not hide it or let a diagnostic overwrite it.
fn is_hold_body(body: &str) -> bool {
    body.lines()
        .next()
        .and_then(|line| line.strip_prefix(HOLD_COMMENT)?.strip_suffix(" -->"))
        .is_some_and(|head| oid(head) && body == hold_body(head))
}

/// With `promote.holdComments`, says on each fork PR in `held` that its
/// promotion holds: one bot comment, posted once and edited only when its
/// text changes, at most `max_writes` of them. A run that published wrote
/// its writes already and leaves these to the next.
fn hold_comments<C: Rest>(
    client: &C,
    config: &Config,
    held: &[(String, u64, String)],
    max_writes: usize,
    lines: &mut Vec<String>,
) -> Result<()> {
    let mut writes = 0;
    for (repo, number, head) in held {
        let body = hold_body(head);
        let comments = match pages(client, &format!("repos/{repo}/issues/{number}/comments")) {
            Ok(comments) => comments,
            Err(error) if pass::budget_spent(&error) => return Err(error),
            Err(error) => {
                lines.push(format!("hold comment {repo}#{number}: {error:#}"));
                continue;
            }
        };
        let existing = comments.iter().rev().find(|c| {
            c["user"]["login"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&config.bot))
                && c["body"].as_str().is_some_and(is_hold_body)
        });
        if existing.is_some_and(|c| c["body"] == body.as_str()) {
            continue;
        }
        if writes >= max_writes {
            lines.push(format!(
                "hold comment {repo}#{number}: held by --max-writes"
            ));
            continue;
        }
        // One with no id cannot be edited; a second would not be one comment.
        let id = existing.map(|c| c["id"].as_u64().filter(|id| *id > 0));
        if id == Some(None) {
            lines.push(format!(
                "hold comment {repo}#{number}: its comment has no id"
            ));
            continue;
        }
        // Leave room for this comment, a fresh /promote command and the
        // publication receipt. REST rejects an exactly full final page.
        if id.is_none() && comments.len() + 3 >= PAGE_SIZE {
            lines.push(format!(
                "hold comment {repo}#{number}: no room in comment evidence window"
            ));
            continue;
        }
        writes += 1;
        let (method, path, done) = match id.flatten() {
            Some(id) => (
                "PATCH",
                format!("repos/{repo}/issues/comments/{id}"),
                "updated",
            ),
            None => (
                "POST",
                format!("repos/{repo}/issues/{number}/comments"),
                "posted",
            ),
        };
        match send(client, method, &path, json!({"body": body})) {
            Ok(_) => lines.push(format!("hold comment {repo}#{number}: {done}")),
            Err(error) if pass::budget_spent(&error) => return Err(error),
            Err(error) => lines.push(format!("hold comment {repo}#{number}: {error:#}")),
        }
    }
    Ok(())
}

/// Everything read before the first write: revalidation, the receipt, and
/// the head to publish. Only a rebuilt head costs a Git run.
fn prepare<C: Rest + Transport>(
    client: &C,
    config: &Config,
    (repo, target): (&str, &pass::promote::Fork),
    pull: &Value,
    now: OffsetDateTime,
    max_writes: usize,
    git: &mut impl rewrite::Rewrite,
) -> Result<Step> {
    let number = pull["number"].as_u64().context("invalid pull number")?;
    let command = validate(client, config, repo, target, pull, now)?;
    // No mutation until the *entire* evidence chain has been read again,
    // including uncached policy and PR-specific provenance.
    let fresh = get(client, &format!("repos/{repo}/pulls/{number}"))?;
    for key in [
        "number",
        "state",
        "draft",
        "title",
        "body",
        "head",
        "base",
        "user",
        "labels",
        "updated_at",
    ] {
        ensure!(
            fresh[key] == pull[key],
            "fork PR {key} changed before publication; retry with fresh evidence"
        );
    }
    let current = validate(client, config, repo, target, &fresh, now)?;
    ensure!(
        current == command,
        "operator command changed before publication"
    );
    let body = authorized_promotion_body(client, config, repo, &fresh, &target.upstream, &command)?;
    let head = command.head;
    let owner = fresh["head"]["repo"]["owner"]["login"]
        .as_str()
        .context("missing head owner")?;
    let original_branch = fresh["head"]["ref"]
        .as_str()
        .context("missing head branch")?;
    let head_repo = fresh["head"]["repo"]["full_name"]
        .as_str()
        .context("missing head repository")?;
    // The receipt first: what an earlier invocation published for this
    // approved head is finished or skipped, never rebuilt on today's base.
    let comments = pages(client, &format!("repos/{repo}/issues/{number}/comments"))?;
    let receipts: Vec<_> = comments
        .iter()
        .filter(|c| {
            c["user"]["login"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&config.bot))
                && c["body"].as_str().is_some_and(|b| b.contains(STATUS))
        })
        .collect();
    ensure!(receipts.len() <= 1, "multiple promote status comments");
    let receipt = receipts
        .first()
        .map(|r| {
            r["id"]
                .as_u64()
                .filter(|n| *n > 0)
                .context("invalid promote status comment id")
        })
        .transpose()?;
    let recorded = match receipts.first() {
        Some(r) => read_receipt(client, config, repo, number, r, comments.len())?,
        None => Receipt::Opaque,
    };
    let branch_of = |branch: &str| {
        ensure!(
            branch == original_branch || publication_branch(branch),
            "publication receipt names another branch"
        );
        Ok(())
    };
    let mut confirmed = false;
    let what = match recorded {
        Receipt::Confirmed(record) if record.approved_oid == head => {
            ensure!(
                record.schema == handoff::SCHEMA
                    && record.fork_repository == repo
                    && record.fork_pr == number
                    && record.upstream_repository == target.upstream
                    && record.upstream_pr > 0
                    && oid(&record.published_oid),
                "publication receipt names another publication"
            );
            branch_of(&record.published_branch)?;
            let existing =
                upstream_pulls(client, &target.upstream, owner, &record.published_branch)?;
            ensure!(existing.len() <= 1, "multiple upstream pull requests");
            let pr = existing
                .into_iter()
                .find(|p| p["number"] == record.upstream_pr)
                .context("recorded upstream PR is not on its publication branch")?;
            confirmed = true;
            Publication {
                published: record.published_oid,
                branch: record.published_branch,
                base: None,
                existing: Some(pr),
                previous: None,
            }
        }
        Receipt::Confirmed(record) => {
            match repair(
                client,
                config,
                (repo, target),
                &fresh,
                &head,
                &record,
                None,
                max_writes,
                git,
            )? {
                Ok(what) => what,
                Err(needed) => return Ok(Step::Held(held(repo, number, needed))),
            }
        }
        Receipt::Pending(record) if record.previous.is_some() => {
            ensure!(
                record.approved_oid == head,
                "unfinished repair publication needs its approved head restored"
            );
            let previous = record
                .previous
                .as_ref()
                .context("missing prior publication")?;
            match repair(
                client,
                config,
                (repo, target),
                &fresh,
                &head,
                previous,
                Some(&record),
                max_writes,
                git,
            )? {
                Ok(what) => what,
                Err(needed) => return Ok(Step::Held(held(repo, number, needed))),
            }
        }
        Receipt::Pending(record) if record.approved_oid == head => {
            ensure!(
                oid(&record.published_oid),
                "publication receipt names an invalid head"
            );
            branch_of(&record.published_branch)?;
            match ref_head(client, head_repo, &record.published_branch)? {
                Some(sha) if sha == record.published_oid => {
                    // Pushed; the upstream PR may or may not have followed. One
                    // opened from the original branch meanwhile holds, as for
                    // a rebuild: never a second upstream PR for one head.
                    ensure!(
                        record.published_branch == original_branch
                            || upstream_pulls(client, &target.upstream, owner, original_branch)?
                                .is_empty(),
                        "existing upstream PR needs manual migration before rewriting"
                    );
                    let existing =
                        upstream_pulls(client, &target.upstream, owner, &record.published_branch)?;
                    ensure!(existing.len() <= 1, "multiple upstream pull requests");
                    Publication {
                        published: record.published_oid,
                        branch: record.published_branch,
                        base: None,
                        existing: existing.into_iter().next(),
                        previous: None,
                    }
                }
                Some(_) => anyhow::bail!(
                    "publication branch {} exists at another head; delete it to publish again",
                    record.published_branch
                ),
                // Nothing was pushed: publish again, under a new receipt.
                None => {
                    match rebuild(client, config, target, &fresh, &head, max_writes, git, None)? {
                        Ok(what) => what,
                        Err(needed) => return Ok(Step::Held(held(repo, number, needed))),
                    }
                }
            }
        }
        _ => match rebuild(client, config, target, &fresh, &head, max_writes, git, None)? {
            Ok(what) => what,
            Err(needed) => return Ok(Step::Held(held(repo, number, needed))),
        },
    };
    let mut publication = fresh.clone();
    publication["body"] = json!(body);
    publication["head"]["sha"] = json!(what.published);
    publication["head"]["ref"] = json!(what.branch);
    if let Some(pr) = &what.existing {
        verify_pr(
            pr,
            what.previous
                .as_ref()
                .map_or(what.published.as_str(), |r| r.published_oid.as_str()),
            &publication,
            &target.upstream,
            &target.base,
        )?;
        if confirmed && pr["title"] == publication["title"] && pr["body"] == publication["body"] {
            // Confirmed and unchanged: no writes, and no rebuild on a later
            // upstream base that would disagree with the open upstream PR.
            return Ok(Step::Line(format!(
                "already promoted {repo}#{number} -> {}#{} head={head}",
                target.upstream,
                pr["number"].as_u64().context("invalid upstream number")?
            )));
        }
    }
    let needed = STEP_WRITES + usize::from(what.base.is_some());
    if max_writes < needed {
        return Ok(Step::Held(held(repo, number, needed)));
    }
    Ok(Step::Publish(Box::new(Prepared {
        fresh,
        head,
        publication,
        what,
        receipt,
    })))
}

fn held(repo: &str, number: u64, needed: usize) -> String {
    format!("held by --max-writes: promote {repo}#{number} (needs {needed})")
}

fn publication_branch(branch: &str) -> bool {
    branch.strip_prefix("bot/promote/").is_some_and(oid)
}

/// Renew publication only after `validate` has authorized the repaired current
/// head. The authenticated old receipt selects the PR, never a new head's branch.
#[allow(clippy::too_many_arguments)]
fn repair<C: Rest + Transport>(
    client: &C,
    config: &Config,
    (repo, target): (&str, &pass::promote::Fork),
    fresh: &Value,
    head: &str,
    previous: &handoff::Record,
    pending: Option<&Promoting>,
    max_writes: usize,
    git: &mut impl rewrite::Rewrite,
) -> Result<std::result::Result<Publication, usize>> {
    handoff::verify_record(repo, target, fresh, previous)?;
    let original = fresh["head"]["ref"]
        .as_str()
        .context("missing approved branch")?;
    ensure!(
        previous.published_branch == original || publication_branch(&previous.published_branch),
        "prior publication names another branch"
    );
    let mut observed = previous.clone();
    if previous.published_branch == original {
        // This ref already moved with the fork fix; never force-update it.
        observed.published_oid = head.into();
    } else if let Some(pending) = pending {
        ensure!(
            pending.published_branch == previous.published_branch,
            "repair changed publication branch"
        );
        let head_repo = fresh["head"]["repo"]["full_name"]
            .as_str()
            .context("missing head repository")?;
        if ref_head(client, head_repo, &previous.published_branch)?.as_deref()
            == Some(&pending.published_oid)
        {
            observed.published_oid = pending.published_oid.clone();
        }
    }
    let pr = handoff::verify_upstream(client, &config.bot, repo, target, fresh, &observed)?;
    ensure!(
        pr["state"] == "open" && pr["merged"] == false,
        "repair publication is no longer open"
    );
    if let Some(pending) = pending.filter(|p| p.published_oid == observed.published_oid) {
        return Ok(Ok(Publication {
            published: pending.published_oid.clone(),
            branch: pending.published_branch.clone(),
            base: None,
            existing: Some(pr),
            previous: Some(observed),
        }));
    }
    let result = rebuild(
        client,
        config,
        target,
        fresh,
        head,
        max_writes,
        git,
        Some(&observed),
    )?;
    if let Ok(what) = &result {
        ensure!(
            what.branch == previous.published_branch
                && what
                    .existing
                    .as_ref()
                    .is_some_and(|p| p["number"] == previous.upstream_pr),
            "repair must update the existing upstream publication"
        );
    }
    Ok(result)
}

/// Publish the approved head from its own branch, or rebuild it on the
/// current upstream base. `Err` is the cap the step needs and lacks, found
/// before any Git run.
#[allow(clippy::too_many_arguments)]
fn rebuild<C: Rest + Transport>(
    client: &C,
    config: &Config,
    target: &pass::promote::Fork,
    fresh: &Value,
    head: &str,
    max_writes: usize,
    git: &mut impl rewrite::Rewrite,
    previous: Option<&handoff::Record>,
) -> Result<std::result::Result<Publication, usize>> {
    let owner = fresh["head"]["repo"]["owner"]["login"]
        .as_str()
        .context("missing head owner")?;
    let original_branch = fresh["head"]["ref"]
        .as_str()
        .context("missing head branch")?;
    let head_repo = fresh["head"]["repo"]["full_name"]
        .as_str()
        .context("missing head repository")?;
    let mut compare = comparison(
        client,
        &format!(
            "repos/{}/compare/{}...{}",
            target.upstream,
            encode(&target.base),
            encode(&format!("{owner}:{original_branch}"))
        ),
        head,
    )?;
    compare["base_commit"] = get(
        client,
        &format!(
            "repos/{}/branches/{}",
            target.upstream,
            encode(&target.base)
        ),
    )?["commit"]
        .clone();
    let (_, _, missing) = signoff_gate(config, &compare, head)?;
    let rewriting = missing > 0
        || compare["behind_by"].as_u64().context("missing behind_by")? > 0
        || previous.is_some_and(|r| r.published_branch != original_branch);
    let needed = STEP_WRITES + usize::from(rewriting);
    if max_writes < needed {
        return Ok(Err(needed));
    }
    let (published, branch, base) = if rewriting {
        ensure!(
            upstream_pulls(client, &target.upstream, owner, original_branch)?.is_empty(),
            "existing upstream PR needs manual migration before rewriting"
        );
        let base = compare["base_commit"]["sha"]
            .as_str()
            .filter(|s| oid(s))
            .context("invalid publication base")?
            .to_owned();
        let (published, branch) = git.prepare(config, target, fresh, &compare)?;
        let branch = previous.map_or(branch, |r| r.published_branch.clone());
        // The branch name is the approved head's. One from an earlier base,
        // or a partial publication without a receipt, holds here, not after
        // the first write; the ref is never force-updated.
        if let Some(sha) = ref_head(client, head_repo, &branch)? {
            ensure!(
                sha == published || previous.is_some_and(|r| sha == r.published_oid),
                "publication branch {branch} exists at another head (an earlier upstream base?); delete it to publish again"
            );
        }
        (published, branch, Some(base))
    } else {
        (head.to_owned(), original_branch.to_owned(), None)
    };
    let existing = upstream_pulls(client, &target.upstream, owner, &branch)?;
    ensure!(existing.len() <= 1, "multiple upstream pull requests");
    Ok(Ok(Publication {
        published,
        branch,
        base,
        existing: existing.into_iter().next(),
        previous: previous.cloned().filter(|_| rewriting),
    }))
}

/// The receipt's record. One with a machine record steers a retry, so the
/// forge's edit evidence must show the bot alone wrote and edited it.
fn read_receipt(
    client: &impl Transport,
    config: &Config,
    repo: &str,
    number: u64,
    receipt: &Value,
    comments: usize,
) -> Result<Receipt> {
    let body = receipt["body"]
        .as_str()
        .context("missing promote status body")?;
    let pending = decode_pending(body)?;
    let recorded = match (handoff::decode(body)?, pending) {
        (None, None) => return Ok(Receipt::Opaque),
        (Some(record), None) => Receipt::Confirmed(record),
        (None, Some(record)) => Receipt::Pending(record),
        (Some(_), Some(_)) => anyhow::bail!("promote status comment holds two records"),
    };
    handoff::authenticate(client, config, repo, number, receipt, comments)?;
    Ok(recorded)
}

fn decode_pending(body: &str) -> Result<Option<Promoting>> {
    let Some(value) = crate::plan::fenced_json_as(body, PROMOTING_FENCE, "promotion record")?
    else {
        return Ok(None);
    };
    let record: Promoting = serde_json::from_value(value).context("invalid promotion record")?;
    handoff::canonical(body, PROMOTING_FENCE, &record)?;
    ensure!(
        record.schema == PROMOTING_SCHEMA
            && oid(&record.approved_oid)
            && oid(&record.published_oid),
        "promotion record identity mismatch"
    );
    validate_branch(&record.published_branch)?;
    Ok(Some(record))
}

/// Pending publication is a hold, never permission to fall back to fork CI.
pub(super) fn pending(body: &str) -> Result<bool> {
    Ok(decode_pending(body)?.is_some())
}

/// A branch's head in `repo`, or none when the branch does not exist.
fn ref_head(client: &impl Rest, repo: &str, branch: &str) -> Result<Option<String>> {
    validate_branch(branch)?;
    match get(
        client,
        &format!("repos/{repo}/git/ref/heads/{}", encode(branch)),
    ) {
        Ok(reference) => Ok(Some(
            reference["object"]["sha"]
                .as_str()
                .filter(|s| oid(s))
                .context("invalid branch head")?
                .to_owned(),
        )),
        Err(error) if error.is::<crate::github::NotFound>() => Ok(None),
        Err(error) => Err(error),
    }
}

/// The write step: pending receipt, branch push where rebuilt, the upstream
/// PR, then the success receipt. Its writes are the invocation's one slot,
/// so a failure here ends the invocation with the receipt saying why.
fn publish<C: Rest + Transport>(
    client: &C,
    (repo, target): (&str, &pass::promote::Fork),
    git: &mut impl rewrite::Rewrite,
    prepared: Prepared,
) -> Result<String> {
    let Prepared {
        fresh,
        head,
        publication,
        what,
        receipt,
    } = prepared;
    let number = fresh["number"].as_u64().context("invalid pull number")?;
    let owner = fresh["head"]["repo"]["owner"]["login"]
        .as_str()
        .context("missing head owner")?;
    let head_repo = fresh["head"]["repo"]["full_name"]
        .as_str()
        .context("missing head repository")?;
    let record = format!(
        "```{PROMOTING_FENCE}\n{}\n```",
        serde_json::to_string_pretty(&Promoting {
            schema: PROMOTING_SCHEMA.into(),
            approved_oid: head.clone(),
            published_oid: what.published.clone(),
            published_branch: what.branch.clone(),
            previous: what.previous.clone(),
        })?
    );
    let pending = format!(
        "Promoting approved head {head} to {}.\n\n{STATUS}\n\n{record}",
        target.upstream
    );
    let status_id = if let Some(id) = receipt {
        send(
            client,
            "PATCH",
            &format!("repos/{repo}/issues/comments/{id}"),
            json!({"body":pending}),
        )?;
        id
    } else {
        let receipt = send(
            client,
            "POST",
            &format!("repos/{repo}/issues/{number}/comments"),
            json!({"body":pending}),
        )?;
        receipt["id"]
            .as_u64()
            .filter(|n| *n > 0)
            .context("missing promote status comment id")?
    };
    // Recheck the approved branch at the publication boundary.
    // Rewrites only create a new ref; they never move this one.
    let result = (|| {
        let reference = get(
            client,
            &format!(
                "repos/{head_repo}/git/ref/heads/{}",
                encode(
                    fresh["head"]["ref"]
                        .as_str()
                        .context("missing approved branch")?
                )
            ),
        )?;
        ensure!(
            reference["object"]["sha"] == head,
            "branch moved before publication"
        );
        if let Some(base) = &what.base {
            let live = get(
                client,
                &format!(
                    "repos/{}/branches/{}",
                    target.upstream,
                    encode(&target.base)
                ),
            )?;
            ensure!(
                live["commit"]["sha"] == *base,
                "upstream base moved before publication; retry"
            );
            git.publish(
                head_repo,
                &what.branch,
                &what.published,
                what.previous.as_ref().map(|r| r.published_oid.as_str()),
            )?;
        }
        let reference = get(
            client,
            &format!("repos/{head_repo}/git/ref/heads/{}", encode(&what.branch)),
        )?;
        ensure!(
            reference["object"]["sha"] == what.published,
            "publication branch moved"
        );
        let text = json!({"title":publication["title"], "body":publication["body"]});
        let published = if let Some(pr) = &what.existing {
            let n = pr["number"]
                .as_u64()
                .filter(|n| *n > 0)
                .context("invalid upstream number")?;
            send(
                client,
                "PATCH",
                &format!("repos/{}/pulls/{n}", target.upstream),
                text,
            )?
        } else {
            let mut payload = text;
            payload["head"] = json!(format!("{owner}:{}", what.branch));
            payload["base"] = json!(target.base);
            // A mutable branch can move after our last read. Creating
            // drafts avoids marking an unapproved raced head ready.
            // There is deliberately no ready-for-review mutation here.
            payload["draft"] = json!(true);
            // Pushing to the bot fork's branch would let every upstream
            // writer edit its workflows and reach its secrets; say no
            // explicitly rather than rely on the forge's default.
            payload["maintainer_can_modify"] = json!(false);
            send(
                client,
                "POST",
                &format!("repos/{}/pulls", target.upstream),
                payload,
            )?
        };
        verify_pr(
            &published,
            &what.published,
            &publication,
            &target.upstream,
            &target.base,
        )?;
        if let Some(pr) = &what.existing {
            ensure!(
                published["number"] == pr["number"],
                "upstream returned a different PR number"
            );
        }
        ensure!(
            published["title"] == publication["title"] && published["body"] == publication["body"],
            "upstream returned different text"
        );
        let n = published["number"]
            .as_u64()
            .filter(|n| *n > 0)
            .context("invalid upstream number")?;
        let url = format!("https://github.com/{}/pull/{n}", target.upstream);
        Ok::<_, anyhow::Error>((url, n))
    })();
    let (url, upstream_number) = match result {
        Ok(url) => url,
        Err(error) => {
            // The record stays: the next invocation resumes this head.
            let body = format!(
                "Publication of approved head {head} was not confirmed: {error:#}. Re-run to observe upstream before retrying.\n\n{STATUS}\n\n{record}"
            );
            send(
                client,
                "PATCH",
                &format!("repos/{repo}/issues/comments/{status_id}"),
                json!({"body":body}),
            )?;
            if error.is::<Held>() {
                return Ok(format!(
                    "hold promote {repo}#{number}: {error:#}; publication head={} retained for retry",
                    what.published
                ));
            }
            return Err(error);
        }
    };
    let mut body = handoff::success(
        repo,
        &fresh,
        &publication,
        &target.upstream,
        upstream_number,
    )?;
    if what.published != head {
        body.push_str(&format!(
            "\n\nOperator approval of {head} carried over to publication head {} after a clean replay; no new review was requested. The upstream PR's CI checks the publication head; fork CI does not gate promotion.",
            what.published
        ));
    }
    send(
        client,
        "PATCH",
        &format!("repos/{repo}/issues/comments/{status_id}"),
        json!({"body":body}),
    )?;
    Ok(format!("promoted {repo}#{number} -> {url} head={head}"))
}

fn validate(
    client: &(impl Rest + Transport),
    config: &Config,
    repo: &str,
    target: &pass::promote::Fork,
    pull: &Value,
    now: OffsetDateTime,
) -> Result<Command> {
    let metadata = get(client, &format!("repos/{repo}"))?;
    let legacy = config.promote.as_ref().and_then(|settings| {
        settings
            .forks
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(repo))
            .map(|(_, target)| target)
    });
    let observed = super::target(&metadata, repo, legacy)?
        .context("repository no longer has midstream metadata")?;
    ensure!(
        observed.upstream == target.upstream && observed.base == target.base,
        "promotion target changed"
    );
    let number = pull["number"].as_u64().context("invalid pull number")?;
    let (command, reviews) = publication_command(
        client,
        config,
        (repo, number),
        pull,
        (in_network(&metadata, &target.upstream), &target.upstream),
        now,
    )?;
    let gate = gates_impl(
        client,
        config,
        repo,
        pull,
        &target.upstream,
        &target.base,
        false,
    )?;
    let Gates::Ready { .. } = gate else {
        anyhow::bail!("publication gates incomplete")
    };
    match policy::gate(client, config, &target.upstream)? {
        policy::Permitted::BotOk => {}
        policy::Permitted::HumanText => provenance::verify(client, config, repo, pull, &command)?,
    }
    // Trigger selection can read a draft timeline after the reviews. Do not
    // publish if approval or command cancellation evidence changed meanwhile.
    ensure!(
        pages(client, &format!("repos/{repo}/pulls/{number}/reviews"))? == reviews,
        "review evidence changed before publication"
    );
    Ok(command)
}

/// The upstream PR is open at the published head, from the fork's head
/// repository on the published branch, against the configured base.
fn verify_pr(pr: &Value, head: &str, fork: &Value, upstream: &str, base: &str) -> Result<()> {
    ensure!(
        pr["state"] == "open"
            && pr["head"]["sha"] == head
            && pr["number"].as_u64().is_some_and(|n| n > 0)
            && pr["head"]["ref"] == fork["head"]["ref"]
            && pr["head"]["repo"]["owner"]["login"] == fork["head"]["repo"]["owner"]["login"]
            && pr["head"]["repo"]["full_name"] == fork["head"]["repo"]["full_name"]
            && pr["base"]["ref"] == base,
        "upstream PR is not open at the published head from the fork's branch, or has another identity"
    );
    repository(&pr["base"]["repo"], upstream)?;
    Ok(())
}

pub(super) fn send(client: &impl Rest, method: &str, path: &str, body: Value) -> Result<Value> {
    let (status, response) = client.send(method, &format!("{API}{path}"), &body)?;
    ensure!(
        (200..300).contains(&status),
        "{method} {path}: HTTP {status}: {response}"
    );
    Ok(response)
}
