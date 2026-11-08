//! The upstream contribution-policy gate, read-only: one record per upstream,
//! `POLICY_REPO/POLICY_PATH/OWNER/REPO.md` on the policy repository's default
//! branch. It stops the bot publishing where maintainers refuse its text, on
//! a record that no longer describes the upstream, or on a verdict someone
//! other than the operator loosened. Promote never writes a record.
use super::*;

/// Loosest to strictest; an unknown verdict in history ranks strictest.
const VERDICTS: [&str; 4] = ["bot-ok", "human-text", "human-only", "no-go"];
const REQUIRED: [&str; 5] = ["verdict", "ai-trailer", "dco", "sources", "checked"];
const OPTIONAL: [&str; 1] = ["rebase"];
const REBASE: [&str; 2] = ["conflicts-only", "any"];
const SOURCE_KEYS: [&str; 3] = ["repo", "path", "sha"];
/// Scanned in the upstream and in its owner's `.github` repository.
const SCAN_DIRS: [&str; 4] = ["", ".github", "docs", "doc"];
const ORG_REPO: &str = ".github";
/// The contents API lists at most this many directory entries.
const LISTING_LIMIT: usize = 1000;
const RECORD_BYTES: u64 = 64 * 1024;
const SOURCE_LIMIT: usize = 64;
/// Review states that decide a pull request; a later one replaces an approval.
const DECIDING: [&str; 3] = ["APPROVED", "CHANGES_REQUESTED", "DISMISSED"];
/// Git tree modes of a regular file, plain or executable, and of a tree.
const FILE_MODES: [&str; 2] = ["100644", "100755"];
const TREE_MODE: &str = "040000";
/// Contents listing types whose id covers what they hold; a symlink's id is
/// only its target's name.
const SOURCE_TYPES: [&str; 2] = ["file", "dir"];

struct Source {
    repo: String,
    path: String,
    sha: String,
}

struct Record {
    verdict: String,
    sources: Vec<Source>,
}

#[derive(Clone, Copy)]
pub(super) enum Permitted {
    BotOk,
    HumanText,
}

/// Ok only for a fresh, unloosened permitted record; every hold is an error
/// whose text starts with `policy`.
pub(super) fn gate(client: &impl Rest, config: &Config, upstream: &str) -> Result<Permitted> {
    validate_repository(upstream)?;
    let promote = config.promote.as_ref().context("promote configuration")?;
    let policy = promote
        .policy_repository
        .as_deref()
        .context("policy invalid: no policyRepository")?;
    validate_repository(policy)?;
    let file = match promote.policy_path.as_deref() {
        Some(dir) => format!("{}/{upstream}.md", dir.trim_end_matches('/')),
        None => format!("{upstream}.md"),
    };
    segments(&file).context("policy invalid: policyPath")?;
    let Some(found) = read(client, &contents(policy, &file, None)).context("policy unreadable")?
    else {
        return Err(Held(format!("policy missing: no record at {policy}:{file}")).into());
    };
    let invalid = |e: anyhow::Error| anyhow::Error::from(Held(format!("policy invalid: {e:#}")));
    let current = text(&found, &file).map_err(invalid)?;
    let record = parse(&current).map_err(invalid)?;
    fresh(client, upstream, &record)?;
    loosening(client, config, policy, &file, (&found["sha"], &current))?;
    match record.verdict.as_str() {
        "bot-ok" => Ok(Permitted::BotOk),
        "human-text" => Ok(Permitted::HumanText),
        verdict => {
            let sources: Vec<_> = record
                .sources
                .iter()
                .map(|s| format!("{}:{}", s.repo, s.path))
                .collect();
            Err(Held(format!(
                "policy {verdict}: the bot may not post there; sources: {}",
                if sources.is_empty() {
                    "none".into()
                } else {
                    sources.join(", ")
                }
            ))
            .into())
        }
    }
}

/// None is absence: the live client's not-found error, or a replayed 404
/// status. Any other failure is an error, never empty.
fn read(client: &impl Rest, path: &str) -> Result<Option<Value>> {
    match client.get(&format!("{API}{path}"), None) {
        Ok(response) if response.status == 404 => Ok(None),
        Ok(response) => {
            ensure!(
                response.status == 200,
                "GET {path}: HTTP {}",
                response.status
            );
            Ok(Some(response.body))
        }
        Err(error) if error.is::<crate::github::NotFound>() => Ok(None),
        Err(error) => Err(error),
    }
}

/// Only plain path segments, so a record or source path cannot leave its
/// repository's contents or add a query.
fn segments(path: &str) -> Result<()> {
    ensure!(
        path.split('/').all(|s| !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))),
        "invalid path {path:?}"
    );
    Ok(())
}

fn contents(repo: &str, path: &str, at: Option<&str>) -> String {
    let path = if path.is_empty() {
        String::new()
    } else {
        format!("/{path}")
    };
    let at = at.map_or(String::new(), |sha| format!("?ref={sha}"));
    format!("repos/{repo}/contents{path}{at}")
}

/// The record's text. The contents API serves a symlink's target as a file
/// at the link's path, so only `tree_entry` shows that a version is regular.
fn text(file: &Value, path: &str) -> Result<String> {
    ensure!(
        file["type"] == "file" && file["path"] == path,
        "{path} is not a regular file"
    );
    ensure!(
        file["encoding"] == "base64" && file["size"].as_u64().is_some_and(|n| n <= RECORD_BYTES),
        "{path} is not a base64 record of at most {RECORD_BYTES} bytes"
    );
    let bytes = crate::supervise::health::base64_decode(
        file["content"].as_str().context("missing record content")?,
    )?;
    String::from_utf8(bytes).context("record is not UTF-8")
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return inner.into();
        }
    }
    value.into()
}

/// `key: value` with a lower-case, dashed key.
fn field(line: &str) -> Option<(&str, String)> {
    let (key, value) = line.split_once(':')?;
    (!key.is_empty() && key.bytes().all(|c| c.is_ascii_lowercase() || c == b'-'))
        .then(|| (key, unquote(value)))
}

/// The subset of YAML records use: `key: value` lines and the `sources:`
/// list of `repo`/`path`/`sha` mappings (or `[]`), then a non-empty body.
fn parse(text: &str) -> Result<Record> {
    let lines: Vec<_> = text.lines().collect();
    ensure!(lines.first() == Some(&"---"), "no front matter");
    let end = lines
        .iter()
        .skip(1)
        .position(|l| *l == "---")
        .context("unterminated front matter")?
        + 1;
    let mut fields = BTreeMap::new();
    let mut sources: Vec<BTreeMap<&str, String>> = vec![];
    let mut in_sources = false;
    for line in &lines[1..end] {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if in_sources && line.starts_with(char::is_whitespace) {
            let rest = line.trim_start();
            let rest = match rest.strip_prefix("- ") {
                Some(rest) => {
                    sources.push(BTreeMap::new());
                    rest.trim_start()
                }
                None => rest,
            };
            let (key, value) = field(rest).with_context(|| format!("cannot parse {line:?}"))?;
            ensure!(SOURCE_KEYS.contains(&key), "unknown source key {key:?}");
            let source = sources.last_mut().context("a source starts with '- '")?;
            ensure!(
                source.insert(key, value).is_none(),
                "source {key} set twice"
            );
            continue;
        }
        let (key, value) = field(line).with_context(|| format!("cannot parse {line:?}"))?;
        ensure!(
            REQUIRED.contains(&key) || OPTIONAL.contains(&key),
            "unknown key {key:?}"
        );
        ensure!(
            key != "sources" || value.is_empty() || value == "[]",
            "sources is a list or []"
        );
        in_sources = key == "sources" && value.is_empty();
        ensure!(fields.insert(key, value).is_none(), "{key} is set twice");
    }
    for key in REQUIRED {
        ensure!(
            fields
                .get(key)
                .is_some_and(|v| key == "sources" || !v.is_empty()),
            "no {key}"
        );
    }
    let verdict = fields["verdict"].clone();
    ensure!(
        VERDICTS.contains(&verdict.as_str()),
        "unknown verdict {verdict:?}"
    );
    let dco = &fields["dco"];
    ensure!(
        ["yes", "no"].iter().any(|p| dco
            .strip_prefix(*p)
            .is_some_and(|r| !r.starts_with(char::is_alphanumeric))),
        "dco starts with yes or no"
    );
    if let Some(rebase) = fields.get("rebase") {
        ensure!(
            REBASE.contains(&rebase.as_str()),
            "unknown rebase {rebase:?}"
        );
    }
    ensure!(sources.len() <= SOURCE_LIMIT, "over {SOURCE_LIMIT} sources");
    let sources = sources
        .into_iter()
        .map(|mut s| {
            let mut take = |key| {
                s.remove(key)
                    .with_context(|| format!("a source has no {key}"))
            };
            let source = Source {
                repo: take("repo")?,
                path: take("path")?,
                sha: take("sha")?,
            };
            validate_repository(&source.repo)?;
            segments(&source.path)?;
            ensure!(
                source.sha.len() == 40
                    && source
                        .sha
                        .bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
                "source {} has no 40-hex sha",
                source.path
            );
            Ok(source)
        })
        .collect::<Result<_>>()?;
    ensure!(
        lines[end + 1..].iter().any(|l| !l.trim().is_empty()),
        "no body"
    );
    Ok(Record { verdict, sources })
}

/// Homegit's name set: the name up to its first dot (a leading one dropped)
/// is a contribution guide, agent instructions, a PR template, a DCO text,
/// or has AI/LLM as a word.
fn policy_name(name: &str) -> bool {
    let name = name.strip_prefix('.').unwrap_or(name);
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    stem.starts_with("contributing")
        || [
            "agents",
            "claude",
            "gemini",
            "copilot-instructions",
            "pull-request-template",
            "dco",
            "developer-certificate-of-origin",
        ]
        .contains(&stem.replace('_', "-").as_str())
        || stem
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| matches!(w, "ai" | "llm" | "llms" | "genai"))
}

/// A directory entry's name, blob or tree id, and listing type.
type Entry = (String, String, String);

/// Default-branch directory listings, each read once; None is an absent
/// directory. Truncated or malformed listings are errors.
struct Listings<'a, C> {
    client: &'a C,
    seen: BTreeMap<(String, String), Option<Vec<Entry>>>,
}

impl<C: Rest> Listings<'_, C> {
    fn get(&mut self, repo: &str, dir: &str) -> Result<Option<&[Entry]>> {
        let key = (repo.to_ascii_lowercase(), dir.to_owned());
        if !self.seen.contains_key(&key) {
            let listing = read(self.client, &contents(repo, dir, None))
                .and_then(|body| {
                    body.map(|body| {
                        let rows = body.as_array().context("listing is not an array")?;
                        ensure!(rows.len() < LISTING_LIMIT, "listing may be truncated");
                        rows.iter()
                            .map(|e| {
                                let field = |k: &str| {
                                    e[k].as_str()
                                        .map(str::to_owned)
                                        .context("malformed listing")
                                };
                                Ok((field("name")?, field("sha")?, field("type")?))
                            })
                            .collect::<Result<Vec<_>>>()
                    })
                    .transpose()
                })
                .with_context(|| format!("policy unreadable: listing {repo}:{dir}"))?;
            self.seen.insert(key.clone(), listing);
        }
        Ok(self.seen[&key].as_deref())
    }
}

/// Every recorded source is still a file or directory with its id, and no
/// policy-named file in the scanned directories is missing from the record.
/// A symlink source holds: its id would not change with its target's text.
fn fresh(client: &impl Rest, upstream: &str, record: &Record) -> Result<()> {
    let mut listings = Listings {
        client,
        seen: BTreeMap::new(),
    };
    let key = |repo: &str, path: &str| format!("{}:{path}", repo.to_ascii_lowercase());
    let mut changes = vec![];
    for source in &record.sources {
        let (dir, name) = source.path.rsplit_once('/').unwrap_or(("", &source.path));
        let found = listings
            .get(&source.repo, dir)?
            .and_then(|rows| rows.iter().find(|(n, ..)| n == name))
            .map(|(_, sha, kind)| (sha.clone(), kind.clone()));
        let name = key(&source.repo, &source.path);
        match found {
            None => changes.push(format!("{name} is gone")),
            Some((_, kind)) if !SOURCE_TYPES.contains(&kind.as_str()) => {
                changes.push(format!("{name} is a {kind}, not a file or directory"))
            }
            Some((sha, _)) if sha != source.sha => changes.push(format!(
                "{name} changed ({} -> {})",
                &source.sha[..12],
                sha.get(..12).unwrap_or(&sha)
            )),
            Some(_) => {}
        }
    }
    let recorded: BTreeSet<_> = record
        .sources
        .iter()
        .map(|s| key(&s.repo, &s.path))
        .collect();
    let owner = upstream.split_once('/').map_or(upstream, |(o, _)| o);
    let org = format!("{owner}/{ORG_REPO}");
    let mut repos = vec![upstream];
    if !org.eq_ignore_ascii_case(upstream) {
        repos.push(&org);
    }
    for repo in repos {
        for dir in SCAN_DIRS {
            // An absent repository (its top level) has no directories.
            if !dir.is_empty() && listings.get(repo, "")?.is_none() {
                break;
            }
            let rows = listings.get(repo, dir)?.unwrap_or_default().to_vec();
            for (name, ..) in rows {
                let path = if dir.is_empty() {
                    name.clone()
                } else {
                    format!("{dir}/{name}")
                };
                let path = key(repo, &path);
                // Any type: a symlinked AGENTS.md governs as much as a file.
                if policy_name(&name) && !recorded.contains(&path) {
                    changes.push(format!("{path} is not in the record"));
                }
            }
        }
    }
    hold!(changes.is_empty(), "policy stale: {}", changes.join(", "));
    Ok(())
}

/// The verdict line of a historical version, loosely; unknown ranks strictest.
fn rank(text: &str) -> usize {
    let mut lines = text.lines();
    let verdict = (lines.next() == Some("---"))
        .then(|| {
            lines
                .take_while(|l| *l != "---")
                .find_map(|l| l.strip_prefix("verdict:").map(unquote))
        })
        .flatten();
    verdict
        .and_then(|v| VERDICTS.iter().position(|k| *k == v))
        .unwrap_or(VERDICTS.len())
}

/// The mode and id of `file`'s entry in `commit`'s tree, walked one
/// directory at a time; None where it is absent. A parent that is not a
/// tree, such as a symlink, is an error.
fn tree_entry(
    client: &impl Rest,
    policy: &str,
    file: &str,
    commit: &str,
) -> Result<Option<(String, String)>> {
    let mut tree = commit.to_owned();
    let mut parts = file.split('/').peekable();
    while let Some(part) = parts.next() {
        let body = get(client, &format!("repos/{policy}/git/trees/{tree}"))?;
        ensure!(body["truncated"] == false, "tree {tree} may be truncated");
        let rows = body["tree"].as_array().context("malformed tree")?;
        let Some(row) = rows.iter().find(|r| r["path"] == part) else {
            return Ok(None);
        };
        let (Some(mode), Some(sha)) =
            (row["mode"].as_str(), row["sha"].as_str().filter(|s| oid(s)))
        else {
            anyhow::bail!("malformed tree {tree}");
        };
        if parts.peek().is_none() {
            return Ok(Some((mode.to_owned(), sha.to_owned())));
        }
        ensure!(
            mode == TREE_MODE,
            "{file} has a parent of mode {mode} at {commit}"
        );
        tree = sha.to_owned();
    }
    anyhow::bail!("empty record path")
}

/// Homegit's loosening rule over the record's default-branch commits: a
/// verdict may become looser than the strictest since the last vouched
/// loosening only in a commit the operator vouches for; tightening again
/// undoes an unvouched loosening. Every version must be a regular file in
/// its commit's tree: a symlink's target can change outside this history.
fn loosening(
    client: &impl Rest,
    config: &Config,
    policy: &str,
    file: &str,
    (current_sha, current): (&Value, &str),
) -> Result<()> {
    let mut irregular = None;
    let history = (|| {
        let commits = pages(
            client,
            &format!("repos/{policy}/commits?path={}", encode(file)),
        )?;
        ensure!(!commits.is_empty(), "no commits for the record");
        let mut seen = BTreeSet::new();
        let mut history = vec![];
        let mut newest = None;
        // Newest first from GitHub; deletions have no version and are skipped.
        for commit in commits.iter().rev() {
            let sha = commit["sha"]
                .as_str()
                .filter(|s| oid(s))
                .context("invalid commit sha")?;
            ensure!(seen.insert(sha), "duplicate commit");
            let Some((mode, blob)) = tree_entry(client, policy, file, sha)? else {
                continue;
            };
            if !FILE_MODES.contains(&mode.as_str()) {
                irregular = Some(format!("{file} has mode {mode} at {sha}"));
                return Ok(vec![]);
            }
            let version = read(client, &contents(policy, file, Some(sha)))?
                .with_context(|| format!("no {file} at {sha}"))?;
            ensure!(
                version["sha"] == blob,
                "{file} at {sha} is not its tree's blob"
            );
            let version = text(&version, file)?;
            history.push((sha.to_owned(), rank(&version)));
            newest = Some((blob, version));
        }
        ensure!(
            newest.is_some_and(|(blob, text)| *current_sha == blob && text == current),
            "the record changed during the read"
        );
        Ok(history)
    })()
    .context("policy history unreadable")?;
    if let Some(irregular) = irregular {
        return Err(Held(format!("policy invalid: {irregular}, not a regular file")).into());
    }
    let (mut baseline, mut previous, mut unvouched) = (None, None, None);
    let mut branch = None;
    for (sha, rank) in history {
        if baseline.is_none_or(|b| rank >= b) {
            baseline = Some(rank);
            unvouched = None;
        } else if previous != Some(rank) {
            match vouched(client, config, policy, &sha, &mut branch)
                .context("policy history unreadable")?
            {
                None => {
                    baseline = Some(rank);
                    unvouched = None;
                }
                Some(login) => unvouched = Some((login, sha)),
            }
        }
        previous = Some(rank);
    }
    if let Some((login, sha)) = unvouched {
        return Err(Held(format!("policy loosened by {login} at {sha}")).into());
    }
    Ok(())
}

/// None when the operator vouches for `sha`: authored and committed by them
/// with a GitHub-verified signature, or from a merged pull request into the
/// default branch whose merged head they last approved, with that head's
/// tree. Otherwise who committed and, if different, authored it.
fn vouched(
    client: &impl Rest,
    config: &Config,
    policy: &str,
    sha: &str,
    branch: &mut Option<String>,
) -> Result<Option<String>> {
    let commit = get(client, &format!("repos/{policy}/commits/{sha}"))?;
    let login = |role: &str| commit[role]["login"].as_str().unwrap_or("unknown");
    // GitHub verifies the signature against the committer; the author is
    // only an email anyone can set, so both must be the operator.
    if commit["commit"]["verification"]["verified"] == true
        && ["author", "committer"]
            .iter()
            .all(|role| login(role).eq_ignore_ascii_case(&config.operator))
    {
        return Ok(None);
    }
    // Name who committed it: an author alone could be the forged operator.
    let (author, committer) = (login("author"), login("committer"));
    let login = if author.eq_ignore_ascii_case(committer) {
        author.to_owned()
    } else {
        format!("{committer} (author {author})")
    };
    if branch.is_none() {
        let repo = get(client, &format!("repos/{policy}"))?;
        repository(&repo, policy)?;
        *branch = Some(
            repo["default_branch"]
                .as_str()
                .context("missing default branch")?
                .to_owned(),
        );
    }
    let pulls = pages(client, &format!("repos/{policy}/commits/{sha}/pulls"))?;
    let Some(pull) = pulls.iter().find(|p| {
        p["merged_at"].is_string()
            && p["base"]["ref"].as_str() == branch.as_deref()
            && p["base"]["repo"]["full_name"]
                .as_str()
                .is_some_and(|r| r.eq_ignore_ascii_case(policy))
    }) else {
        return Ok(Some(login));
    };
    let (Some(head), Some(tip), Some(number)) = (
        pull["head"]["sha"].as_str().filter(|s| oid(s)),
        pull["merge_commit_sha"].as_str().filter(|s| oid(s)),
        pull["number"].as_u64().filter(|n| *n > 0),
    ) else {
        return Ok(Some(login));
    };
    let reviews = pages(client, &format!("repos/{policy}/pulls/{number}/reviews"))?;
    let last = reviews.iter().rfind(|r| {
        r["user"]["login"]
            .as_str()
            .is_some_and(|l| l.eq_ignore_ascii_case(&config.operator))
            && r["state"].as_str().is_some_and(|s| DECIDING.contains(&s))
    });
    if !last.is_some_and(|r| r["state"] == "APPROVED" && r["commit_id"] == head) {
        return Ok(Some(login));
    }
    // More merged than approved: the merged tip's tree is not the head's.
    let tree = |sha: &str| -> Result<Value> {
        Ok(get(client, &format!("repos/{policy}/git/commits/{sha}"))?["tree"]["sha"].clone())
    };
    let approved = tree(head)?;
    Ok((!(approved.as_str().is_some_and(oid) && approved == tree(tip)?)).then_some(login))
}
