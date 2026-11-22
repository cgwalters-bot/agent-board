//! Publication never moves the approved ref or closes a fork PR. The fork PR's
//! one bot status comment is the receipt a retry reads first: a confirmed
//! publication is skipped or text-updated without rebuilding its head, and a
//! partial one resumes from the branch it already pushed. Validation computes
//! the PR's own commits once ([`rewrite::Commits`]); replay, the receipts and
//! every confirmation use the suffix that remains after the upstream comparison.
//! A receipt records those commits and the
//! upstream commit they were validated or replayed on, and a publication is
//! resumed, confirmed or skipped only while it records exactly the approved
//! commits and upstream still contains that commit; an older receipt without
//! that record only for a direct publication the current comparison proves,
//! or, once confirmed, one whose trees match a replay of the approved commits.
//! A candidate that holds never stops the candidates after it.
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
    /// Absent on older receipts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cut: Option<rewrite::Cut>,
    /// Preserve the existing PR and exact old branch head across partial repairs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous: Option<handoff::Record>,
}

/// The upstream commit a recorded publication sits on, once its record shows
/// it carries exactly the approved commits and upstream still contains it.
/// Returns the upstream tip that contains it.
fn recorded_onto(
    client: &impl Rest,
    target: &pass::promote::Fork,
    cut: Option<&rewrite::Cut>,
    commits: &rewrite::Commits,
) -> Result<(String, String)> {
    let cut = cut.filter(|cut| commits.recorded(cut) && oid(&cut.onto));
    hold!(
        cut.is_some(),
        "publication does not record the approved commits; inspect and migrate it manually"
    );
    let onto = cut.context("missing publication cut")?.onto.clone();
    let tip = tip_containing(client, target, &onto)?;
    Ok((onto, tip))
}

/// Bound the Git replay used to validate a confirmed receipt without a cut.
const MIGRATION_COMMITS: usize = 10;

/// A confirmed receipt written before receipts recorded a cut: rebuild it from
/// upstream's comparison of the published head. The publication's commits must
/// form one chain on an upstream commit. Return its base and immutable commit
/// IDs for Git to check against a replay.
fn migration_range(
    client: &impl Rest,
    target: &pass::promote::Fork,
    record: &handoff::Record,
    range: &Range,
) -> Result<(String, Vec<String>)> {
    const HOLD: &str = "confirmed publication does not record the approved commits";
    let own = range.commits.shas();
    hold!(
        own.len() <= MIGRATION_COMMITS,
        "{HOLD}, and its {} commits exceed the {MIGRATION_COMMITS} a migration compares; inspect and migrate it manually",
        own.len()
    );
    let published = &record.published_oid;
    let compare = comparison(
        client,
        &format!(
            "repos/{}/compare/{}...{published}",
            target.upstream, range.base
        ),
        published,
    )?;
    ensure!(
        compare["base_commit"]["sha"] == range.base.as_str(),
        "publication comparison base mismatch"
    );
    let onto = compare["merge_base_commit"]["sha"]
        .as_str()
        .context("missing publication merge base")?;
    let rows = compare["commits"]
        .as_array()
        .context("missing publication commits")?;
    hold!(
        rows.len() == own.len(),
        "{HOLD}: it carries {} commits beyond upstream, not the PR's {}; inspect and migrate it manually",
        rows.len(),
        own.len()
    );
    let mut parent = onto;
    let mut shas = Vec::with_capacity(rows.len());
    for row in rows {
        let sha = row["sha"].as_str().context("invalid publication commit")?;
        hold!(
            row["parents"]
                .as_array()
                .is_some_and(|p| p.len() == 1 && p[0]["sha"] == parent),
            "{HOLD}, and its commits are not one chain on upstream; inspect and migrate it manually"
        );
        shas.push(sha.to_owned());
        parent = sha;
    }
    Ok((onto.to_owned(), shas))
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
    /// A legacy check is optional; try it after all regular candidates.
    Legacy,
    Publish(Box<Prepared>),
}

/// The head and branch to publish, and the upstream PR already on that branch.
struct Publication {
    published: String,
    branch: String,
    /// The upstream tip that must still be current at the write boundary.
    base: Option<String>,
    /// The upstream commit on which the PR's commits were validated or
    /// replayed. Upstream must still contain it when the PR write returns,
    /// or the publication stays unconfirmed.
    onto: String,
    rewrite: Option<Rewritten>,
    existing: Option<Value>,
    previous: Option<handoff::Record>,
}

struct Rewritten {
    push: bool,
}

/// The PR's own commits, computed once by the first validation, and the
/// upstream facts of the latest one.
struct Range {
    commits: rewrite::Commits,
    behind: u64,
    /// Publishing the approved head itself would carry exactly `commits`.
    direct: bool,
    /// The upstream tip compared.
    base: String,
}

struct Prepared {
    fresh: Value,
    /// The head the operator approved; approval and the receipt name it.
    head: String,
    /// When `head` was first pushed: the fork base must not move after it.
    introduced: OffsetDateTime,
    /// What the receipts record of the publication.
    cut: rewrite::Cut,
    /// `fresh` with the published head and branch.
    publication: Value,
    what: Publication,
    confirmed: bool,
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
        let targets = targets(client, config)?;
        let mut legacy = Vec::new();
        for (repo, target) in &targets {
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
            for (index, pull) in pulls.into_iter().enumerate() {
                let number = pull["number"].as_u64().context("invalid pull number")?;
                let pending = || {
                    format!(
                        "{repo}#{number}; {} candidate(s) in this fork and later forks not evaluated",
                        candidates.len() - index
                    )
                };
                // A candidate that cannot be published holds alone, before any
                // write; the candidates after it are still read this invocation.
                let prepared = match prepare(
                    client,
                    config,
                    (repo, target),
                    (&pull, false),
                    now,
                    max_writes,
                    git,
                ) {
                    Ok(Step::Publish(prepared)) => prepared,
                    Ok(Step::Legacy) => {
                        legacy.push((repo, target, pull));
                        continue;
                    }
                    Ok(Step::Line(line)) => {
                        lines.push(line);
                        continue;
                    }
                    Ok(Step::Held(line)) => {
                        lines.push(line);
                        return Ok(());
                    }
                    Err(error) if pass::budget_spent(&error) => {
                        return Err(error.context(pending()));
                    }
                    Err(error) => {
                        if error.is::<ScopeFailure>() {
                            return Err(error);
                        }
                        lines.push(format!("hold promote {repo}#{number}: {error:#}"));
                        continue;
                    }
                };
                lines.push(publish(client, (repo, target), git, *prepared).with_context(pending)?);
                return Ok(());
            }
        }
        // Only use budget left over after normal publications, across all
        // forks. Revalidate the legacy candidate rather than reusing evidence
        // gathered before the rest of the scan.
        for (repo, target, pull) in legacy {
            let number = pull["number"].as_u64().context("invalid pull number")?;
            let context = || format!("legacy receipt for {repo}#{number}");
            match prepare(
                client,
                config,
                (repo, target),
                (&pull, true),
                now,
                max_writes,
                git,
            ) {
                Ok(Step::Publish(prepared)) => {
                    lines.push(
                        publish(client, (repo, target), git, *prepared).with_context(context)?,
                    );
                    return Ok(());
                }
                Ok(Step::Line(line)) => lines.push(line),
                Ok(Step::Held(line)) => {
                    lines.push(line);
                    return Ok(());
                }
                Ok(Step::Legacy) => anyhow::bail!("legacy candidate deferred twice"),
                Err(error) if pass::budget_spent(&error) || error.is::<ScopeFailure>() => {
                    return Err(error.context(context()));
                }
                Err(error) => lines.push(format!("hold promote {repo}#{number}: {error:#}")),
            }
        }
        Ok(())
    })
}

/// Everything read before the first write: revalidation, the receipt, and
/// the head to publish. Only a rebuilt head costs a Git run.
fn prepare<C: Rest + Transport>(
    client: &C,
    config: &Config,
    (repo, target): (&str, &pass::promote::Fork),
    (pull, legacy_only): (&Value, bool),
    now: OffsetDateTime,
    max_writes: usize,
    git: &mut impl rewrite::Rewrite,
) -> Result<Step> {
    let number = pull["number"].as_u64().context("invalid pull number")?;
    let (command, first) = validate(client, config, repo, target, pull, now, None)?;
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
    // Do not reread the fork range. Check the unchanged fork base and head,
    // then select the suffix against the fresh upstream comparison.
    let (current, range) = validate(
        client,
        config,
        repo,
        target,
        &fresh,
        now,
        Some(first.commits),
    )?;
    ensure!(
        current == command,
        "operator authorization changed before publication"
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
    let legacy = matches!(&recorded, Receipt::Confirmed(record)
        if record.approved_oid == head && record.cut.is_none()
            && !(range.direct && record.published_oid == head));
    if legacy && !legacy_only {
        return Ok(Step::Legacy);
    }
    hold!(
        !legacy_only || legacy,
        "legacy publication evidence changed; retry"
    );
    let what = match recorded {
        Receipt::Confirmed(record) if record.approved_oid == head => {
            handoff::verify_record(repo, target, &fresh, &record)?;
            branch_of(&record.published_branch)?;
            // The same head may carry other commits under another fork base,
            // and an upstream rewind adds old upstream commits to a published
            // range. An older receipt records neither; prove a direct cut by
            // comparison or verify the rewritten trees in the deferred scan.
            let onto = if record.cut.is_some() {
                recorded_onto(client, target, record.cut.as_ref(), &range.commits)?.0
            } else if range.direct && record.published_oid == head {
                range.base.clone()
            } else {
                let (onto, published) = migration_range(client, target, &record, &range)?;
                git.verify_replay(config, target, &fresh, &range.commits, &onto, &published)?;
                onto
            };
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
                base: if legacy {
                    let tip = tip_containing(client, target, &onto)?;
                    // Containment alone is not enough: even a fast-forward
                    // can integrate an approved commit and change the suffix.
                    hold!(
                        tip == range.base,
                        "upstream base moved during legacy verification; retry"
                    );
                    Some(tip)
                } else {
                    None
                },
                onto,
                rewrite: None,
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
                &range,
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
            hold!(
                record.published_branch != original_branch || range.direct,
                "pending direct repair carries fork-only commits; manual migration required"
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
                &range,
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
            let resume = if record.published_branch == original_branch {
                // An old pending direct receipt is not authority to create a PR.
                // Recover only one that is already there, and only if the head
                // carries exactly the PR's commits, as any record says.
                if range.direct
                    && record.published_oid == head
                    && record
                        .cut
                        .as_ref()
                        .is_none_or(|cut| range.commits.recorded(cut))
                {
                    let existing =
                        upstream_pulls(client, &target.upstream, owner, original_branch)?;
                    ensure!(existing.len() <= 1, "multiple upstream pull requests");
                    existing
                        .into_iter()
                        .next()
                        .map(|pr| {
                            ensure!(
                                pr["user"]["login"]
                                    .as_str()
                                    .is_some_and(|login| login.eq_ignore_ascii_case(&config.bot)),
                                "existing direct publication is not bot-authored"
                            );
                            Ok(Publication {
                                published: head.clone(),
                                branch: original_branch.into(),
                                base: None,
                                // The comparison that proved no fork-only
                                // commits; an upstream rewind can undo that.
                                onto: range.base.clone(),
                                rewrite: None,
                                existing: Some(pr),
                                previous: None,
                            })
                        })
                        .transpose()?
                } else {
                    None
                }
            } else {
                match ref_head(client, head_repo, &record.published_branch)? {
                    Some(sha) if sha == record.published_oid => {
                        let (onto, tip) =
                            recorded_onto(client, target, record.cut.as_ref(), &range.commits)?;
                        // Pushed; the upstream PR may or may not have followed. One
                        // opened from the original branch meanwhile holds, as for
                        // a rebuild: never a second upstream PR for one head.
                        ensure!(
                            upstream_pulls(client, &target.upstream, owner, original_branch)?
                                .is_empty(),
                            "existing upstream PR needs manual migration before rewriting"
                        );
                        let existing = upstream_pulls(
                            client,
                            &target.upstream,
                            owner,
                            &record.published_branch,
                        )?;
                        ensure!(existing.len() <= 1, "multiple upstream pull requests");
                        Some(Publication {
                            published: record.published_oid,
                            branch: record.published_branch,
                            base: Some(tip),
                            onto,
                            rewrite: Some(Rewritten { push: false }),
                            existing: existing.into_iter().next(),
                            previous: None,
                        })
                    }
                    Some(_) => anyhow::bail!(
                        "publication branch {} exists at another head; delete it to publish again",
                        record.published_branch
                    ),
                    // Nothing was pushed: publish again, under a new receipt.
                    None => None,
                }
            };
            match resume {
                Some(what) => what,
                None => {
                    match rebuild(
                        client, config, target, &fresh, &head, &range, max_writes, git, None,
                    )? {
                        Ok(what) => what,
                        Err(needed) => return Ok(Step::Held(held(repo, number, needed))),
                    }
                }
            }
        }
        recorded => {
            if let Receipt::Pending(record) = &recorded
                && record.published_branch != original_branch
            {
                // An earlier head's unfinished rewrite may have opened its
                // upstream PR before the receipt was confirmed. A rebuild for
                // this head checks only its own branches and would open another.
                branch_of(&record.published_branch)?;
                ensure!(
                    upstream_pulls(client, &target.upstream, owner, &record.published_branch)?
                        .is_empty(),
                    "unfinished publication of earlier head {} has an upstream PR on {}; migrate it manually",
                    record.approved_oid,
                    record.published_branch
                );
            }
            match rebuild(
                client, config, target, &fresh, &head, &range, max_writes, git, None,
            )? {
                Ok(what) => what,
                Err(needed) => return Ok(Step::Held(held(repo, number, needed))),
            }
        }
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
    let needed = if confirmed {
        1
    } else {
        STEP_WRITES + usize::from(what.rewrite.as_ref().is_some_and(|r| r.push))
    };
    if max_writes < needed {
        return Ok(Step::Held(held(repo, number, needed)));
    }
    Ok(Step::Publish(Box::new(Prepared {
        fresh,
        head,
        introduced: command.introduced,
        cut: range.commits.cut(&what.onto),
        publication,
        what,
        confirmed,
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
    range: &Range,
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
        let (base, onto, rewrite) = if pending.published_branch != original {
            let (onto, tip) = recorded_onto(client, target, pending.cut.as_ref(), &range.commits)?;
            (Some(tip), onto, Some(Rewritten { push: false }))
        } else {
            hold!(
                pending
                    .cut
                    .as_ref()
                    .is_none_or(|cut| range.commits.recorded(cut)),
                "publication does not record the approved commits; inspect and migrate it manually"
            );
            (None, range.base.clone(), None)
        };
        return Ok(Ok(Publication {
            published: pending.published_oid.clone(),
            branch: pending.published_branch.clone(),
            base,
            onto,
            rewrite,
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
        range,
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
    range: &Range,
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
    let (_, _, missing) = signoff_gate(config, &range.commits)?;
    let rewriting = missing > 0
        || range.behind > 0
        || !range.direct
        || previous.is_some_and(|r| r.published_branch != original_branch);
    let needed = STEP_WRITES + usize::from(rewriting);
    if max_writes < needed {
        return Ok(Err(needed));
    }
    let (published, branch) = if rewriting {
        ensure!(
            upstream_pulls(client, &target.upstream, owner, original_branch)?.is_empty(),
            "existing upstream PR needs manual migration before rewriting"
        );
        let (published, branch) =
            git.prepare(config, target, fresh, &range.commits, &range.base)?;
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
        (published, branch)
    } else {
        (head.to_owned(), original_branch.to_owned())
    };
    let existing = upstream_pulls(client, &target.upstream, owner, &branch)?;
    ensure!(existing.len() <= 1, "multiple upstream pull requests");
    Ok(Ok(Publication {
        published,
        branch,
        base: Some(range.base.clone()),
        onto: range.base.clone(),
        rewrite: rewriting.then_some(Rewritten { push: true }),
        existing: existing.into_iter().next(),
        previous: previous.cloned().filter(|_| rewriting),
    }))
}

fn upstream_tip(client: &impl Rest, target: &pass::promote::Fork) -> Result<String> {
    branch_tip(client, &target.upstream, &target.base)
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
        introduced,
        cut,
        publication,
        what,
        confirmed,
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
            cut: Some(cut.clone()),
            previous: what.previous.clone(),
        })?
    );
    let pending = format!(
        "Promoting approved head {head} to {}.\n\n{STATUS}\n\n{record}",
        target.upstream
    );
    let status_id = if confirmed {
        // A text-only update cannot change the confirmed publication's
        // identity. Keep its receipt intact across either outcome of PATCH.
        ensure!(what.existing.is_some(), "missing confirmed upstream PR");
        None
    } else if let Some(id) = receipt {
        send(
            client,
            "PATCH",
            &format!("repos/{repo}/issues/comments/{id}"),
            json!({"body":pending}),
        )?;
        Some(id)
    } else {
        let receipt = send(
            client,
            "POST",
            &format!("repos/{repo}/issues/{number}/comments"),
            json!({"body":pending}),
        )?;
        Some(
            receipt["id"]
                .as_u64()
                .filter(|n| *n > 0)
                .context("missing promote status comment id")?,
        )
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
        let check_fork = || -> Result<()> {
            hold!(
                base_settled(client, repo, &fresh, introduced)? == cut.fork_base,
                "fork base tip changed before publication; retry with fresh evidence"
            );
            Ok(())
        };
        let check_upstream = || -> Result<()> {
            if let Some(base) = &what.base {
                hold!(
                    upstream_tip(client, target)? == *base,
                    "upstream base moved before publication; retry"
                );
            }
            Ok(())
        };
        check_fork()?;
        let push = what.rewrite.as_ref().is_some_and(|r| r.push);
        if push {
            check_upstream()?;
            git.publish(
                head_repo,
                &what.branch,
                &what.published,
                what.previous.as_ref().map(|r| r.published_oid.as_str()),
            )?;
            check_fork()?;
        }
        let reference = get(
            client,
            &format!("repos/{head_repo}/git/ref/heads/{}", encode(&what.branch)),
        )?;
        ensure!(
            reference["object"]["sha"] == what.published,
            "publication branch moved"
        );
        check_upstream()?;
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
        // A successful PR write is not proof the range's base still belongs
        // to upstream. Leave the receipt pending if upstream rewound during
        // the write, on a first attempt as on a retry.
        tip_containing(client, target, &what.onto)?;
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
            let Some(status_id) = status_id else {
                // The confirmed receipt stays as it was.
                if error.is::<Held>() {
                    return Ok(format!("hold promote {repo}#{number}: {error:#}"));
                }
                return Err(error);
            };
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
    let Some(status_id) = status_id else {
        return Ok(format!("promoted {repo}#{number} -> {url} head={head}"));
    };
    let mut body = handoff::success(
        repo,
        &fresh,
        &publication,
        &target.upstream,
        upstream_number,
        Some(&cut),
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
    prior: Option<rewrite::Commits>,
) -> Result<(Command, Range)> {
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
    let command = evaluate(
        client,
        config,
        (repo, number),
        pull,
        (in_network(&metadata, &target.upstream), &target.upstream),
        now,
    )?;
    // An authenticated command alone must not ready a head the operator never
    // approved. A later COMMENTED review does not cancel a deciding review.
    let reviews = pages(client, &format!("repos/{repo}/pulls/{number}/reviews"))?;
    reviews
        .iter()
        .rev()
        .find(|r| {
            r["user"]["login"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&config.operator))
                && r["state"] != "COMMENTED"
        })
        .filter(|r| r["state"] == "APPROVED" && r["commit_id"] == command.head)
        .context("operator has not approved the current head")?;
    // The same cut preview reports: fixed before any review of the head.
    let gate = gates_impl(
        client,
        config,
        repo,
        pull,
        (&target.upstream, &target.base),
        command.introduced,
        Mode::Publication(prior),
    )?;
    let Gates::Ready {
        behind,
        direct,
        tip,
        commits,
        ..
    } = gate
    else {
        anyhow::bail!("publication gates incomplete")
    };
    match policy::gate(client, config, &target.upstream)? {
        policy::Permitted::BotOk => {}
        policy::Permitted::HumanText => provenance::verify(client, config, repo, pull, &command)?,
    }
    let range = Range {
        commits,
        behind,
        direct,
        base: tip.context("unpinned publication base")?,
    };
    Ok((command, range))
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
