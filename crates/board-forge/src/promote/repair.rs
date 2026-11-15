//! One-time repair of open midstream drafts written before agent-board#1002,
//! whose bodies tie them to their source with a bare `#N` and forge-internal
//! references ([`text::repair`]). Preview by default; `--apply` edits only
//! the body of the bot's own open drafts in verified promotion forks, at most
//! `max_writes` of them, and never anything upstream. Running it again finds
//! nothing to do: a repaired body is [`text::Repair::Unchanged`].
use super::*;
use serde_json::json;
use text::Repair;

/// The pull request fields that must be unchanged between the listing a
/// repair was decided on and the read just before its write.
const IDENTITY: [&str; 6] = ["number", "state", "draft", "title", "body", "user"];

/// Preview, or with `max_writes` apply, the repair in every verified
/// midstream fork. Reads use the promotion request/time budget.
pub fn repair<C: Rest + Transport>(
    client: &C,
    config: &Config,
    max_writes: Option<usize>,
) -> Result<Vec<String>> {
    if !enabled(config) {
        return Ok(vec!["promote: off".into()]);
    }
    ensure!(
        config.forgejo_board().is_none(),
        "promote supports GitHub only"
    );
    pass::promotion_reads(client, config, |client, lines| {
        if max_writes.is_some() {
            // The bot owns the drafts; any other writer would be editing
            // someone else's text.
            let identity = get(client, "user")?;
            ensure!(
                identity["login"]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case(&config.bot)),
                "repair credential must authenticate as configured bot"
            );
        }
        let mut writes = 0;
        for (repo, target) in &targets(client, config)? {
            let mut pulls = pages(
                client,
                &format!("repos/{repo}/pulls?state=open&sort=created&direction=asc"),
            )
            .with_context(|| format!("list drafts in {repo}; later forks not repaired"))?;
            pulls.sort_by_key(|p| p["number"].as_u64());
            for pull in &pulls {
                let number = pull["number"]
                    .as_u64()
                    .filter(|n| *n > 0)
                    .context("invalid pull number")?;
                if !own_draft(config, repo, pull) {
                    continue;
                }
                let decided = decide(repo, &target.upstream, pull).unwrap_or_else(|error| {
                    // Each draft stands alone; the next is still read.
                    Repair::Skip(format!("{error:#}"))
                });
                let line = match decided {
                    Repair::Unchanged => format!("repair {repo}#{number}: unchanged"),
                    Repair::Skip(why) => {
                        format!("repair {repo}#{number}: held for manual repair: {why}")
                    }
                    Repair::Rewrite(after) => {
                        let change = change(pull, &after)?;
                        match max_writes {
                            None => format!("would repair {repo}#{number}: {change}"),
                            Some(max) if writes >= max => {
                                format!("held by --max-writes: repair {repo}#{number}: {change}")
                            }
                            Some(_) => {
                                writes += 1;
                                match write(client, config, (repo, &target.upstream), pull, &after)
                                {
                                    Ok(()) => format!("repaired {repo}#{number}: {change}"),
                                    Err(error) if pass::budget_spent(&error) => return Err(error),
                                    // Each draft stands alone; the next is still read.
                                    Err(error) => format!("repair failed: {error:#}"),
                                }
                            }
                        }
                    }
                };
                lines.push(line);
            }
        }
        Ok(())
    })
}

/// The bot's own open draft whose base is the fork itself.
fn own_draft(config: &Config, repo: &str, pull: &Value) -> bool {
    pull["state"] == "open"
        && pull["draft"] == true
        && pull["user"]["login"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(&config.bot))
        && pull["base"]["repo"]["full_name"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(repo))
}

fn decide(repo: &str, upstream: &str, pull: &Value) -> Result<Repair> {
    // GitHub represents an absent description as null.
    let body = pull["body"].as_str().unwrap_or_default();
    let title = pull["title"].as_str().context("missing pull title")?;
    Ok(text::repair(repo, upstream, title, body))
}

fn change(pull: &Value, after: &str) -> Result<String> {
    Ok(format!(
        "before={} after={}",
        serde_json::to_string(&pull["body"])?,
        serde_json::to_string(after)?
    ))
}

/// Re-read the draft and write only if it is still the one decided on.
fn write(
    client: &impl Rest,
    config: &Config,
    (repo, upstream): (&str, &str),
    pull: &Value,
    after: &str,
) -> Result<()> {
    let number = pull["number"].as_u64().context("invalid pull number")?;
    let fresh = get(client, &format!("repos/{repo}/pulls/{number}"))?;
    for key in IDENTITY {
        ensure!(
            fresh[key] == pull[key],
            "repair {repo}#{number}: {key} changed since it was read; run the repair again"
        );
    }
    ensure!(
        own_draft(config, repo, &fresh)
            && decide(repo, upstream, &fresh)? == Repair::Rewrite(after.into()),
        "repair {repo}#{number}: no longer the bot's draft with this repair"
    );
    let edited = publish::send(
        client,
        "PATCH",
        &format!("repos/{repo}/pulls/{number}"),
        json!({ "body": after }),
    )?;
    ensure!(
        edited["number"] == number && edited["body"] == after,
        "repair {repo}#{number}: the edit returned another body"
    );
    Ok(())
}
