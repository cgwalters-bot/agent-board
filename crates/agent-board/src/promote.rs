//! `promote FORK_PR_URL --dry-run`: check whether a fork pull request the bot opened may
//! go upstream, and print what promoting it would do. This ports the checks of homegit's
//! `bot-pr promote`: only the head the approver approved (an approving review of it, or a
//! `/promote` comment written after it was pushed), only into a repository whose
//! contribution policy record says bot-ok and is current, and a stacked pull request only
//! once its base is merged upstream. Each refusal is the one comment a real promote would
//! leave on the fork pull request. Who the bot and the approver are, which organisation
//! owns the forks and where policy records live come from `--config`
//! (`policy/promote.toml`), not from the code or the dispatch.
//!
//! This is the first slice: it only reads, through `Rest`, which has no write method,
//! and refuses to run without `--dry-run`. Rebasing, the DCO sign-off, opening the
//! upstream pull request, commenting and the board update are not ported yet, nor are
//! `/promote --human-text`, go-ahead hints and the check of who loosened a policy verdict.
use anyhow::{Context, Result, bail, ensure};
use board_forge::github::Rest;
use clap::Args;
use serde::Deserialize;
use serde_json::Value;
use std::{cell::Cell, collections::BTreeMap, io::Write, path::PathBuf};
use time::{OffsetDateTime, PrimitiveDateTime, format_description::well_known::Rfc3339};

const BRANCH_PREFIX: &str = "bot/";
const META_START: &str = "<!-- bot-meta -->";
const META_END: &str = "<!-- /bot-meta -->";
const PROMOTE_COMMAND: &str = "/promote";
/// Approves like `/promote` in homegit; its human-text checks are not ported, and a
/// human-text policy is refused, so here it only approves.
const HUMAN_TEXT_COMMAND: &str = "/promote --human-text";
const DRAFT_COMMAND: &str = "/draft";
const READY_COMMAND: &str = "/ready";
/// Contribution policy records are `upstream-policy/OWNER/REPO.md` in the policy repository.
const POLICY_DIR: &str = "upstream-policy";
const POLICY_VERDICTS: [&str; 4] = ["bot-ok", "human-text", "human-only", "no-go"];
const POLICY_REQUIRED_KEYS: [&str; 5] = ["verdict", "ai-trailer", "dco", "sources", "checked"];
/// The optional `rebase:` of a record; only validated here.
const REBASE_MODES: [&str; 2] = ["conflicts-only", "any"];
const SOURCE_KEYS: [&str; 3] = ["repo", "path", "sha"];
/// Where homegit's `upstream-policy scan` looks for policy files, in the repository and
/// in its owner's `.github` repository.
const SCAN_DIRS: [&str; 4] = ["", ".github", "docs", "doc"];
const ORG_REPO: &str = ".github";
/// One promote reads a few dozen resources; the budget stops a pathological pull request
/// or policy record from turning one run into an unbounded crawl of the API.
const PROMOTE_REQUEST_LIMIT: usize = 100;
const PER_PAGE: usize = 100;
const PAGE_LIMIT: usize = 10;

#[derive(Args)]
pub struct Promote {
    /// The fork pull request: https://github.com/FORGE/REPO/pull/N.
    pub url: String,
    /// Check everything and print what promoting would do, without changing anything (required for now).
    #[arg(long)]
    pub dry_run: bool,
    /// Who and where promote trusts (policy/promote.toml); read it from trusted main only.
    #[arg(long, value_name = "FILE")]
    pub config: PathBuf,
}

/// Who and where promote trusts. It is a file the repository owns rather than flags, so
/// that whoever dispatches the workflow cannot name another approver or author, or read
/// policy records from another repository; each key is required.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The author of every fork pull request; its own older forks count as forks too.
    pub bot: String,
    /// The only login whose approval or `/promote` promotes.
    pub approver: String,
    /// The organisation that owns the forks.
    pub forge: String,
    pub policy_repo: String,
    pub policy_branch: String,
}

impl Config {
    pub fn parse(input: &str) -> Result<Self> {
        let config: Self = toml::from_str(input).context("decode promote config TOML")?;
        for (key, login) in [
            ("bot", &config.bot),
            ("approver", &config.approver),
            ("forge", &config.forge),
        ] {
            ensure!(
                valid_login(login),
                "promote config: {key} '{login}' is not a GitHub login"
            );
        }
        ensure!(
            valid_repo(&config.policy_repo),
            "promote config: policy_repo '{}' is not OWNER/REPO",
            config.policy_repo
        );
        ensure!(
            valid_branch(&config.policy_branch),
            "promote config: policy_branch '{}' is not a branch name",
            config.policy_branch
        );
        Ok(config)
    }

    pub fn load(path: &std::path::Path) -> Result<Self> {
        let input = std::fs::read_to_string(path)
            .with_context(|| format!("read promote config {}", path.display()))?;
        Self::parse(&input).with_context(|| format!("in {}", path.display()))
    }

    /// Where fork pull requests live: the forge, and the bot's own older forks.
    fn fork_owners(&self) -> [&str; 2] {
        [&self.forge, &self.bot]
    }
}

/// Validation that needs no token, before the client is built.
pub fn check_args(args: &Promote, config: &Config) -> Result<()> {
    ensure!(
        args.dry_run,
        "unsupported_capability: promote only runs with --dry-run so far; opening upstream pull requests is not implemented"
    );
    parse_fork_url(config, &args.url).map(drop)
}

/// The request-counted reads of one promote.
pub struct Api<'a, R> {
    rest: &'a R,
    remaining: Cell<usize>,
}

impl<'a, R: Rest> Api<'a, R> {
    pub fn new(rest: &'a R) -> Self {
        Self {
            rest,
            remaining: Cell::new(PROMOTE_REQUEST_LIMIT),
        }
    }

    fn get(&self, path: &str, raw: bool) -> Result<Option<String>> {
        let remaining = self.remaining.get();
        ensure!(
            remaining > 0,
            "promote request budget ({PROMOTE_REQUEST_LIMIT}) exhausted; nothing was decided"
        );
        self.remaining.set(remaining - 1);
        self.rest.get(path, raw)
    }

    fn json(&self, path: &str) -> Result<Option<Value>> {
        self.get(path, false)?
            .map(|body| serde_json::from_str(&body).with_context(|| format!("decode {path}")))
            .transpose()
    }

    fn required(&self, path: &str) -> Result<Value> {
        self.json(path)?
            .with_context(|| format!("GitHub has no {path}"))
    }

    /// Every page of an array endpoint, at most `PAGE_LIMIT` of them.
    fn pages(&self, path: &str) -> Result<Vec<Value>> {
        let mut all = Vec::new();
        let separator = if path.contains('?') { '&' } else { '?' };
        for page in 1..=PAGE_LIMIT {
            let value =
                self.required(&format!("{path}{separator}per_page={PER_PAGE}&page={page}"))?;
            let items = value
                .as_array()
                .with_context(|| format!("{path} is not a list"))?;
            all.extend(items.iter().cloned());
            if items.len() < PER_PAGE {
                return Ok(all);
            }
        }
        bail!("{path} has more than {} entries", PAGE_LIMIT * PER_PAGE)
    }
}

pub fn run<R: Rest>(
    args: &Promote,
    config: &Config,
    api: &Api<R>,
    out: &mut impl Write,
) -> Result<()> {
    check_args(args, config)?;
    let (fork, number) = parse_fork_url(config, &args.url)?;
    let url = &args.url;
    let (bot, approver) = (&config.bot, &config.approver);
    let pr = api.required(&format!("repos/{fork}/pulls/{number}"))?;
    let author = text(&pr["user"]["login"]);
    ensure!(author == bot, "{url} was opened by {author}, not {bot}");
    let state = text(&pr["state"]);
    ensure!(
        state == "open",
        "{url} is {state}; only an open fork PR can be promoted"
    );
    let head_repo = text(&pr["head"]["repo"]["full_name"]);
    ensure!(
        head_repo == fork,
        "{url}'s head is in {head_repo}, not {fork}"
    );
    let head = text(&pr["head"]["ref"]);
    ensure!(
        head.len() > BRANCH_PREFIX.len() && head.starts_with(BRANCH_PREFIX) && valid_branch(head),
        "{url}'s branch '{head}' is not a {BRANCH_PREFIX}<slug> branch"
    );
    let head_sha = text(&pr["head"]["sha"]);
    ensure!(is_sha(head_sha), "{url} has no head commit");
    let owner = fork.split('/').next().unwrap_or_default();

    // The upstream comes from the bot-meta section: a fork's parent is not
    // necessarily the upstream, but it must be in the fork's network.
    let body = pr["body"]
        .as_str()
        .unwrap_or_default()
        .replace("\r\n", "\n");
    let meta = Meta::parse(config, url, &body)?;
    let upstream = &meta.upstream;
    let base = &meta.base;
    let network = api.required(&format!("repos/{fork}"))?;
    let (parent, source) = (
        text(&network["parent"]["full_name"]),
        text(&network["source"]["full_name"]),
    );
    ensure!(
        upstream == parent || upstream == source,
        "bot-meta names upstream {upstream}, but {fork} is a fork of {parent} (network root {source})"
    );
    let title = text(&pr["title"]);
    let upstream_body = strip_meta(&body);
    ensure!(
        !upstream_body.contains("bot-meta"),
        "{url}'s body still mentions bot-meta after stripping the section; fix its markers"
    );
    ensure!(
        !upstream_body.is_empty(),
        "{url}'s body is empty without the bot-meta section"
    );

    let mut refusals = Vec::new();
    // Before its base is merged upstream, a stacked pull request would carry the base's
    // commits too.
    if let Some(stacked) = &meta.stacked {
        match stack_state(api, config, upstream, base, stacked)? {
            Stack::Open => refusals.push(format!(
                "it is stacked on {stacked}, which is not promoted yet: promote that first, and this one once it is merged upstream"
            )),
            Stack::Promoted(pr) => refusals.push(format!(
                "it is stacked on {stacked}, whose upstream PR {pr} is not merged yet: promote this one once it is"
            )),
            Stack::Dropped => refusals.push(format!(
                "it is stacked on {stacked}, which was closed without being merged upstream: rebase {head} onto {upstream}:{base} by hand and drop the 'Stacked on' line"
            )),
            Stack::Merged(merged) => {
                eprintln!("{stacked}, which {url} is stacked on, is merged upstream as {merged}")
            }
        }
    } else if text(&pr["base"]["ref"]) != base {
        eprintln!(
            "warning: the fork PR targets '{}', but bot-meta says upstream base '{base}'; using '{base}'",
            text(&pr["base"]["ref"])
        );
    }

    let activity = Activity::read(api, approver, &fork, number)?;
    // The timeline dates force-pushes for `/promote` and dismissals, whose reviews keep
    // their original submission time.
    let timeline = if activity.has_promote_comment() || activity.has_dismissal() {
        api.pages(&format!("repos/{fork}/issues/{number}/timeline"))?
    } else {
        Vec::new()
    };
    let pushed = if activity.has_promote_comment() {
        push_time(api, &fork, head, head_sha, &timeline)?
    } else {
        None
    };
    let approval = activity.approval(head_sha, pushed, &timeline);
    if let Some(why) = approval_refusal(approver, approval.as_ref(), head_sha, pushed) {
        refusals.push(why);
    }
    let policy = policy_gate(api, config, upstream)?;
    if let Err(why) = &policy {
        refusals.push(why.clone());
    }

    // The approved commit, not the branch, which a push could have moved since. A fork's
    // commits are readable through its upstream, as they share one network.
    let compare = api.required(&format!("repos/{upstream}/compare/{base}...{head_sha}"))?;
    let ahead = compare["ahead_by"]
        .as_u64()
        .context("compare has no ahead_by")?;
    let behind = compare["behind_by"]
        .as_u64()
        .context("compare has no behind_by")?;
    ensure!(
        ahead > 0,
        "{head} has no commits on top of {upstream}:{base}; was it merged already?"
    );
    let existing = api.pages(&format!(
        "repos/{upstream}/pulls?head={}&state=open",
        encode(&format!("{owner}:{head}"))
    ))?;
    // The head query matches branches in any repository owned by this owner.
    let existing = existing
        .iter()
        .find(|pr| {
            text(&pr["head"]["repo"]["full_name"]) == fork
                && text(&pr["base"]["repo"]["full_name"]) == upstream
                && text(&pr["base"]["ref"]) == base
        })
        .and_then(|pr| pr["html_url"].as_str());
    // A push during these reads would make the plan or refusal one for another head.
    let reread = api.required(&format!("repos/{fork}/pulls/{number}"))?;
    let now = text(&reread["head"]["sha"]);
    ensure!(
        now == head_sha && text(&reread["state"]) == "open",
        "{url} changed while it was checked (head {now}, was {head_sha}); nothing was decided, run it again"
    );

    let verdict = match &approval {
        None => "no verdict".to_owned(),
        Some(a) if a.via_comment => format!("{} by {PROMOTE_COMMAND} {}", a.state, a.url),
        Some(a) => a.state.clone(),
    };
    writeln!(out, "Fork PR:   {url} ({approver}: {verdict})")?;
    writeln!(
        out,
        "Upstream:  {upstream}, base {base}, from {owner}:{head} at {head_sha} ({ahead} ahead, {behind} behind)"
    )?;
    writeln!(
        out,
        "Board:     {}",
        meta.item.as_deref().unwrap_or("(none)")
    )?;
    writeln!(out, "Policy:    {}", policy.as_deref().unwrap_or("refused"))?;
    writeln!(out)?;
    if refusals.is_empty() {
        writeln!(out, "Would:")?;
        if behind > 0 {
            writeln!(
                out,
                "- rebase {head} onto {upstream}:{base} and force-push it (on conflicts: dismiss the approval, comment, stop)"
            )?;
        }
        writeln!(
            out,
            "- check whether {upstream} requires DCO, and if so sign off as {approver} (not ported; decided by the real promote)"
        )?;
        match existing {
            Some(existing) => writeln!(out, "- reuse the already open {existing}")?,
            None => writeln!(
                out,
                "- open a {} PR in {upstream}",
                if activity.wants_draft() {
                    "draft"
                } else {
                    "ready-for-review"
                }
            )?,
        }
        writeln!(out, "- comment the upstream PR link on {url} and close it")?;
        if let Some(item) = &meta.item {
            writeln!(
                out,
                "- set board item {item} to In Review with Branch = the upstream PR"
            )?;
        }
    } else {
        writeln!(out, "Would comment on {url} and stop:")?;
        writeln!(out, "Not promoted: {}.", refusals.join("; "))?;
    }
    writeln!(out, "\nTitle: {title}\n\n{upstream_body}")?;
    Ok(())
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

fn is_sha(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// OWNER/REPO, neither part empty or all dots.
fn valid_repo(name: &str) -> bool {
    let parts: Vec<_> = name.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && !p.chars().all(|c| c == '.')
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
        })
}

/// A GitHub user or organisation name.
fn valid_login(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Branch names used unencoded in API paths, as `bot-pr` does.
fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("..")
        && !name.starts_with(['/', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./-".contains(c))
}

/// Percent-encoding for one path segment or query value.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// `https://github.com/OWNER/REPO/pull/N` with OWNER one of the config's fork owners.
fn parse_fork_url(config: &Config, url: &str) -> Result<(String, u64)> {
    let parsed = url
        .strip_prefix("https://github.com/")
        .map(|rest| rest.strip_suffix('/').unwrap_or(rest))
        .and_then(|rest| match rest.split('/').collect::<Vec<_>>()[..] {
            [owner, repo, "pull", number] => Some((owner, repo, number)),
            _ => None,
        })
        .and_then(|(owner, repo, number)| {
            let fork = format!("{owner}/{repo}");
            let number = number
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0 && !number.starts_with('0'))?;
            (config.fork_owners().contains(&owner) && valid_repo(&fork)).then_some((fork, number))
        });
    parsed.with_context(|| {
        format!(
            "expected a PR URL in one of the forks in {0} (https://github.com/{0}/REPO/pull/N), got '{url}'",
            config.forge
        )
    })
}

/// What a fork PR's bot-meta section records.
#[derive(Debug, PartialEq)]
struct Meta {
    upstream: String,
    base: String,
    item: Option<String>,
    stacked: Option<String>,
}

/// The text after `prefix` in `s`, up to the first character not satisfying `keep`.
fn after<'a>(s: &'a str, prefix: &str, keep: impl Fn(char) -> bool) -> Option<&'a str> {
    let rest = &s[s.find(prefix)? + prefix.len()..];
    Some(&rest[..rest.find(|c| !keep(c)).unwrap_or(rest.len())])
}

impl Meta {
    fn parse(config: &Config, url: &str, body: &str) -> Result<Self> {
        let section = body
            .find(META_START)
            .and_then(|start| {
                let end = body[start..].find(META_END)? + start + META_END.len();
                Some(&body[start..end])
            })
            .with_context(|| {
                format!("{url} has no complete {META_START} ... {META_END} section; restore it in the PR body")
            })?;
        let upstream_line = after(section, "Upstream: `", |c| c != '\n').unwrap_or_default();
        let (upstream, base) = upstream_line
            .split_once("`, base `")
            .and_then(|(upstream, rest)| Some((upstream, rest.split_once('`')?.0)))
            .with_context(|| {
                format!("cannot find the 'Upstream: `OWNER/REPO`, base `BRANCH`' line in {url}'s bot-meta section")
            })?;
        ensure!(
            valid_repo(upstream),
            "invalid upstream '{upstream}' in {url}'s bot-meta section"
        );
        ensure!(
            valid_branch(base),
            "invalid base '{base}' in {url}'s bot-meta section"
        );
        let item = after(section, "Board item: `PVTI_", |c| {
            c.is_ascii_alphanumeric() || "_-".contains(c)
        })
        .filter(|id| !id.is_empty() && section.contains(&format!("`PVTI_{id}`")))
        .map(|id| format!("PVTI_{id}"));
        let stacked = after(section, "Stacked on: ", |c| !c.is_whitespace() && c != ';')
            .map(|parent| {
                parse_fork_url(config, parent)
                    .with_context(|| format!("invalid 'Stacked on' in {url}'s bot-meta section"))
                    .map(|_| parent.to_owned())
            })
            .transpose()?;
        Ok(Self {
            upstream: upstream.into(),
            base: base.into(),
            item,
            stacked,
        })
    }
}

/// The body without any bot-meta section, or the whitespace before it, as the upstream
/// PR gets it.
fn strip_meta(body: &str) -> String {
    let mut body = body.to_owned();
    while let Some(start) = body.find(META_START) {
        let Some(end) = body[start..].find(META_END) else {
            break;
        };
        let end = start + end + META_END.len();
        let start = body[..start].trim_end().len();
        body.replace_range(start..end, "");
    }
    body.trim_end().to_owned()
}

enum Stack {
    Open,
    Promoted(String),
    Merged(String),
    Dropped,
}

/// Where the fork PR `parent` is: still a fork PR, promoted (its upstream PR open),
/// merged upstream into `base`, or closed with nothing of it merged there.
fn stack_state<R: Rest>(
    api: &Api<R>,
    config: &Config,
    upstream: &str,
    base: &str,
    parent: &str,
) -> Result<Stack> {
    let (fork, number) = parse_fork_url(config, parent)?;
    let pr = api.required(&format!("repos/{fork}/pulls/{number}"))?;
    if text(&pr["state"]) == "open" {
        return Ok(Stack::Open);
    }
    let branch = text(&pr["head"]["ref"]);
    ensure!(valid_branch(branch), "{parent} has no valid head branch");
    let owner = fork.split('/').next().unwrap_or_default();
    let created =
        timestamp(&pr["created_at"]).with_context(|| format!("{parent} has no created_at"))?;
    let head = text(&pr["head"]["sha"]);
    ensure!(is_sha(head), "{parent} has no head commit");
    let ups = api.pages(&format!(
        "repos/{upstream}/pulls?head={}&state=all",
        encode(&format!("{owner}:{branch}"))
    ))?;
    // `head=OWNER:BRANCH` matches any of the owner's repositories and every past use of
    // the branch. Require the parent's recorded head, not just a later merge date:
    // an earlier promotion can remain open while the branch is reused. Promotion
    // rewrites must be pushed to the fork before closing the parent so both PRs record
    // the rewritten head. If that association is lost, refuse rather than guess.
    // Only a PR into the child's upstream base counts: a merge into another branch
    // leaves the parent's commits out of `base`.
    let ups: Vec<_> = ups
        .iter()
        .filter(|up| {
            text(&up["head"]["repo"]["full_name"]) == fork
                && text(&up["head"]["sha"]) == head
                && text(&up["base"]["ref"]) == base
        })
        .collect();
    let url = |pr: &Value| text(&pr["html_url"]).to_owned();
    if let Some(pr) = ups
        .iter()
        .find(|pr| timestamp(&pr["merged_at"]).is_some_and(|merged| merged >= created))
    {
        return Ok(Stack::Merged(url(pr)));
    }
    Ok(match ups.iter().find(|pr| text(&pr["state"]) == "open") {
        Some(pr) => Stack::Promoted(url(pr)),
        None => Stack::Dropped,
    })
}

fn timestamp(value: &Value) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value.as_str()?, &Rfc3339).ok()
}

/// Whether a line of `body`, trimmed of spaces and tabs, is `command`.
fn has_line(body: &str, command: &str) -> bool {
    body.lines()
        .any(|line| line.trim_matches([' ', '\t', '\r']) == command)
}

/// The approver's reviews, conversation comments and inline review comments on the fork PR.
/// Inline review comments only count for `/draft` and `/ready`, as in `bot-pr`.
struct Activity {
    reviews: Vec<Value>,
    comments: Vec<Value>,
    review_comments: Vec<Value>,
}

#[derive(Debug)]
struct Approval {
    state: String,
    at: OffsetDateTime,
    url: String,
    /// The approved commit; a `/promote` names none unless it followed the head's push.
    commit: Option<String>,
    via_comment: bool,
}

impl Activity {
    fn read<R: Rest>(api: &Api<R>, approver: &str, fork: &str, number: u64) -> Result<Self> {
        let by_reviewer = |items: Vec<Value>| {
            items
                .into_iter()
                .filter(|v| v["user"]["login"] == approver)
                .collect()
        };
        Ok(Self {
            reviews: by_reviewer(api.pages(&format!("repos/{fork}/pulls/{number}/reviews"))?),
            comments: by_reviewer(api.pages(&format!("repos/{fork}/issues/{number}/comments"))?),
            review_comments: by_reviewer(
                api.pages(&format!("repos/{fork}/pulls/{number}/comments"))?,
            ),
        })
    }

    fn is_promote(comment: &Value) -> bool {
        let body = text(&comment["body"]);
        has_line(body, PROMOTE_COMMAND) || has_line(body, HUMAN_TEXT_COMMAND)
    }

    fn has_promote_comment(&self) -> bool {
        self.comments.iter().any(Self::is_promote)
    }

    fn has_dismissal(&self) -> bool {
        self.reviews.iter().any(|r| r["state"] == "DISMISSED")
    }

    /// The latest of the approver's approvals, change requests and dismissed reviews, and
    /// `/promote` comments (which approve the head they were written on: the current one
    /// only if written after `pushed`). A later change request or dismissal cancels an
    /// approval. A dismissal counts from its `review_dismissed` event in `timeline`, and
    /// as the latest of all if it has none, so an unknown time can only refuse.
    fn approval(
        &self,
        head: &str,
        pushed: Option<OffsetDateTime>,
        timeline: &[Value],
    ) -> Option<Approval> {
        let dismissed = |review: &Value| {
            timeline
                .iter()
                .filter(|e| {
                    e["event"] == "review_dismissed"
                        && review["id"].is_u64()
                        && e["dismissed_review"]["review_id"] == review["id"]
                })
                .filter_map(|e| timestamp(&e["created_at"]))
                .max()
                .unwrap_or(PrimitiveDateTime::MAX.assume_utc())
        };
        let reviews = self.reviews.iter().filter_map(|r| {
            let state = text(&r["state"]);
            ["APPROVED", "CHANGES_REQUESTED", "DISMISSED"]
                .contains(&state)
                .then_some(())?;
            let submitted = timestamp(&r["submitted_at"])?;
            Some(Approval {
                state: state.into(),
                at: if state == "DISMISSED" {
                    dismissed(r).max(submitted)
                } else {
                    submitted
                },
                url: text(&r["html_url"]).into(),
                commit: r["commit_id"].as_str().map(str::to_owned),
                via_comment: false,
            })
        });
        let promotions = self
            .comments
            .iter()
            .filter(|c| Self::is_promote(c))
            .filter_map(|c| {
                let at = timestamp(&c["created_at"])?;
                Some(Approval {
                    state: "APPROVED".into(),
                    at,
                    url: text(&c["html_url"]).into(),
                    commit: pushed.filter(|p| at > *p).map(|_| head.to_owned()),
                    via_comment: true,
                })
            });
        let mut all: Vec<_> = reviews.chain(promotions).collect();
        all.sort_by_key(|a| a.at);
        all.pop()
    }

    /// Whether the approver's latest `/draft` or `/ready` line, in a comment or review, is
    /// `/draft`.
    fn wants_draft(&self) -> bool {
        let last_command = |body: &str| {
            body.lines()
                .map(|l| l.trim_matches([' ', '\t', '\r']))
                .rfind(|l| *l == DRAFT_COMMAND || *l == READY_COMMAND)
                .map(str::to_owned)
        };
        let mut commands: Vec<_> = self
            .reviews
            .iter()
            .map(|r| {
                (
                    timestamp(&r["submitted_at"]),
                    last_command(text(&r["body"])),
                )
            })
            .chain(
                self.comments
                    .iter()
                    .chain(&self.review_comments)
                    .map(|c| (timestamp(&c["updated_at"]), last_command(text(&c["body"])))),
            )
            .filter_map(|(at, command)| Some((at?, command?)))
            .collect();
        commands.sort_by_key(|(at, _)| *at);
        commands.last().is_some_and(|(_, c)| c == DRAFT_COMMAND)
    }
}

/// Why the head isn't approved, or `None` if it is.
fn approval_refusal(
    approver: &str,
    approval: Option<&Approval>,
    head: &str,
    pushed: Option<OffsetDateTime>,
) -> Option<String> {
    match approval {
        Some(a) if a.state == "APPROVED" && a.commit.as_deref() == Some(head) => None,
        Some(a) if a.state == "APPROVED" && a.via_comment && pushed.is_none() => Some(format!(
            "it has a '{PROMOTE_COMMAND}' by {approver}, but when its head was pushed can't be told; {approver} needs to approve it"
        )),
        Some(a) if a.state == "APPROVED" => Some(format!(
            "it has commits pushed after {approver} approved it; {approver} needs to approve the current head or comment '{PROMOTE_COMMAND}' again"
        )),
        _ => Some(format!(
            "it is not approved by {approver}; only an approving review by {approver} or a comment of theirs with a '{PROMOTE_COMMAND}' line promotes a fork PR"
        )),
    }
}

/// When `head_sha` became the head: the latest of the push the fork's activity log
/// records for it, any force-push in the PR's timeline, and the committer date of any of
/// its commits (a commit made before a comment can be pushed after it, so commit dates
/// alone can't tell). `None` if the log's latest push to the branch isn't of `head_sha`,
/// which voids every `/promote`.
fn push_time<R: Rest>(
    api: &Api<R>,
    fork: &str,
    branch: &str,
    head_sha: &str,
    timeline: &[Value],
) -> Result<Option<OffsetDateTime>> {
    let log = api.required(&format!(
        "repos/{fork}/activity?ref={}&direction=desc&per_page=1",
        encode(&format!("refs/heads/{branch}"))
    ))?;
    let Some(logged) = log
        .get(0)
        .filter(|push| push["after"] == head_sha)
        .and_then(|push| timestamp(&push["timestamp"]))
    else {
        eprintln!(
            "warning: the activity log of {fork} doesn't show when {branch} was pushed at {}, so '{PROMOTE_COMMAND}' comments there count for nothing",
            &head_sha[..12]
        );
        return Ok(None);
    };
    Ok(timeline
        .iter()
        .filter_map(|event| match text(&event["event"]) {
            "head_ref_force_pushed" => timestamp(&event["created_at"]),
            "committed" => timestamp(&event["committer"]["date"]),
            _ => None,
        })
        .chain([logged])
        .max())
}

/// A git tree entry's mode for a regular, non-executable file.
const REGULAR_FILE: &str = "100644";
const SYMLINK: &str = "120000";

#[derive(Clone)]
struct Entry {
    sha: String,
    kind: String,
    mode: String,
}

/// A repository's canonical name and default branch, and its directory trees.
#[derive(Default)]
struct Trees {
    repos: BTreeMap<String, Option<(String, String)>>,
    dirs: BTreeMap<(String, String, String), BTreeMap<String, Entry>>,
}

impl Trees {
    /// `(full name, default branch)`, or `None` for a repository GitHub doesn't have.
    fn repo<R: Rest>(&mut self, api: &Api<R>, name: &str) -> Result<Option<(String, String)>> {
        let key = name.to_lowercase();
        if !self.repos.contains_key(&key) {
            let info = match api.json(&format!("repos/{name}"))? {
                None => None,
                Some(info) => {
                    let (full, branch) = (text(&info["full_name"]), text(&info["default_branch"]));
                    ensure!(
                        valid_repo(full) && !branch.is_empty(),
                        "GitHub gave no name or default branch for {name}"
                    );
                    // GitHub redirects a renamed repository, and a record naming the old
                    // name would then vouch for another one's policy.
                    ensure!(
                        full.eq_ignore_ascii_case(name),
                        "{name} was renamed to {full}; its contribution policy record must name the new one"
                    );
                    Some((full.to_owned(), branch.to_owned()))
                }
            };
            self.repos.insert(key.clone(), info);
        }
        Ok(self.repos[&key].clone())
    }

    /// The entries of `dir` ("" for the top) on `repo`'s branch, by name; empty if it
    /// doesn't exist.
    fn entries<R: Rest>(
        &mut self,
        api: &Api<R>,
        repo: &(String, String),
        dir: &str,
    ) -> Result<BTreeMap<String, Entry>> {
        let key = (repo.0.to_lowercase(), repo.1.clone(), dir.to_owned());
        if let Some(entries) = self.dirs.get(&key) {
            return Ok(entries.clone());
        }
        let sha = if dir.is_empty() {
            Some(repo.1.clone())
        } else {
            let (parent, name) = dir.rsplit_once('/').unwrap_or(("", dir));
            let entries = self.entries(api, repo, parent)?;
            let entry = entries.get(name);
            ensure!(
                entry.is_none_or(|entry| entry.mode != SYMLINK),
                "{}:{dir} is a symlink; re-check the policy with repository-local sources",
                repo.0
            );
            entry
                .filter(|entry| entry.kind == "tree")
                .map(|entry| entry.sha.clone())
        };
        let mut entries = BTreeMap::new();
        if let Some(sha) = sha {
            let tree = api.required(&format!("repos/{}/git/trees/{}", repo.0, encode(&sha)))?;
            ensure!(
                tree["truncated"] != true,
                "the tree of {} in {} is truncated",
                if dir.is_empty() { "the top" } else { dir },
                repo.0
            );
            for entry in tree["tree"]
                .as_array()
                .with_context(|| format!("GitHub gave no tree for {dir:?} of {}", repo.0))?
            {
                entries.insert(
                    text(&entry["path"]).to_owned(),
                    Entry {
                        sha: text(&entry["sha"]).to_owned(),
                        kind: text(&entry["type"]).to_owned(),
                        mode: text(&entry["mode"]).to_owned(),
                    },
                );
            }
        }
        self.dirs.insert(key, entries.clone());
        Ok(entries)
    }

    fn sha<R: Rest>(
        &mut self,
        api: &Api<R>,
        repo: &(String, String),
        path: &str,
    ) -> Result<Option<String>> {
        let path = path.trim_end_matches('/');
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        let entries = self.entries(api, repo, dir)?;
        let entry = entries.get(name);
        // A symlink's blob hashes its pathname, not the policy text at its target.
        ensure!(
            entry.is_none_or(|entry| entry.mode != SYMLINK),
            "{}:{path} is a symlink; re-check the policy with repository-local sources",
            repo.0
        );
        Ok(entry.map(|entry| entry.sha.clone()))
    }
}

/// Whether a file or directory name, up to its first dot, names a policy source: as
/// homegit's SOURCE_NAME_RE, contribution guides, agent instructions, PR templates, DCO
/// texts, and anything with AI or LLM as a word in its name.
fn is_policy_source(entry: &str) -> bool {
    let stem = entry
        .strip_prefix('.')
        .unwrap_or(entry)
        .split('.')
        .next()
        .unwrap_or_default()
        .to_lowercase();
    let words: Vec<_> = stem.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    stem.starts_with("contributing")
        || [
            "agents",
            "claude",
            "gemini",
            "copilot-instructions",
            "pull_request_template",
            "dco",
            "developer-certificate-of-origin",
            "developer_certificate_of_origin",
        ]
        .contains(&stem.as_str())
        || words
            .iter()
            .any(|w| ["ai", "llm", "llms", "genai"].contains(w))
}

/// A record's front matter: `key: value` lines, and the `sources:` list of mappings.
#[derive(Debug)]
struct Record {
    verdict: String,
    checked: String,
    sources: Vec<BTreeMap<String, String>>,
}

fn unquote(v: &str) -> &str {
    let v = v.trim();
    for q in ['"', '\''] {
        if let Some(inner) = v.strip_prefix(q).and_then(|v| v.strip_suffix(q)) {
            return inner;
        }
    }
    v
}

fn parse_record(text: &str) -> Result<Record> {
    let lines: Vec<_> = text.lines().collect();
    ensure!(
        lines.first() == Some(&"---"),
        "a record starts with a '---' front matter line"
    );
    let end = lines[1..]
        .iter()
        .position(|l| *l == "---")
        .map(|i| i + 1)
        .context("the front matter has no closing '---' line")?;
    let mut fields = BTreeMap::new();
    let mut sources: Option<Vec<BTreeMap<String, String>>> = None;
    let mut all_sources = None;
    for (n, line) in lines.iter().enumerate().take(end).skip(1) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let bad = |why: String| anyhow::anyhow!("line {}: {why}", n + 1);
        if let (Some(list), true) = (sources.as_mut(), line.starts_with(char::is_whitespace)) {
            let (item, rest) = match trimmed.strip_prefix("- ") {
                Some(rest) => (true, rest.trim_start()),
                None => (false, trimmed),
            };
            if item {
                list.push(BTreeMap::new());
            }
            let (key, value) = rest
                .split_once(':')
                .ok_or_else(|| bad(format!("cannot parse '{line}'")))?;
            let current = list
                .last_mut()
                .ok_or_else(|| bad("a source starts with '- '".into()))?;
            ensure!(
                SOURCE_KEYS.contains(&key),
                bad(format!("unknown source key '{key}'"))
            );
            ensure!(
                current.insert(key.into(), unquote(value).into()).is_none(),
                bad(format!("the source's '{key}' is set twice"))
            );
            continue;
        }
        if let Some(list) = sources.take() {
            all_sources = Some(list);
        }
        let (key, value) = line
            .split_once(':')
            .filter(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_lowercase() || c == '-'))
            .ok_or_else(|| bad(format!("cannot parse '{line}'")))?;
        ensure!(
            !fields.contains_key(key) && !(key == "sources" && all_sources.is_some()),
            bad(format!("'{key}' is set twice"))
        );
        if key == "sources" {
            ensure!(
                ["", "[]"].contains(&value.trim()),
                bad("'sources:' is followed by a list, or is '[]'".into())
            );
            sources = Some(Vec::new());
            fields.insert(key.to_owned(), "list".to_owned());
        } else {
            fields.insert(key.to_owned(), unquote(value).to_owned());
        }
    }
    let sources = sources.or(all_sources).unwrap_or_default();
    for key in POLICY_REQUIRED_KEYS {
        ensure!(
            fields.get(key).is_some_and(|v| !v.is_empty()),
            "the front matter has no '{key}'"
        );
    }
    let verdict = &fields["verdict"];
    ensure!(
        POLICY_VERDICTS.contains(&verdict.as_str()),
        "unknown verdict '{verdict}'"
    );
    let dco = &fields["dco"];
    ensure!(
        ["yes", "no"].iter().any(|w| dco == w
            || dco
                .strip_prefix(w)
                .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_'))),
        "'dco' starts with yes or no"
    );
    ensure!(
        fields
            .get("rebase")
            .is_none_or(|mode| REBASE_MODES.contains(&mode.as_str())),
        "'rebase' can only be {} (leave it out otherwise)",
        REBASE_MODES.join(" or ")
    );
    for source in &sources {
        let path = source.get("path").context("a source has no 'path'")?;
        ensure!(
            source.get("sha").is_some_and(|s| is_sha(s)),
            "the source {path} has no 40-hex 'sha'"
        );
        ensure!(
            source.get("repo").is_none_or(|r| valid_repo(r)),
            "invalid repo in the source {path}"
        );
    }
    ensure!(
        lines[end + 1..].iter().any(|l| !l.trim().is_empty()),
        "the record has no body (quotes and rationale)"
    );
    Ok(Record {
        verdict: verdict.clone(),
        checked: fields["checked"].clone(),
        sources,
    })
}

/// The contribution policy gate: `Ok` with the verdict if `upstream`'s record in homegit
/// is valid, current and bot-ok, else `Err` with why not. As homegit's `upstream-policy
/// check`, except that it does not check who loosened a verdict in the record's history.
fn policy_gate<R: Rest>(
    api: &Api<R>,
    config: &Config,
    upstream: &str,
) -> Result<Result<String, String>> {
    let mut trees = Trees::default();
    let repo = trees
        .repo(api, upstream)?
        .with_context(|| format!("GitHub has no repository {upstream}"))?;
    let name = &repo.0;
    // Looked up through homegit's tree, not the contents API, which would follow a
    // symlink to another repository's record: the record must be a regular file.
    let homegit = (config.policy_repo.clone(), config.policy_branch.clone());
    let (owner, file) = name.split_once('/').unwrap_or_default();
    let entry = trees
        .entries(api, &homegit, &format!("{POLICY_DIR}/{owner}"))?
        .remove(&format!("{file}.md"));
    let text = match entry {
        None => {
            return Ok(Err(format!(
                "{name} has no contribution policy record yet (a policy-check subagent writes it)"
            )));
        }
        Some(entry) if entry.kind != "blob" || entry.mode != REGULAR_FILE => {
            return Ok(Err(format!(
                "{name}'s contribution policy record is not a regular file"
            )));
        }
        Some(entry) => api
            .get(
                &format!("repos/{}/git/blobs/{}", homegit.0, encode(&entry.sha)),
                true,
            )?
            .with_context(|| format!("GitHub has no blob for {name}'s policy record"))?,
    };
    let record = match parse_record(&text) {
        Ok(record) => record,
        Err(error) => {
            return Ok(Err(format!(
                "{name}'s contribution policy record is invalid ({error:#})"
            )));
        }
    };

    let key =
        |repo: &str, path: &str| format!("{}:{}", repo.to_lowercase(), path.trim_end_matches('/'));
    let org = format!("{}/{ORG_REPO}", name.split('/').next().unwrap_or_default());
    let mut changes = Vec::new();
    for source in &record.sources {
        let source_repo = source.get("repo").map_or(name.as_str(), String::as_str);
        let path = &source["path"];
        let current = match trees.repo(api, source_repo)? {
            Some(r) => trees.sha(api, &r, path)?,
            None if source_repo.eq_ignore_ascii_case(&org) => None,
            None => bail!("GitHub has no repository {source_repo}, a source of {name}'s policy"),
        };
        match current {
            None => changes.push(format!("{} is gone", key(source_repo, path))),
            Some(now) if now != source["sha"] => changes.push(format!(
                "{} changed ({} -> {})",
                key(source_repo, path),
                &source["sha"][..12],
                now.get(..12).unwrap_or(&now)
            )),
            Some(_) => {}
        }
    }
    let recorded: Vec<_> = record
        .sources
        .iter()
        .map(|s| key(s.get("repo").map_or(name, |r| r), &s["path"]))
        .collect();
    let mut scanned = vec![repo.clone()];
    if !org.eq_ignore_ascii_case(name)
        && let Some(org) = trees.repo(api, &org)?
    {
        scanned.push(org);
    }
    for r in &scanned {
        for dir in SCAN_DIRS {
            for (entry, Entry { kind, .. }) in trees.entries(api, r, dir)? {
                let path = if dir.is_empty() {
                    entry.clone()
                } else {
                    format!("{dir}/{entry}")
                };
                if (kind == "blob" || kind == "tree")
                    && is_policy_source(&entry)
                    && !recorded.contains(&key(&r.0, &path))
                {
                    changes.push(format!("{} is not in the record", key(&r.0, &path)));
                }
            }
        }
    }
    if !changes.is_empty() {
        return Ok(Err(format!(
            "{name}'s contribution policy record is stale, re-check the policy ({})",
            changes.join(", ")
        )));
    }
    Ok(match record.verdict.as_str() {
        "bot-ok" => Ok(format!("bot-ok (checked {})", record.checked)),
        "human-text" => Err(format!(
            "{name}'s contribution policy is human-text, and '{HUMAN_TEXT_COMMAND}' is not ported here: {} writes the text and opens the upstream PR",
            config.approver
        )),
        verdict => Err(format!(
            "{name}'s contribution policy is {verdict}: the bot does not contribute there"
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{cell::RefCell, collections::BTreeSet};

    const HEAD: &str = "1111111111111111111111111111111111111111";
    const OLD: &str = "2222222222222222222222222222222222222222";
    const CONTRIBUTING: &str = "3333333333333333333333333333333333333333";
    const POLICY_TREE: &str = "4444444444444444444444444444444444444444";
    const ACME_TREE: &str = "5555555555555555555555555555555555555555";
    const RECORD_BLOB: &str = "6666666666666666666666666666666666666666";
    const URL: &str = "https://github.com/cgwalters-forge/proj/pull/1";
    const PAGE: &str = "?per_page=100&page=1";
    const BOT_LOGIN: &str = "cgwalters-bot";
    const REVIEWER: &str = "cgwalters";
    const POLICY_REPO: &str = "cgwalters-bot/homegit";
    const POLICY_BRANCH: &str = "main";
    const CONFIG: &str = r#"
bot = "cgwalters-bot"
approver = "cgwalters"
forge = "cgwalters-forge"
policy_repo = "cgwalters-bot/homegit"
policy_branch = "main"
"#;

    fn config() -> Config {
        Config::parse(CONFIG).unwrap()
    }

    fn args(url: &str, dry_run: bool) -> Promote {
        Promote {
            url: url.into(),
            dry_run,
            config: PathBuf::new(),
        }
    }

    /// GitHub as a map of paths to bodies; `None` is a 404, and a path missing from the
    /// map fails the test, so a mistyped request can't pass as a 404. A `then:PATH` entry
    /// answers every request of PATH after the first, as if it changed in between.
    struct Fake {
        responses: BTreeMap<String, Option<String>>,
        requests: RefCell<usize>,
        seen: RefCell<BTreeSet<String>>,
    }

    impl Rest for Fake {
        fn get(&self, path: &str, _raw: bool) -> Result<Option<String>> {
            *self.requests.borrow_mut() += 1;
            let later = format!("then:{path}");
            let key = match self.seen.borrow_mut().insert(path.to_owned()) {
                false if self.responses.contains_key(&later) => later.as_str(),
                _ => path,
            };
            self.responses
                .get(key)
                .cloned()
                .with_context(|| format!("unexpected request {path}"))
        }
    }

    fn record(verdict: &str, sha: &str) -> String {
        format!(
            "---\nverdict: {verdict}\nai-trailer: Generated-by\ndco: no, no DCO app\nsources:\n  - repo: acme/proj\n    path: CONTRIBUTING.md\n    sha: {sha}\nchecked: 2026-10-01 by cgwalters-bot\n---\n\n> Contributions welcome.\n\nRationale.\n"
        )
    }

    fn body(extra: &str) -> String {
        format!(
            "Fix the thing.\r\n\r\nGenerated-by: AI\r\n\r\n{META_START}\r\n- Upstream: `acme/proj`, base `main`\r\n- Board item: `PVTI_abc-1`\r\n{extra}{META_END}\r\n"
        )
    }

    /// An approvable fork PR: the bot's, approved at HEAD, into a bot-ok repository with
    /// a current record, one commit ahead and none behind.
    fn world() -> BTreeMap<String, Option<String>> {
        let mut w = BTreeMap::new();
        set(
            &mut w,
            "repos/cgwalters-forge/proj/pulls/1",
            json!({"user":{"login":BOT_LOGIN},"state":"open","title":"Fix the thing",
                "head":{"ref":"bot/fix","sha":HEAD,"repo":{"full_name":"cgwalters-forge/proj"}},
                "base":{"ref":"main"},"body":body("")}),
        );
        set(
            &mut w,
            "repos/cgwalters-forge/proj",
            json!({"full_name":"cgwalters-forge/proj","parent":{"full_name":"acme/proj"},"source":{"full_name":"acme/proj"}}),
        );
        set(
            &mut w,
            &format!("repos/cgwalters-forge/proj/pulls/1/reviews{PAGE}"),
            json!([{"user":{"login":REVIEWER},"state":"APPROVED","submitted_at":"2026-10-10T10:00:00Z",
                "commit_id":HEAD,"html_url":"https://github.com/cgwalters-forge/proj/pull/1#review-1","body":""}]),
        );
        set(
            &mut w,
            &format!("repos/cgwalters-forge/proj/issues/1/comments{PAGE}"),
            json!([]),
        );
        set(
            &mut w,
            &format!("repos/cgwalters-forge/proj/pulls/1/comments{PAGE}"),
            json!([]),
        );
        set(
            &mut w,
            "repos/acme/proj",
            json!({"full_name":"acme/proj","default_branch":"main"}),
        );
        set(
            &mut w,
            "repos/acme/proj/git/trees/main",
            json!({"truncated":false,"tree":[
                {"path":"CONTRIBUTING.md","sha":CONTRIBUTING,"type":"blob"},
                {"path":"README.md","sha":OLD,"type":"blob"}]}),
        );
        w.insert("repos/acme/.github".into(), None);
        set(
            &mut w,
            &format!("repos/{POLICY_REPO}/git/trees/{POLICY_BRANCH}"),
            json!({"tree":[{"path":POLICY_DIR,"sha":POLICY_TREE,"type":"tree","mode":"040000"}]}),
        );
        set(
            &mut w,
            &format!("repos/{POLICY_REPO}/git/trees/{POLICY_TREE}"),
            json!({"tree":[{"path":"acme","sha":ACME_TREE,"type":"tree","mode":"040000"}]}),
        );
        policy(&mut w, Some(&record("bot-ok", CONTRIBUTING)));
        set(
            &mut w,
            &format!("repos/acme/proj/compare/main...{HEAD}"),
            json!({"ahead_by":1,"behind_by":0}),
        );
        set(
            &mut w,
            "repos/acme/proj/pulls?head=cgwalters-forge%3Abot%2Ffix&state=open&per_page=100&page=1",
            json!([]),
        );
        w
    }

    /// homegit's `upstream-policy/acme/proj.md` as a regular file, or none.
    fn policy(w: &mut BTreeMap<String, Option<String>>, text: Option<&str>) {
        let entries = match text {
            Some(_) => {
                json!([{"path":"proj.md","sha":RECORD_BLOB,"type":"blob","mode":REGULAR_FILE}])
            }
            None => json!([]),
        };
        set(
            w,
            &format!("repos/{POLICY_REPO}/git/trees/{ACME_TREE}"),
            json!({"tree":entries}),
        );
        w.insert(
            format!("repos/{POLICY_REPO}/git/blobs/{RECORD_BLOB}"),
            text.map(str::to_owned),
        );
    }

    fn set(w: &mut BTreeMap<String, Option<String>>, path: &str, value: Value) {
        w.insert(path.to_owned(), Some(value.to_string()));
    }

    fn reviews(w: &mut BTreeMap<String, Option<String>>, value: Value) {
        set(
            w,
            &format!("repos/cgwalters-forge/proj/pulls/1/reviews{PAGE}"),
            value,
        );
    }

    fn comments(w: &mut BTreeMap<String, Option<String>>, value: Value) {
        set(
            w,
            &format!("repos/cgwalters-forge/proj/issues/1/comments{PAGE}"),
            value,
        );
    }

    fn pr(w: &mut BTreeMap<String, Option<String>>, edit: impl Fn(&mut Value)) {
        let path = "repos/cgwalters-forge/proj/pulls/1";
        let mut value: Value = serde_json::from_str(w[path].as_deref().unwrap()).unwrap();
        edit(&mut value);
        set(w, path, value);
    }

    /// The fork's activity log says HEAD was pushed at 10:00, the timeline has a commit
    /// committed at 09:00.
    fn pushed_at_ten(w: &mut BTreeMap<String, Option<String>>) {
        set(
            w,
            "repos/cgwalters-forge/proj/activity?ref=refs%2Fheads%2Fbot%2Ffix&direction=desc&per_page=1",
            json!([{"after":HEAD,"timestamp":"2026-10-10T10:00:00Z"}]),
        );
        set(
            w,
            &format!("repos/cgwalters-forge/proj/issues/1/timeline{PAGE}"),
            json!([{"event":"committed","committer":{"date":"2026-10-10T09:00:00Z"}}]),
        );
    }

    fn timeline(w: &mut BTreeMap<String, Option<String>>, value: Value) {
        set(
            w,
            &format!("repos/cgwalters-forge/proj/issues/1/timeline{PAGE}"),
            value,
        );
    }

    /// cgwalters' approval of HEAD at 10:00 is dismissed at `at`, and they comment
    /// `/promote` at 11:00, after the push at 10:00.
    fn dismissed_and_promoted(w: &mut BTreeMap<String, Option<String>>, at: &str) {
        reviews(
            w,
            json!([{"id":1,"user":{"login":REVIEWER},"state":"DISMISSED",
                "submitted_at":"2026-10-10T10:00:00Z","commit_id":HEAD,"html_url":"u"}]),
        );
        comments(w, promote_comment("2026-10-10T11:00:00Z"));
        pushed_at_ten(w);
        timeline(
            w,
            json!([{"event":"committed","committer":{"date":"2026-10-10T09:00:00Z"}},
                {"event":"review_dismissed","created_at":at,"dismissed_review":{"review_id":1,"state":"approved"}}]),
        );
    }

    fn promote_comment(at: &str) -> Value {
        json!([{"user":{"login":REVIEWER},"body":"Thanks.\r\n /promote \r\n","created_at":at,"updated_at":at,
            "html_url":"https://github.com/cgwalters-forge/proj/pull/1#issuecomment-9"}])
    }

    fn stacked(w: &mut BTreeMap<String, Option<String>>, parent_state: &str, upstream: Value) {
        let stack = "- Stacked on: https://github.com/cgwalters-forge/proj/pull/2; promote waits until that is merged upstream\r\n";
        pr(w, |v| v["body"] = body(stack).into());
        set(
            w,
            "repos/cgwalters-forge/proj/pulls/2",
            json!({"state":parent_state,"created_at":"2026-10-05T00:00:00Z","head":{"ref":"bot/parent","sha":HEAD}}),
        );
        set(
            w,
            "repos/acme/proj/pulls?head=cgwalters-forge%3Abot%2Fparent&state=all&per_page=100&page=1",
            upstream,
        );
    }

    const FORK: &str = "cgwalters-forge/proj";

    /// An upstream PR from `repo`'s bot/parent branch, as `pulls?head=` lists it.
    fn upstream_pr(number: u64, state: &str, merged_at: Option<&str>, repo: &str) -> Value {
        json!({"html_url":format!("https://github.com/acme/proj/pull/{number}"),"state":state,
            "merged_at":merged_at,"head":{"ref":"bot/parent","sha":HEAD,"repo":{"full_name":repo}},
            "base":{"ref":"main"}})
    }

    fn promote(w: BTreeMap<String, Option<String>>) -> Result<String> {
        promote_as(&config(), w)
    }

    fn promote_as(config: &Config, w: BTreeMap<String, Option<String>>) -> Result<String> {
        let fake = Fake {
            responses: w,
            requests: RefCell::new(0),
            seen: RefCell::default(),
        };
        let mut out = Vec::new();
        run(&args(URL, true), config, &Api::new(&fake), &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    type Setup = fn(&mut BTreeMap<String, Option<String>>);

    #[test]
    fn workflow_builds_agent_board_for_cross_repository_calls() {
        let workflow = include_str!("../../../.github/workflows/promote.yml");
        assert!(workflow.contains("  workflow_call:"));
        assert!(workflow.contains("  workflow_dispatch:"));
        let checkout = workflow.split("        with:\n").nth(1).unwrap();
        let checkout = checkout.split("      - run:").next().unwrap();
        // Literal source selection is independent of tracker caller repository/ref
        // and dispatch ref; neither can substitute its own Cargo package.
        assert!(checkout.contains("          repository: cgwalters-forge/agent-board\n"));
        assert!(checkout.contains("          ref: refs/heads/main\n"));
        assert!(checkout.contains("          persist-credentials: false\n"));
        assert!(!checkout.contains("${{"));
        // Who to trust comes from that checkout's config, not from an input.
        let invocation = workflow
            .split("release/agent-board promote ")
            .nth(1)
            .unwrap();
        let invocation = invocation.lines().next().unwrap();
        assert_eq!(
            invocation,
            "\"$URL\" --dry-run --config policy/promote.toml"
        );
    }

    #[test]
    fn earlier_branch_use_merged_after_parent_creation() {
        let mut w = world();
        let mut earlier = upstream_pr(79, "closed", Some("2026-10-06T00:00:00Z"), FORK);
        earlier["head"]["sha"] = OLD.into();
        stacked(&mut w, "closed", json!([earlier]));
        let out = promote(w).unwrap();
        assert!(
            out.contains("closed without being merged upstream"),
            "{out}"
        );
        assert!(!out.contains("Would:"), "{out}");
    }

    /// Ported from homegit's tests/bot-pr-promote-signoff.sh and the cases of the
    /// policy gate: each setup, and what the dry run says (`Ok` would promote and
    /// contains the text; `Err` would refuse with it in the comment).
    #[test]
    fn dry_run_table() {
        let cases: Vec<(&str, Setup, Result<&str, &str>)> = vec![
            (
                "approved",
                |_| {},
                Ok("- open a ready-for-review PR in acme/proj"),
            ),
            (
                "compared head",
                |_| {},
                Ok("from cgwalters-forge:bot/fix at 1111111111111111111111111111111111111111 ("),
            ),
            (
                "board item",
                |_| {},
                Ok("- set board item PVTI_abc-1 to In Review"),
            ),
            (
                "meta stripped",
                |_| {},
                Ok("Title: Fix the thing\n\nFix the thing.\n\nGenerated-by: AI\n"),
            ),
            (
                "unapproved",
                |w| reviews(w, json!([])),
                Err("is not approved by cgwalters"),
            ),
            (
                "someone else's approval",
                |w| {
                    reviews(
                        w,
                        json!([{"user":{"login":"someone"},"state":"APPROVED",
                "submitted_at":"2026-10-10T10:00:00Z","commit_id":HEAD,"html_url":"u"}]),
                    )
                },
                Err("is not approved by cgwalters"),
            ),
            (
                "stale approval",
                |w| {
                    reviews(
                        w,
                        json!([{"user":{"login":REVIEWER},"state":"APPROVED",
                "submitted_at":"2026-10-10T10:00:00Z","commit_id":OLD,"html_url":"u"}]),
                    )
                },
                Err("commits pushed after cgwalters approved"),
            ),
            (
                "changes requested after",
                |w| {
                    reviews(
                        w,
                        json!([
                {"user":{"login":REVIEWER},"state":"APPROVED","submitted_at":"2026-10-10T10:00:00Z","commit_id":HEAD,"html_url":"u"},
                {"user":{"login":REVIEWER},"state":"CHANGES_REQUESTED","submitted_at":"2026-10-10T11:00:00Z","commit_id":HEAD,"html_url":"u"}]),
                    )
                },
                Err("is not approved by cgwalters"),
            ),
            (
                "comment review ignored",
                |w| {
                    reviews(
                        w,
                        json!([
                {"user":{"login":REVIEWER},"state":"APPROVED","submitted_at":"2026-10-10T10:00:00Z","commit_id":HEAD,"html_url":"u"},
                {"user":{"login":REVIEWER},"state":"COMMENTED","submitted_at":"2026-10-10T11:00:00Z","commit_id":HEAD,"html_url":"u"}]),
                    )
                },
                Ok("Would:"),
            ),
            (
                "dismissed",
                |w| {
                    reviews(
                        w,
                        json!([{"id":1,"user":{"login":REVIEWER},"state":"DISMISSED",
                "submitted_at":"2026-10-10T10:00:00Z","commit_id":HEAD,"html_url":"u"}]),
                    );
                    timeline(w, json!([]));
                },
                Err("is not approved by cgwalters"),
            ),
            (
                "dismissed after /promote",
                |w| {
                    dismissed_and_promoted(w, "2026-10-10T12:00:00Z");
                },
                Err("is not approved by cgwalters"),
            ),
            (
                "dismissed before /promote",
                |w| {
                    dismissed_and_promoted(w, "2026-10-10T10:45:00Z");
                },
                Ok("APPROVED by /promote"),
            ),
            (
                "dismissal of another review",
                |w| {
                    dismissed_and_promoted(w, "2026-10-10T10:45:00Z");
                    reviews(
                        w,
                        json!([{"id":1,"user":{"login":REVIEWER},"state":"DISMISSED",
                "submitted_at":"2026-10-10T10:00:00Z","commit_id":HEAD,"html_url":"u"},
                {"id":2,"user":{"login":REVIEWER},"state":"DISMISSED",
                "submitted_at":"2026-10-10T10:01:00Z","commit_id":HEAD,"html_url":"u"}]),
                    );
                },
                Err("is not approved by cgwalters"),
            ),
            (
                "/promote after the push",
                |w| {
                    reviews(w, json!([]));
                    comments(w, promote_comment("2026-10-10T10:30:00Z"));
                    pushed_at_ten(w);
                },
                Ok("APPROVED by /promote"),
            ),
            (
                "/promote before the push",
                |w| {
                    reviews(w, json!([]));
                    comments(w, promote_comment("2026-10-10T09:30:00Z"));
                    pushed_at_ten(w);
                },
                Err("commits pushed after cgwalters approved"),
            ),
            (
                "/promote before a later commit date",
                |w| {
                    reviews(w, json!([]));
                    comments(w, promote_comment("2026-10-10T10:30:00Z"));
                    pushed_at_ten(w);
                    set(
                        w,
                        &format!("repos/cgwalters-forge/proj/issues/1/timeline{PAGE}"),
                        json!([{"event":"head_ref_force_pushed","created_at":"2026-10-10T11:00:00Z"}]),
                    );
                },
                Err("commits pushed after cgwalters approved"),
            ),
            (
                "/promote, push unknown",
                |w| {
                    reviews(w, json!([]));
                    comments(w, promote_comment("2026-10-10T10:30:00Z"));
                    set(
                        w,
                        "repos/cgwalters-forge/proj/activity?ref=refs%2Fheads%2Fbot%2Ffix&direction=desc&per_page=1",
                        json!([{"after":OLD,"timestamp":"2026-10-10T10:00:00Z"}]),
                    );
                    timeline(w, json!([]));
                },
                Err("when its head was pushed can't be told"),
            ),
            (
                "/promote by someone else",
                |w| {
                    reviews(w, json!([]));
                    comments(
                        w,
                        json!([{"user":{"login":"someone"},"body":"/promote","created_at":"2026-10-10T10:30:00Z"}]),
                    );
                },
                Err("is not approved by cgwalters"),
            ),
            (
                "/promote not on its own line",
                |w| {
                    reviews(w, json!([]));
                    comments(
                        w,
                        json!([{"user":{"login":REVIEWER},"body":"please /promote","created_at":"2026-10-10T10:30:00Z"}]),
                    );
                },
                Err("is not approved by cgwalters"),
            ),
            (
                "/draft",
                |w| {
                    comments(
                        w,
                        json!([{"user":{"login":REVIEWER},"body":"/draft","created_at":"2026-10-10T11:00:00Z","updated_at":"2026-10-10T11:00:00Z"}]),
                    )
                },
                Ok("- open a draft PR"),
            ),
            (
                "/draft then /ready",
                |w| {
                    comments(
                        w,
                        json!([
                {"user":{"login":REVIEWER},"body":"/draft","created_at":"2026-10-10T11:00:00Z","updated_at":"2026-10-10T11:00:00Z"},
                {"user":{"login":REVIEWER},"body":"/ready","created_at":"2026-10-10T12:00:00Z","updated_at":"2026-10-10T12:00:00Z"}]),
                    )
                },
                Ok("- open a ready-for-review PR"),
            ),
            (
                "behind",
                |w| {
                    set(
                        w,
                        &format!("repos/acme/proj/compare/main...{HEAD}"),
                        json!({"ahead_by":1,"behind_by":3}),
                    )
                },
                Ok("- rebase bot/fix onto acme/proj:main and force-push it"),
            ),
            (
                "already open upstream",
                |w| {
                    set(
                        w,
                        "repos/acme/proj/pulls?head=cgwalters-forge%3Abot%2Ffix&state=open&per_page=100&page=1",
                        json!([{"html_url":"https://github.com/acme/proj/pull/7",
                            "head":{"repo":{"full_name":"cgwalters-forge/proj"}},
                            "base":{"ref":"main","repo":{"full_name":"acme/proj"}}}]),
                    )
                },
                Ok("- reuse the already open https://github.com/acme/proj/pull/7"),
            ),
            (
                "same owner and branch in a different fork",
                |w| {
                    set(
                        w,
                        "repos/acme/proj/pulls?head=cgwalters-forge%3Abot%2Ffix&state=open&per_page=100&page=1",
                        json!([{"html_url":"https://github.com/acme/proj/pull/8",
                            "head":{"repo":{"full_name":"cgwalters-forge/other"}},
                            "base":{"ref":"main","repo":{"full_name":"acme/proj"}}}]),
                    )
                },
                Ok("- open a ready-for-review PR"),
            ),
            (
                "same fork with a different target base",
                |w| {
                    set(
                        w,
                        "repos/acme/proj/pulls?head=cgwalters-forge%3Abot%2Ffix&state=open&per_page=100&page=1",
                        json!([{"html_url":"https://github.com/acme/proj/pull/8",
                            "head":{"repo":{"full_name":"cgwalters-forge/proj"}},
                            "base":{"ref":"stable","repo":{"full_name":"acme/proj"}}}]),
                    )
                },
                Ok("- open a ready-for-review PR"),
            ),
            (
                "matching upstream on a later page",
                |w| {
                    let unrelated = json!({"html_url":"https://github.com/acme/proj/pull/8",
                        "head":{"repo":{"full_name":"cgwalters-forge/other"}},
                        "base":{"ref":"main","repo":{"full_name":"acme/proj"}}});
                    set(
                        w,
                        "repos/acme/proj/pulls?head=cgwalters-forge%3Abot%2Ffix&state=open&per_page=100&page=1",
                        json!(vec![unrelated; PER_PAGE]),
                    );
                    set(
                        w,
                        "repos/acme/proj/pulls?head=cgwalters-forge%3Abot%2Ffix&state=open&per_page=100&page=2",
                        json!([{"html_url":"https://github.com/acme/proj/pull/7",
                            "head":{"repo":{"full_name":"cgwalters-forge/proj"}},
                            "base":{"ref":"main","repo":{"full_name":"acme/proj"}}}]),
                    );
                },
                Ok("- reuse the already open https://github.com/acme/proj/pull/7"),
            ),
            (
                "policy missing",
                |w| {
                    policy(w, None);
                },
                Err("acme/proj has no contribution policy record yet"),
            ),
            (
                "policy human-text",
                |w| {
                    policy(w, Some(&record("human-text", CONTRIBUTING)));
                },
                Err("policy is human-text"),
            ),
            (
                "policy human-only",
                |w| {
                    policy(w, Some(&record("human-only", CONTRIBUTING)));
                },
                Err("policy is human-only"),
            ),
            (
                "policy no-go",
                |w| {
                    policy(w, Some(&record("no-go", CONTRIBUTING)));
                },
                Err("policy is no-go"),
            ),
            (
                "policy invalid",
                |w| {
                    policy(w, Some(&record("maybe", CONTRIBUTING)));
                },
                Err("record is invalid (unknown verdict 'maybe')"),
            ),
            (
                "policy source changed",
                |w| {
                    policy(w, Some(&record("bot-ok", OLD)));
                },
                Err(
                    "stale, re-check the policy (acme/proj:CONTRIBUTING.md changed (222222222222 -> 333333333333))",
                ),
            ),
            (
                "policy source unrecorded",
                |w| {
                    set(
                        w,
                        "repos/acme/proj/git/trees/main",
                        json!({"tree":[
                {"path":"CONTRIBUTING.md","sha":CONTRIBUTING,"type":"blob"},
                {"path":"AI-POLICY.md","sha":OLD,"type":"blob"}]}),
                    )
                },
                Err("acme/proj:AI-POLICY.md is not in the record"),
            ),
            (
                "policy source in the org",
                |w| {
                    set(
                        w,
                        "repos/acme/.github",
                        json!({"full_name":"acme/.github","default_branch":"main"}),
                    );
                    set(
                        w,
                        "repos/acme/.github/git/trees/main",
                        json!({"tree":[{"path":"AGENTS.md","sha":OLD,"type":"blob"}]}),
                    );
                },
                Err("acme/.github:AGENTS.md is not in the record"),
            ),
            (
                "policy source gone",
                |w| set(w, "repos/acme/proj/git/trees/main", json!({"tree":[]})),
                Err("acme/proj:CONTRIBUTING.md is gone"),
            ),
            (
                "policy record a symlink",
                |w| {
                    set(
                        w,
                        &format!("repos/{POLICY_REPO}/git/trees/{ACME_TREE}"),
                        json!({"tree":[{"path":"proj.md","sha":RECORD_BLOB,"type":"blob","mode":"120000"}]}),
                    );
                },
                Err("acme/proj's contribution policy record is not a regular file"),
            ),
            (
                "policy rebase invalid",
                |w| {
                    policy(
                        w,
                        Some(
                            &record("bot-ok", CONTRIBUTING)
                                .replace("checked:", "rebase: always\nchecked:"),
                        ),
                    )
                },
                Err("'rebase' can only be conflicts-only or any"),
            ),
            (
                "policy rebase valid",
                |w| {
                    policy(
                        w,
                        Some(
                            &record("bot-ok", CONTRIBUTING)
                                .replace("checked:", "rebase: conflicts-only\nchecked:"),
                        ),
                    )
                },
                Ok("Would:"),
            ),
            (
                "/draft in an inline review comment",
                |w| {
                    set(
                        w,
                        &format!("repos/cgwalters-forge/proj/pulls/1/comments{PAGE}"),
                        json!([{"user":{"login":REVIEWER},"body":"/draft","updated_at":"2026-10-10T11:00:00Z"}]),
                    )
                },
                Ok("- open a draft PR"),
            ),
            (
                "stacked, parent open",
                |w| stacked(w, "open", json!([])),
                Err("which is not promoted yet"),
            ),
            (
                "stacked, parent promoted",
                |w| stacked(w, "closed", json!([upstream_pr(80, "open", None, FORK)])),
                Err("whose upstream PR https://github.com/acme/proj/pull/80 is not merged yet"),
            ),
            (
                "stacked, parent dropped",
                |w| stacked(w, "closed", json!([upstream_pr(80, "closed", None, FORK)])),
                Err("which was closed without being merged upstream"),
            ),
            (
                "stacked, parent merged",
                |w| {
                    stacked(
                        w,
                        "closed",
                        json!([upstream_pr(
                            80,
                            "closed",
                            Some("2026-10-11T00:00:00Z"),
                            FORK
                        )]),
                    )
                },
                Ok("Would:"),
            ),
            (
                "stacked, an earlier use of the parent's branch merged",
                |w| {
                    stacked(
                        w,
                        "closed",
                        json!([
                            upstream_pr(80, "closed", None, FORK),
                            upstream_pr(70, "closed", Some("2026-10-01T00:00:00Z"), FORK)
                        ]),
                    )
                },
                Err("which was closed without being merged upstream"),
            ),
            (
                "stacked, promoted, an earlier use of the parent's branch merged",
                |w| {
                    stacked(
                        w,
                        "closed",
                        json!([
                            upstream_pr(80, "open", None, FORK),
                            upstream_pr(70, "closed", Some("2026-10-01T00:00:00Z"), FORK)
                        ]),
                    )
                },
                Err("whose upstream PR https://github.com/acme/proj/pull/80 is not merged yet"),
            ),
            (
                "stacked, parent merged into a different upstream base",
                |w| {
                    let mut merged = upstream_pr(80, "closed", Some("2026-10-11T00:00:00Z"), FORK);
                    merged["base"]["ref"] = "stable".into();
                    stacked(w, "closed", json!([merged]))
                },
                Err("which was closed without being merged upstream"),
            ),
            (
                "stacked, merged from another of the owner's repositories",
                |w| {
                    stacked(
                        w,
                        "closed",
                        json!([upstream_pr(
                            80,
                            "closed",
                            Some("2026-10-11T00:00:00Z"),
                            "cgwalters-forge/other"
                        )]),
                    )
                },
                Err("which was closed without being merged upstream"),
            ),
            (
                "several refusals in one comment",
                |w| {
                    reviews(w, json!([]));
                    policy(w, None);
                },
                Err(
                    "Not promoted: it is not approved by cgwalters; only an approving review by cgwalters or a comment of theirs with a '/promote' line promotes a fork PR; acme/proj has no contribution policy record yet",
                ),
            ),
        ];
        for (name, setup, expected) in cases {
            let mut w = world();
            setup(&mut w);
            let out = promote(w).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            match expected {
                Ok(text) => {
                    assert!(out.contains("\nWould:\n"), "{name}: refused:\n{out}");
                    assert!(out.contains(text), "{name}: no {text:?} in:\n{out}");
                }
                Err(text) => {
                    assert!(
                        out.contains(&format!("Would comment on {URL} and stop:\nNot promoted: ")),
                        "{name}: not refused:\n{out}"
                    );
                    assert!(!out.contains("\nWould:\n"), "{name}: {out}");
                    assert!(out.contains(text), "{name}: no {text:?} in:\n{out}");
                }
            }
        }
    }

    /// What `bot-pr promote` refuses outright, even in a dry run.
    #[test]
    fn dry_run_errors() {
        let cases: Vec<(&str, Setup, &str)> = vec![
            (
                "someone else's PR",
                |w| pr(w, |v| v["user"]["login"] = "someone".into()),
                "was opened by someone, not cgwalters-bot",
            ),
            (
                "closed",
                |w| pr(w, |v| v["state"] = "closed".into()),
                "is closed; only an open fork PR",
            ),
            (
                "head elsewhere",
                |w| pr(w, |v| v["head"]["repo"]["full_name"] = "other/proj".into()),
                "head is in other/proj",
            ),
            (
                "not a bot branch",
                |w| pr(w, |v| v["head"]["ref"] = "main".into()),
                "is not a bot/<slug> branch",
            ),
            (
                "bare prefix",
                |w| pr(w, |v| v["head"]["ref"] = "bot/".into()),
                "is not a bot/<slug> branch",
            ),
            (
                "no meta",
                |w| pr(w, |v| v["body"] = "Fix.".into()),
                "has no complete <!-- bot-meta -->",
            ),
            (
                "unterminated meta",
                |w| {
                    pr(w, |v| {
                        v["body"] =
                            format!("Fix.\n{META_START}\n- Upstream: `acme/proj`, base `main`\n")
                                .into()
                    })
                },
                "has no complete <!-- bot-meta -->",
            ),
            (
                "no upstream line",
                |w| {
                    pr(w, |v| {
                        v["body"] = format!("Fix.\n{META_START}\n{META_END}").into()
                    })
                },
                "cannot find the 'Upstream:",
            ),
            (
                "invalid upstream",
                |w| {
                    pr(w, |v| {
                        v["body"] = format!(
                            "Fix.\n{META_START}\n- Upstream: `../x`, base `main`\n{META_END}"
                        )
                        .into()
                    })
                },
                "invalid upstream '../x'",
            ),
            (
                "invalid base",
                |w| {
                    pr(w, |v| {
                        v["body"] = format!(
                            "Fix.\n{META_START}\n- Upstream: `acme/proj`, base `a..b`\n{META_END}"
                        )
                        .into()
                    })
                },
                "invalid base 'a..b'",
            ),
            (
                "empty body",
                |w| {
                    pr(w, |v| {
                        v["body"] = format!(
                            "{META_START}\n- Upstream: `acme/proj`, base `main`\n{META_END}"
                        )
                        .into()
                    })
                },
                "body is empty without the bot-meta section",
            ),
            (
                "second marker",
                |w| {
                    pr(w, |v| {
                        v["body"] = format!("Fix. bot-meta\n{META_START}\n- Upstream: `acme/proj`, base `main`\n{META_END}").into()
                    })
                },
                "still mentions bot-meta",
            ),
            (
                "not in the fork's network",
                |w| {
                    set(
                        w,
                        "repos/cgwalters-forge/proj",
                        json!({"parent":{"full_name":"other/proj"},"source":{"full_name":"other/proj"}}),
                    )
                },
                "bot-meta names upstream acme/proj, but cgwalters-forge/proj is a fork of other/proj",
            ),
            (
                "stacked outside the forge",
                |w| {
                    pr(w, |v| {
                        v["body"] =
                            body("- Stacked on: https://github.com/acme/proj/pull/2\n").into()
                    })
                },
                "invalid 'Stacked on'",
            ),
            (
                "merged already",
                |w| {
                    set(
                        w,
                        &format!("repos/acme/proj/compare/main...{HEAD}"),
                        json!({"ahead_by":0,"behind_by":2}),
                    )
                },
                "has no commits on top of acme/proj:main",
            ),
            (
                "branch advanced during the check",
                |w| {
                    let path = "repos/cgwalters-forge/proj/pulls/1";
                    let mut moved: Value =
                        serde_json::from_str(w[path].as_deref().unwrap()).unwrap();
                    moved["head"]["sha"] = OLD.into();
                    set(w, &format!("then:{path}"), moved);
                },
                "changed while it was checked (head 2222222222222222222222222222222222222222, was 1111111111111111111111111111111111111111)",
            ),
            (
                "too many reviews",
                |w| {
                    let page = Value::Array(vec![json!({"user":{"login":"someone"}}); PER_PAGE]);
                    for n in 1..=PAGE_LIMIT {
                        set(
                            w,
                            &format!(
                                "repos/cgwalters-forge/proj/pulls/1/reviews?per_page=100&page={n}"
                            ),
                            page.clone(),
                        );
                    }
                },
                "has more than 1000 entries",
            ),
            (
                "upstream renamed",
                |w| {
                    set(
                        w,
                        "repos/acme/proj",
                        json!({"full_name":"acme/project","default_branch":"main"}),
                    )
                },
                "acme/proj was renamed to acme/project",
            ),
            (
                "policy source renamed",
                |w| {
                    policy(
                        w,
                        Some(
                            &record("bot-ok", CONTRIBUTING)
                                .replace("repo: acme/proj", "repo: acme/guide"),
                        ),
                    );
                    set(
                        w,
                        "repos/acme/guide",
                        json!({"full_name":"acme/handbook","default_branch":"main"}),
                    );
                },
                "acme/guide was renamed to acme/handbook",
            ),
            (
                "truncated tree",
                |w| {
                    set(
                        w,
                        "repos/acme/proj/git/trees/main",
                        json!({"truncated":true,"tree":[]}),
                    )
                },
                "is truncated",
            ),
        ];
        for (name, setup, expected) in cases {
            let mut w = world();
            setup(&mut w);
            let error = format!("{:#}", promote(w).expect_err(name));
            assert!(error.contains(expected), "{name}: {error}");
        }
    }

    #[test]
    fn request_budget_is_enforced() {
        let fake = Fake {
            responses: world(),
            requests: RefCell::new(0),
            seen: RefCell::default(),
        };
        let api = Api::new(&fake);
        api.remaining.set(3);
        let error = run(&args(URL, true), &config(), &api, &mut Vec::new()).unwrap_err();
        assert!(format!("{error:#}").contains("budget"));
        assert_eq!(*fake.requests.borrow(), 3);
    }

    #[test]
    fn urls_and_args() {
        for (url, ok) in [
            (URL, true),
            ("https://github.com/cgwalters-forge/proj/pull/1/", true),
            ("https://github.com/cgwalters-bot/proj/pull/12", true),
            ("https://github.com/acme/proj/pull/1", false),
            ("https://github.com/cgwalters-forge/proj/issues/1", false),
            ("https://github.com/cgwalters-forge/proj/pull/0", false),
            ("https://github.com/cgwalters-forge/proj/pull/01", false),
            ("https://github.com/cgwalters-forge/../pull/1", false),
            (
                "https://github.com/cgwalters-forge/proj/pull/1/files",
                false,
            ),
            ("http://github.com/cgwalters-forge/proj/pull/1", false),
            ("https://github.example/cgwalters-forge/proj/pull/1", false),
        ] {
            assert_eq!(check_args(&args(url, true), &config()).is_ok(), ok, "{url}");
        }
        let error = check_args(&args(URL, false), &config()).unwrap_err();
        assert!(error.to_string().contains("--dry-run"));
    }

    /// The shipped config parses; a missing key is an error naming it, and an unknown key
    /// or a malformed value is refused.
    #[test]
    fn config_table() {
        let shipped = include_str!("../../../policy/promote.toml");
        let shipped = Config::parse(shipped).unwrap();
        assert_eq!(shipped.fork_owners(), config().fork_owners());
        for key in ["bot", "approver", "forge", "policy_repo", "policy_branch"] {
            let input: String = CONFIG
                .lines()
                .filter(|l| !l.starts_with(&format!("{key} =")))
                .map(|l| format!("{l}\n"))
                .collect();
            let error = format!("{:#}", Config::parse(&input).unwrap_err());
            assert!(error.contains(&format!("missing field `{key}`")), "{error}");
        }
        for (edit, expected) in [
            ("extra = \"x\"\n", "unknown field `extra`"),
            ("approver = \"\"\n", "approver '' is not a GitHub login"),
            ("forge = \"a/b\"\n", "forge 'a/b' is not a GitHub login"),
            (
                "policy_repo = \"homegit\"\n",
                "policy_repo 'homegit' is not",
            ),
            ("policy_branch = \"../x\"\n", "policy_branch '../x' is not"),
        ] {
            let key = edit.split(' ').next().unwrap();
            let input: String = CONFIG
                .lines()
                .filter(|l| !l.starts_with(&format!("{key} =")))
                .map(|l| format!("{l}\n"))
                .chain([edit.to_owned()])
                .collect();
            let error = format!("{:#}", Config::parse(&input).unwrap_err());
            assert!(error.contains(expected), "{edit}: {error}");
        }
        // The configured approver is the only one whose approval counts.
        let other = Config {
            approver: "someone".into(),
            ..config()
        };
        let out = promote_as(&other, world()).unwrap();
        assert!(out.contains("it is not approved by someone"), "{out}");
    }

    #[test]
    fn policy_symlinks_are_refused() {
        // The link SHA stays fixed while its unrecognized target's policy changes.
        for target_sha in [OLD, HEAD] {
            let mut w = world();
            set(
                &mut w,
                "repos/acme/proj/git/trees/main",
                json!({"tree":[
                    {"path":"CONTRIBUTING.md","sha":CONTRIBUTING,"type":"blob","mode":"120000"},
                    {"path":"docs","sha":OLD,"type":"tree","mode":"040000"}]}),
            );
            set(
                &mut w,
                &format!("repos/acme/proj/git/trees/{OLD}"),
                json!({"tree":[{"path":"contribution-guide.md","sha":target_sha,"type":"blob","mode":REGULAR_FILE}]}),
            );
            let error = promote(w).unwrap_err().to_string();
            assert!(error.contains("CONTRIBUTING.md is a symlink"), "{error}");
        }

        for dir in SCAN_DIRS.into_iter().filter(|dir| !dir.is_empty()) {
            let mut w = world();
            set(
                &mut w,
                "repos/acme/proj/git/trees/main",
                json!({"tree":[
                    {"path":"CONTRIBUTING.md","sha":CONTRIBUTING,"type":"blob","mode":REGULAR_FILE},
                    {"path":dir,"sha":OLD,"type":"blob","mode":"120000"}]}),
            );
            let error = promote(w).unwrap_err().to_string();
            assert!(error.contains(&format!("{dir} is a symlink")), "{error}");
        }
    }

    #[test]
    fn policy_source_names() {
        for (name, source) in [
            ("CONTRIBUTING.md", true),
            ("contributing", true),
            ("AGENTS.md", true),
            ("CLAUDE.md", true),
            ("copilot-instructions.md", true),
            ("PULL_REQUEST_TEMPLATE.md", true),
            ("DCO", true),
            ("AI-POLICY.md", true),
            ("llm_usage.md", true),
            ("ai", true),
            (".ai", true),
            ("README.md", false),
            ("maintainers.md", false),
            ("trail.md", false),
            ("agents-extra.md", false),
        ] {
            assert_eq!(is_policy_source(name), source, "{name}");
        }
    }

    #[test]
    fn record_parsing() {
        assert!(parse_record(&record("bot-ok", CONTRIBUTING)).is_ok());
        let empty = "---\nverdict: bot-ok\nai-trailer: none\ndco: yes\nsources: []\nchecked: today\n---\nbody\n";
        assert!(parse_record(empty).unwrap().sources.is_empty());
        for (text, why) in [
            ("verdict: bot-ok\n", "starts with a '---'"),
            ("---\nverdict: bot-ok\n", "no closing"),
            (&empty.replace("verdict: bot-ok\n", ""), "no 'verdict'"),
            (
                &empty.replace("verdict: bot-ok\n", "verdict: bot-ok\nverdict: no-go\n"),
                "set twice",
            ),
            (
                &empty.replace("dco: yes", "dco: yesterday"),
                "'dco' starts with yes or no",
            ),
            (&empty.replace("---\nbody\n", "---\n\n"), "no body"),
            (&record("bot-ok", "abc"), "no 40-hex 'sha'"),
            (
                &record("bot-ok", CONTRIBUTING).replace("    path:", "    size:"),
                "unknown source key",
            ),
            (
                &record("bot-ok", CONTRIBUTING).replace("repo: acme/proj", "repo: ../x"),
                "invalid repo",
            ),
            (
                &empty.replace("sources: []", "sources: x"),
                "followed by a list",
            ),
            (
                &empty.replace("checked: today", "Checked: today"),
                "cannot parse",
            ),
        ] {
            let error = format!("{:#}", parse_record(text).unwrap_err());
            assert!(error.contains(why), "{why}: {error}");
        }
    }
}
