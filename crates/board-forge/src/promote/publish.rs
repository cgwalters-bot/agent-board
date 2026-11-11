//! Publication never moves the approved ref or closes a fork PR. A retry observes the
//! upstream branch PR before writing, including after a lost POST response.
use super::*;
use serde_json::json;

const STATUS: &str = "<!-- agent-board-promote-status -->";
const STEP_WRITES: usize = 3;

/// Explicit caller permission plus --apply; at most one candidate and four
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
    pass::preview_reads(client, |client| {
        // Status comments must remain recoverable with the same authenticated
        // writer, not merely an assumed author from trusted configuration.
        let identity = get(client, "user")?;
        ensure!(
            identity["login"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&config.bot)),
            "publication credential must authenticate as configured bot"
        );
        let mut lines = Vec::new();
        for (repo, target) in &settings.forks {
            let mut pulls = pages(
                client,
                &format!("repos/{repo}/pulls?state=open&sort=created&direction=asc"),
            )?;
            pulls.sort_by_key(|p| p["number"].as_u64());
            for pull in pulls {
                let number = pull["number"]
                    .as_u64()
                    .filter(|n| *n > 0)
                    .context("invalid pull number")?;
                let before = validate(client, config, repo, target, &pull, now);
                let command = match before {
                    Ok(command) => command,
                    Err(error) => {
                        lines.push(format!("hold promote {repo}#{number}: {error:#}"));
                        continue;
                    }
                };
                // No mutation until the *entire* evidence chain has been read
                // again, including uncached policy and PR-specific provenance.
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
                let mut publication_pull = fresh.clone();
                let mut publication_command = Command {
                    head: command.head.clone(),
                    ..current
                };
                let owner = fresh["head"]["repo"]["owner"]["login"]
                    .as_str()
                    .context("missing head owner")?;
                let original_branch = fresh["head"]["ref"]
                    .as_str()
                    .context("missing head branch")?;
                let mut compare = comparison(
                    client,
                    &format!(
                        "repos/{}/compare/{}...{}",
                        target.upstream,
                        encode(&target.base),
                        encode(&format!("{owner}:{original_branch}"))
                    ),
                    &command.head,
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
                let (_, _, missing) = signoff_gate(config, &compare, &command.head)?;
                let rewriting =
                    missing > 0 || compare["behind_by"].as_u64().context("missing behind_by")? > 0;
                let needed = STEP_WRITES + usize::from(rewriting);
                if max_writes < needed {
                    lines.push(format!(
                        "held by --max-writes: promote {repo}#{number} (needs {needed})"
                    ));
                    return Ok(lines);
                }
                if rewriting {
                    ensure!(
                        upstream_pulls(client, &target.upstream, owner, original_branch)?
                            .is_empty(),
                        "existing upstream PR needs manual migration before rewriting"
                    );
                    let prepared = match git.prepare(config, target, &fresh, &compare) {
                        Ok(prepared) => prepared,
                        Err(error) => {
                            lines.push(format!("hold promote {repo}#{number}: {error:#}"));
                            return Ok(lines);
                        }
                    };
                    publication_pull["head"]["sha"] = json!(prepared.0);
                    publication_pull["head"]["ref"] = json!(prepared.1);
                    publication_command.head = publication_pull["head"]["sha"]
                        .as_str()
                        .context("missing prepared head")?
                        .into();
                }
                let owner = publication_pull["head"]["repo"]["owner"]["login"]
                    .as_str()
                    .context("missing head owner")?;
                let branch = publication_pull["head"]["ref"]
                    .as_str()
                    .context("missing head branch")?;
                let existing = upstream_pulls(client, &target.upstream, owner, branch)?;
                ensure!(existing.len() <= 1, "multiple upstream pull requests");
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
                if let Some(pr) = existing.first() {
                    verify_pr(
                        pr,
                        &publication_command,
                        &publication_pull,
                        &target.upstream,
                        &target.base,
                    )?;
                    let url = format!(
                        "https://github.com/{}/pull/{}",
                        target.upstream,
                        pr["number"].as_u64().context("invalid upstream number")?
                    );
                    if pr["title"] == fresh["title"]
                        && pr["body"] == fresh["body"]
                        && receipts
                            .first()
                            .is_some_and(|r| r["body"] == status_body(&command, &url))
                    {
                        // Already confirmed and unchanged: don't consume this
                        // invocation's single publication slot indefinitely.
                        continue;
                    }
                }
                let pending = format!(
                    "Promoting approved head {} to {}.\n\n{STATUS}",
                    command.head, target.upstream
                );
                let status_id = if let Some(receipt) = receipts.first() {
                    let id = receipt["id"]
                        .as_u64()
                        .filter(|n| *n > 0)
                        .context("invalid promote status comment id")?;
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
                let head_repo = fresh["head"]["repo"]["full_name"]
                    .as_str()
                    .context("missing head repository")?;
                let publication = (|| {
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
                        reference["object"]["sha"] == command.head,
                        "branch moved before publication"
                    );
                    if rewriting {
                        let base = get(
                            client,
                            &format!(
                                "repos/{}/branches/{}",
                                target.upstream,
                                encode(&target.base)
                            ),
                        )?;
                        ensure!(
                            base["commit"]["sha"] == compare["base_commit"]["sha"],
                            "upstream base moved before publication; retry"
                        );
                        git.publish(head_repo, branch, &publication_command.head)?;
                    }
                    let reference = get(
                        client,
                        &format!("repos/{head_repo}/git/ref/heads/{}", encode(branch)),
                    )?;
                    ensure!(
                        reference["object"]["sha"] == publication_command.head,
                        "publication branch moved"
                    );
                    let text = json!({"title":fresh["title"], "body":fresh["body"]});
                    let published = if let Some(pr) = existing.first() {
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
                        payload["head"] = json!(format!("{owner}:{branch}"));
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
                        &publication_command,
                        &publication_pull,
                        &target.upstream,
                        &target.base,
                    )?;
                    if let Some(pr) = existing.first() {
                        ensure!(
                            published["number"] == pr["number"],
                            "upstream returned a different PR number"
                        );
                    }
                    ensure!(
                        published["title"] == fresh["title"] && published["body"] == fresh["body"],
                        "upstream returned different text"
                    );
                    let n = published["number"]
                        .as_u64()
                        .filter(|n| *n > 0)
                        .context("invalid upstream number")?;
                    let url = format!("https://github.com/{}/pull/{n}", target.upstream);
                    Ok::<_, anyhow::Error>(url)
                })();
                let url = match publication {
                    Ok(url) => url,
                    Err(error) => {
                        let body = format!(
                            "Publication of approved head {} was not confirmed: {error:#}. Re-run to observe upstream before retrying.\n\n{STATUS}",
                            command.head
                        );
                        send(
                            client,
                            "PATCH",
                            &format!("repos/{repo}/issues/comments/{status_id}"),
                            json!({"body":body}),
                        )?;
                        return Err(error);
                    }
                };
                let body = status_body(&command, &url);
                send(
                    client,
                    "PATCH",
                    &format!("repos/{repo}/issues/comments/{status_id}"),
                    json!({"body":body}),
                )?;
                lines.push(format!(
                    "promoted {repo}#{number} -> {url} head={}",
                    command.head
                ));
                return Ok(lines);
            }
        }
        Ok(lines)
    })
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
    repository(&metadata, repo)?;
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
    let decision = reviews.iter().rev().find(|r| {
        r["user"]["login"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(&config.operator))
            && r["state"] != "COMMENTED"
    });
    ensure!(
        decision.is_some_and(|r| r["state"] == "APPROVED" && r["commit_id"] == command.head),
        "operator has not approved the current head"
    );
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
    Ok(command)
}

fn status_body(command: &Command, url: &str) -> String {
    format!(
        "Published approved head {} as {url}. Fork PR stays open pending Status/scheduling handoff.\n\n{STATUS}",
        command.head
    )
}

fn verify_pr(
    pr: &Value,
    command: &Command,
    fork: &Value,
    upstream: &str,
    base: &str,
) -> Result<()> {
    ensure!(
        pr["state"] == "open"
            && pr["head"]["sha"] == command.head
            && pr["number"].as_u64().is_some_and(|n| n > 0)
            && pr["head"]["ref"] == fork["head"]["ref"]
            && pr["head"]["repo"]["owner"]["login"] == fork["head"]["repo"]["owner"]["login"]
            && pr["head"]["repo"]["full_name"] == fork["head"]["repo"]["full_name"]
            && pr["base"]["ref"] == base,
        "upstream returned an unapproved head or unexpected PR identity"
    );
    repository(&pr["base"]["repo"], upstream)?;
    Ok(())
}

fn send(client: &impl Rest, method: &str, path: &str, body: Value) -> Result<Value> {
    let (status, response) = client.send(method, &format!("{API}{path}"), &body)?;
    ensure!(
        (200..300).contains(&status),
        "{method} {path}: HTTP {status}: {response}"
    );
    Ok(response)
}
