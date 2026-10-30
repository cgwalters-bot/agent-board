//! Conflict avoidance before a dispatch (cgwalters-forge/agent-board#88): an
//! implement run that changes the files an open pull request of the bot
//! changes, or that a run still in flight is likely changing, ends in rebase
//! fix rounds, and a pull request can run out of them (agentic-job #640-#642
//! all changed the safe-outputs code). So before dispatching, the issue is
//! compared with the bot's open pull requests and with the implement runs in
//! flight on the repository the plan changes, and the run is held back while
//! it overlaps either, or else run independently.
//!
//! Waiting, not stacking: a guess from named paths does not choose a base.
//! Only a plan that names the slice it stacks on starts from another pull
//! request's head ([`crate::stack`]), and that run is not checked here: it
//! shares the files of the stack below it by design.
//!
//! Off unless the caller's configuration names the bot (`dispatch --bot`).
//! The overlap is a coarse guess: the paths an issue's text names, against
//! the files a pull request touches or the paths a running issue names. It is
//! a scheduling heuristic, not a boundary: a read that fails runs the issue
//! independently, as without the check. See docs/job-plans.md#overlap.
//!
//! The pass may waive an open pull request's overlap ([`Waiver`],
//! agent-board#426): a draft nobody is moving races no run, and a conflict
//! in documentation is cheap to resolve for the board's most urgent work.
use crate::github::{API_ROOT, Transport};
use crate::plan::Target;
use crate::poll::{Rest, validate_repository};
use anyhow::{Context, Result, ensure};
use board_core::ItemRef;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// Paths read from one issue's text, at most, so that a pasted listing
/// costs a bounded comparison.
pub const PATH_LIMIT: usize = 50;
/// Open pull requests read, newest first, and files read of each. An older
/// one, or a file past the limit, is not compared.
pub const PULL_READS: u64 = 50;
pub const FILE_READS: u64 = 100;
/// Running issues whose text is read, at most.
pub const RUN_READS: usize = 10;
/// Requests spent on [`Waiver::stale_draft`], at most, so
/// that the check's requests stay within what a start reserves; one past it
/// holds.
pub const DRAFT_READS: usize = 4;
/// Paths named in a decision, at most.
const SHOWN: usize = 3;

const PULLS_QUERY: &str = r#"query($owner: String!, $name: String!, $pulls: Int!, $files: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequests(states: OPEN, first: $pulls, orderBy: {field: CREATED_AT, direction: DESC}) {
      nodes { number author { login } files(first: $files) { nodes { path } } }
    }
  }
}"#;

pub(crate) const TEXT_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) { issue(number: $number) { title body } }
}"#;

/// Draft identity and current head; commit dates cannot date a push of an
/// old commit. Read only where [`Waiver::stale_draft`] asks.
pub(crate) const DRAFT_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) { pullRequest(number: $number) {
    isDraft createdAt headRefOid headRefName headRepository { nameWithOwner }
  } }
}"#;

/// What the check decided for a dispatch.
#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    /// Nothing overlaps, or the check was off or could not read; the note
    /// says which.
    Independent(String),
    /// Only open pull requests a [`Waiver`] waives overlap: run
    /// independently, and say which and why.
    Ignored(String),
    /// Dispatch nothing now: the next pass decides again. With the open
    /// pull requests of the destination's repository that hold it, by
    /// number: none where a run in flight holds it or the check could not
    /// read.
    Wait(String, Vec<u64>),
}

impl Decision {
    /// The sentence the dispatch record carries.
    pub fn sentence(&self) -> String {
        match self {
            Self::Independent(why) => format!("Run independently: {why}."),
            Self::Ignored(why) => format!("Run independently: overlap ignored: {why}."),
            Self::Wait(why, _) => format!("Waiting: {why}."),
        }
    }
}

/// When an open pull request's overlap does not hold a run, as the pass's
/// `overlap` setting asks; the default waives nothing. Runs in flight are
/// never waived: they have not pushed, so nobody can tell what they change.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Waiver {
    /// The pass's clock and `overlap.staleDraftHours`: a draft opened and
    /// last pushed to at least that many hours before does not hold, only
    /// with complete authenticated activity for its current head branch.
    pub stale_draft: Option<(OffsetDateTime, u64)>,
    /// Whether the waiting issue is in the board's first Priority option:
    /// then a pull request that shares only documentation does not hold.
    pub top: bool,
}

/// Whether a path is documentation for [`Waiver::top`]: Markdown, or
/// anything under `docs/`.
fn documentation(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".md") || path.starts_with("docs/")
}

impl Waiver {
    /// Why pull request `pull` of `dest`'s repository, which shares
    /// `theirs`, does not hold the run, if it does not. A draft read spends
    /// one of `reads`; with none left, it holds.
    fn waives<T: Transport + Rest>(
        &self,
        client: &T,
        (dest, pull): (&ItemRef, u64),
        theirs: &[String],
        reads: &mut usize,
    ) -> Result<Option<String>> {
        if self.top && theirs.iter().all(|p| documentation(p)) {
            return Ok(Some(
                "it shares only documentation, and the issue is in the first Priority".into(),
            ));
        }
        let Some((now, hours)) = self.stale_draft.filter(|_| *reads > 0) else {
            return Ok(None);
        };
        *reads -= 1;
        let (owner, name) = owner_name(dest)?;
        let data = client.graphql(
            DRAFT_QUERY,
            json!({"owner": owner, "name": name, "number": pull}),
        )?;
        let node = data
            .pointer("/repository/pullRequest")
            .filter(|n| n.is_object())
            .with_context(|| format!("pull request {pull} is not readable"))?;
        if node["isDraft"] != Value::Bool(true) {
            return Ok(None);
        }
        let Some(opened) = timestamp(&node["createdAt"]) else {
            return Ok(None);
        };
        if (now - opened).whole_hours() < i64::try_from(hours)? || *reads == 0 {
            return Ok(None);
        }
        let Some(repo) = node
            .pointer("/headRepository/nameWithOwner")
            .and_then(Value::as_str)
        else {
            return Ok(None);
        };
        validate_repository(repo)?;
        let Some(branch) = node["headRefName"].as_str().filter(|b| !b.is_empty()) else {
            return Ok(None);
        };
        let Some(head) = node["headRefOid"].as_str().filter(|h| oid(h)) else {
            return Ok(None);
        };
        let reference = format!("refs/heads/{branch}");
        let encoded: String = url::form_urlencoded::byte_serialize(reference.as_bytes()).collect();
        *reads -= 1;
        // Unlike the public Events feed or commit pushedDate, repository activity
        // records branch changes, including force pushes back to an old commit.
        // No actor/type/time filter: omissions could hide a newer head change.
        let response = client.get(
            &format!(
                "{API_ROOT}repos/{repo}/activity?ref={encoded}&direction=desc&per_page={FILE_READS}"
            ),
            None,
        )?;
        if response.status != 200 || response.link.is_some() {
            return Ok(None);
        }
        let Some(events) = response
            .body
            .as_array()
            .filter(|e| !e.is_empty() && e.len() < FILE_READS as usize)
        else {
            return Ok(None);
        };
        // Require a complete, ordered chain ending at the PR head. A partial
        // page, unknown activity, or a different branch/head proves no idle time.
        let mut expected = head;
        let mut previous = now;
        for event in events {
            let Some(at) = timestamp(&event["timestamp"]) else {
                return Ok(None);
            };
            let Some(before) = event["before"].as_str().filter(|h| oid(h)) else {
                return Ok(None);
            };
            if event["ref"] != reference
                || event["after"] != expected
                || at > previous
                || !matches!(
                    event["activity_type"].as_str(),
                    Some(
                        "push"
                            | "force_push"
                            | "branch_creation"
                            | "branch_deletion"
                            | "pr_merge"
                            | "merge_queue_merge"
                    )
                )
            {
                return Ok(None);
            }
            expected = before;
            previous = at;
        }
        if expected != "0".repeat(40)
            || events.last().map(|e| &e["activity_type"]) != Some(&json!("branch_creation"))
        {
            return Ok(None);
        }
        let Some(changed) = timestamp(&events[0]["timestamp"]).map(|pushed| opened.max(pushed))
        else {
            return Ok(None);
        };
        let idle = (now - changed).whole_hours();
        Ok((idle >= i64::try_from(hours)?)
            .then(|| format!("it is a draft whose head has not changed for {idle} hours")))
    }
}

fn timestamp(value: &Value) -> Option<OffsetDateTime> {
    value
        .as_str()
        .and_then(|at| OffsetDateTime::parse(at, &Rfc3339).ok())
}

fn oid(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(crate) fn shown(paths: &[String]) -> String {
    let mut out: Vec<String> = paths.iter().take(SHOWN).map(|p| format!("`{p}`")).collect();
    if paths.len() > SHOWN {
        out.push(format!("{} more", paths.len() - SHOWN));
    }
    out.join(", ")
}

/// The paths a text names: tokens of path characters whose last part has a
/// file extension or is a dotfile (`.gitignore`), or that end in `/` (a
/// directory). URLs, repository names (`OWNER/REPO`), `host.name/...` and
/// abbreviations (`e.g`) are left out, since they name no file.
pub fn paths(text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for token in text.split(|c: char| !pathlike(c)) {
        if found.len() == PATH_LIMIT {
            break;
        }
        // Only a literal `./` prefix: `.github/` and dotfiles keep their dot.
        let mut token = token;
        while let Some(rest) = token.strip_prefix("./") {
            token = rest;
        }
        let token = token.trim_end_matches('.');
        let dir = token.len() > 1 && token.ends_with('/');
        let token = token.trim_end_matches('/');
        let (first, last) = (
            token.split('/').next().unwrap_or_default(),
            token.rsplit('/').next().unwrap_or_default(),
        );
        let file = last.rsplit_once('.').is_some_and(|(stem, ext)| {
            !stem.is_empty()
                && (1..=8).contains(&ext.len())
                && ext.bytes().all(|c| c.is_ascii_alphanumeric())
                && ext.bytes().any(|c| c.is_ascii_alphabetic())
        });
        // `e.g`, `i.e` and `a.k.a` pass the extension rule, but a bare token of
        // dotted single letters is an abbreviation, not a file: two issues that
        // both say "e.g." share no file, and one run waiting on the other for it
        // lasts until that run ends (#204). `a.rs` (a longer extension) and
        // `lib/e.g` (a directory) are still paths; a file like `a.c` is named
        // with its directory.
        let abbreviation = !token.contains('/') && token.split('.').all(|part| part.len() == 1);
        let dotfile = last.strip_prefix('.').is_some_and(|name| {
            !name.contains('.') && name.bytes().any(|c| c.is_ascii_alphabetic())
        });
        let host = token.contains('/') && first.trim_start_matches('.').contains('.');
        if ((file && !abbreviation) || dotfile || dir)
            && !host
            && token.len() <= 200
            && !token.starts_with(['/', '-'])
            && !token.contains("..")
            && !token.contains("//")
        {
            found.insert(token.to_owned());
        }
    }
    found
}

fn pathlike(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_./-".contains(c)
}

/// Whether two named paths may be the same file: equal, one a directory of
/// the other, or one a bare file name the other ends in.
pub fn overlaps(a: &str, b: &str) -> bool {
    let within = |dir: &str, path: &str| path.strip_prefix(dir).is_some_and(|r| r.starts_with('/'));
    let named =
        |name: &str, path: &str| !name.contains('/') && path.rsplit('/').next() == Some(name);
    a == b || within(a, b) || within(b, a) || named(a, b) || named(b, a)
}

fn shared(named: &BTreeSet<String>, other: &BTreeSet<String>) -> Vec<String> {
    other
        .iter()
        .filter(|o| named.iter().any(|n| overlaps(n, o)))
        .cloned()
        .collect()
}

/// Decide whether the run that `source`'s plan (`brief`) would start for
/// `dest`, the issue the plan names, waits for the `bot`'s open pull requests
/// on `dest`'s repository or the implement runs in flight on `target` for it.
/// The task text is read from `source`, which may be a tracker issue.
pub fn decide<C: Transport + Rest>(
    client: &C,
    source: &ItemRef,
    dest: &ItemRef,
    brief: &str,
    bot: &str,
    target: &Target,
) -> Decision {
    let waiver = Waiver::default();
    match check(
        client,
        (source, dest),
        brief,
        bot,
        target,
        (&BTreeSet::new(), &waiver),
    ) {
        Ok(decision) => decision,
        // Without the check every run is independent; failing to read leaves that.
        Err(error) => {
            Decision::Independent(format!("the overlap check could not read ({error:#})"))
        }
    }
}

/// [`decide`] for a scheduler that starts runs on its own (`agent-board
/// pass`): a check that cannot read waits rather than runs, as nobody chose
/// this issue by hand to run beside whatever it may overlap. The runs by id
/// in `ended` are not in flight, whatever GitHub says (agent-board#376), and
/// the pull requests `waiver` waives do not hold it.
pub fn decide_strict<C: Transport + Rest>(
    client: &C,
    (source, dest): (&ItemRef, &ItemRef),
    brief: &str,
    bot: &str,
    target: &Target,
    (ended, waiver): (&BTreeSet<u64>, &Waiver),
) -> Decision {
    check(client, (source, dest), brief, bot, target, (ended, waiver)).unwrap_or_else(|error| {
        Decision::Wait(
            format!("the overlap check could not read ({error:#})"),
            Vec::new(),
        )
    })
}

fn check<C: Transport + Rest>(
    client: &C,
    (source, dest): (&ItemRef, &ItemRef),
    brief: &str,
    bot: &str,
    target: &Target,
    (ended, waiver): (&BTreeSet<u64>, &Waiver),
) -> Result<Decision> {
    let mut named = paths(brief);
    named.extend(paths(&text(client, source)?));
    if named.is_empty() {
        return Ok(Decision::Independent("the issue names no path".into()));
    }
    // A run in flight has not pushed, and a batch cannot join a run that
    // started, so an overlap waits for its pull request.
    for running in in_flight(client, dest, target, ended)?
        .iter()
        .take(RUN_READS)
    {
        // Text against text, the same bare `.name` in both is as often an
        // extension as a file: agentic-job#636 waited on #882 only because
        // both said `.patch`. Against a path with a directory, or a pull
        // request's files, it still overlaps.
        let bare = |path: &str| path.starts_with('.') && !path.contains('/');
        let theirs: Vec<String> = paths(&text(client, running)?)
            .into_iter()
            .filter(|o| named.iter().any(|n| overlaps(n, o) && !(n == o && bare(n))))
            .collect();
        if !theirs.is_empty() {
            return Ok(Decision::Wait(
                format!(
                    "the run for {} in flight names {}",
                    running.display(),
                    shown(&theirs)
                ),
                Vec::new(),
            ));
        }
    }
    let (mut overlapping, mut ignored) = (Vec::new(), Vec::new());
    let mut reads = DRAFT_READS;
    for (number, files) in open_pulls(client, dest, bot)? {
        let theirs = shared(&named, &files);
        if !theirs.is_empty() {
            let pull = ItemRef {
                number,
                ..dest.clone()
            };
            let changes = format!("{} changes {}", pull.display(), shown(&theirs));
            match waiver.waives(client, (dest, number), &theirs, &mut reads)? {
                Some(why) => ignored.push(format!("{changes}, but {why}")),
                None => overlapping.push((number, changes)),
            }
        }
    }
    Ok(if !overlapping.is_empty() {
        let (pulls, changes): (Vec<u64>, Vec<String>) = overlapping.into_iter().unzip();
        Decision::Wait(format!("open pull request {}", changes.join("; ")), pulls)
    } else if !ignored.is_empty() {
        Decision::Ignored(format!("open pull request {}", ignored.join("; ")))
    } else {
        Decision::Independent(format!(
            "no open pull request of {bot} changes a path it names"
        ))
    })
}

pub(crate) fn owner_name(issue: &ItemRef) -> Result<(&str, &str)> {
    validate_repository(&issue.repository)?;
    issue.repository.split_once('/').context("issue repository")
}

fn text<T: Transport>(client: &T, issue: &ItemRef) -> Result<String> {
    let (owner, name) = owner_name(issue)?;
    let data = client.graphql(
        TEXT_QUERY,
        json!({"owner": owner, "name": name, "number": issue.number}),
    )?;
    let node = data
        .pointer("/repository/issue")
        .filter(|n| n.is_object())
        .with_context(|| format!("{} is not an issue", issue.display()))?;
    Ok(format!(
        "{}\n{}",
        node["title"].as_str().unwrap_or_default(),
        node["body"].as_str().unwrap_or_default()
    ))
}

/// The bot's open pull requests on `dest`'s repository and the files each changes.
fn open_pulls<T: Transport>(
    client: &T,
    dest: &ItemRef,
    bot: &str,
) -> Result<Vec<(u64, BTreeSet<String>)>> {
    let (owner, name) = owner_name(dest)?;
    let data = client.graphql(
        PULLS_QUERY,
        json!({"owner": owner, "name": name, "pulls": PULL_READS, "files": FILE_READS}),
    )?;
    let nodes = data
        .pointer("/repository/pullRequests/nodes")
        .and_then(Value::as_array)
        .context("pull requests have no nodes")?;
    let mut out = Vec::new();
    for pull in nodes {
        let author = pull.pointer("/author/login").and_then(Value::as_str);
        if !author.is_some_and(|a| a.eq_ignore_ascii_case(bot)) {
            continue;
        }
        let number = pull["number"]
            .as_u64()
            .context("pull request has no number")?;
        let files = pull
            .pointer("/files/nodes")
            .and_then(Value::as_array)
            .context("pull request has no files")?
            .iter()
            // A path is quoted into the next run's task and into the record, and
            // git allows backticks and newlines in one: keep only plain paths.
            .filter_map(|f| f["path"].as_str())
            .filter(|p| p.chars().all(pathlike))
            .map(str::to_owned)
            .collect();
        out.push((number, files));
    }
    Ok(out)
}

/// The other issues of `dest`'s repository with an implement run of the
/// caller's workflow queued or in progress. The caller names each run
/// `KIND OWNER/REPO#N` with an optional ` (POOL)` (its `run-name`), from the
/// plan's `repo` and `item`. The runs in `ended` are left out.
fn in_flight<C: Rest>(
    client: &C,
    dest: &ItemRef,
    target: &Target,
    ended: &BTreeSet<u64>,
) -> Result<Vec<ItemRef>> {
    target.validate()?;
    let mut out = Vec::new();
    for status in ["in_progress", "queued"] {
        let url = format!(
            "{API_ROOT}repos/{}/actions/workflows/{}/runs?status={status}&per_page=100",
            target.caller, target.workflow
        );
        let response = client.get(&url, None)?;
        ensure!(
            response.status == 200,
            "listing runs: GitHub HTTP status {}",
            response.status
        );
        let runs = response.body["workflow_runs"]
            .as_array()
            .context("run list has no workflow_runs")?;
        for run in runs {
            if run["id"].as_u64().is_some_and(|id| ended.contains(&id)) {
                continue;
            }
            let title = run["display_title"].as_str().unwrap_or_default();
            let Some(named) = title.strip_prefix("implement ") else {
                continue;
            };
            let named = named.split(' ').next().unwrap_or_default();
            let Some((repo, number)) = named.rsplit_once('#') else {
                continue;
            };
            let Ok(number) = number.parse::<u64>() else {
                continue;
            };
            if repo.eq_ignore_ascii_case(&dest.repository)
                && number != dest.number
                && !out.iter().any(|o: &ItemRef| o.number == number)
            {
                out.push(ItemRef {
                    number,
                    ..dest.clone()
                });
            }
        }
    }
    out.sort_by_key(|o| o.number);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::poll::RestResponse;

    #[test]
    fn paths_table() {
        let text = "Change `crates/board-forge/src/batch.rs` and docs/job-plans.md (see \
            https://github.com/cgwalters-forge/agent-board/blob/main/README.md), and the \
            safe-outputs/ code in ./src/lib.rs, .github/workflows/ci.yml, ././.gitignore and \
            .config/ too. Not cgwalters-forge/agent-board#86, \
            github.com/x/y.md, e.g. 1.5, i.e. the backoff (a.k.a. jitter), and/or \
            ../etc/passwd or -rf.sh. Keep a.rs and lib/e.g, not a.c. A bare host \
            name, github.com, still passes.";
        let found: Vec<String> = paths(text).into_iter().collect();
        assert_eq!(
            found,
            [
                ".config",
                ".github/workflows/ci.yml",
                ".gitignore",
                "a.rs",
                "crates/board-forge/src/batch.rs",
                "docs/job-plans.md",
                "github.com",
                "lib/e.g",
                "safe-outputs",
                "src/lib.rs",
            ]
        );
        assert!(paths("Fix the flaky test.").is_empty());
        assert!(paths("E.g. i.e. a.k.a. P.S. U.S.").is_empty());
        let many: String = (0..2 * PATH_LIMIT).map(|n| format!("f{n}.rs ")).collect();
        assert_eq!(paths(&many).len(), PATH_LIMIT);
    }

    #[test]
    fn overlaps_table() {
        for (a, b, expected) in [
            ("src/batch.rs", "src/batch.rs", true),
            ("batch.rs", "crates/board-forge/src/batch.rs", true),
            (
                "crates/board-forge",
                "crates/board-forge/src/batch.rs",
                true,
            ),
            (
                "crates/board-forge/src/batch.rs",
                "crates/board-forge",
                true,
            ),
            ("src/batch.rs", "src/batch.rs.orig", false),
            ("crates/board", "crates/board-forge/src/batch.rs", false),
            ("src/plan.rs", "crates/board-forge/src/plan.rs", false),
            ("plan.rs", "batch.rs", false),
            (".github", ".github/workflows/ci.yml", true),
            (".gitignore", "sub/.gitignore", true),
            ("github/workflows/ci.yml", ".github/workflows/ci.yml", false),
        ] {
            assert_eq!(overlaps(a, b), expected, "{a} {b}");
        }
    }

    /// The caller's run list, the bot's and others' pull requests on
    /// example/work and on example/tracker, and issue texts by OWNER/REPO#N.
    struct Forge {
        runs: Vec<&'static str>,
        pulls: Value,
        tracker_pulls: Value,
        texts: std::collections::BTreeMap<&'static str, &'static str>,
        fail: bool,
    }

    impl Transport for Forge {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            ensure!(!self.fail, "GitHub transport failed");
            let repo = format!("{}/{}", variables["owner"], variables["name"]).replace('"', "");
            if query == PULLS_QUERY {
                assert_eq!(variables["files"], FILE_READS);
                let nodes = match repo.as_str() {
                    "example/work" => &self.pulls,
                    "example/tracker" => &self.tracker_pulls,
                    other => panic!("pull requests of {other}"),
                };
                return Ok(json!({"repository": {"pullRequests": {"nodes": nodes}}}));
            }
            if query == DRAFT_QUERY {
                assert_eq!(repo, "example/work");
                let node = (self.pulls.as_array().into_iter().flatten())
                    .find(|p| p["number"] == variables["number"]);
                return Ok(json!({"repository": {"pullRequest": node}}));
            }
            assert_eq!(query, TEXT_QUERY);
            let key = format!("{repo}#{}", variables["number"]);
            let body = self.texts.get(key.as_str()).copied();
            Ok(json!({"repository": {"issue": {"title": "T", "body": body.unwrap_or_default()}}}))
        }
    }

    impl Rest for Forge {
        fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
            if url.contains("/activity?") {
                let parsed = url::Url::parse(url)?;
                assert_eq!(parsed.path(), "/repos/example/fork/activity");
                let reference = parsed.query_pairs().find(|(k, _)| k == "ref").unwrap().1;
                let number: u64 = reference
                    .strip_prefix("refs/heads/draft-")
                    .unwrap()
                    .parse()?;
                let pull = self
                    .pulls
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|p| p["number"] == number)
                    .unwrap();
                ensure!(pull["activityStatus"] != 0, "activity unreadable");
                return Ok(RestResponse {
                    status: pull["activityStatus"].as_u64().unwrap_or(200) as u16,
                    etag: None,
                    last_modified: None,
                    link: pull["activityLink"].as_str().map(str::to_owned),
                    body: pull["activity"].clone(),
                });
            }
            let runs: Vec<Value> = if url.contains("status=in_progress") {
                (1..)
                    .zip(&self.runs)
                    .map(|(id, t)| json!({"id": id, "display_title": t}))
                    .collect()
            } else {
                assert!(url.contains("status=queued"), "{url}");
                vec![]
            };
            assert!(url.starts_with(
                "https://api.github.com/repos/cgwalters-forge/tracker/actions/workflows/dispatch.yml/runs?"
            ));
            Ok(RestResponse {
                status: 200,
                etag: None,
                last_modified: None,
                link: None,
                body: json!({"workflow_runs": runs}),
            })
        }

        fn send(&self, method: &str, url: &str, _: &Value) -> Result<(u16, Value)> {
            panic!("unexpected {method} {url}")
        }
    }

    #[test]
    fn decision_table() {
        let target = Target {
            caller: "cgwalters-forge/tracker".into(),
            workflow: "dispatch.yml".into(),
            git_ref: "main".into(),
        };
        let issue = ItemRef {
            host: "github.com".into(),
            repository: "example/work".into(),
            number: 10,
        };
        let pull = |n: u64, author: &str, files: &[&str]| {
            json!({"number": n, "author": {"login": author},
                "files": {"nodes": files.iter().map(|p| json!({"path": p})).collect::<Vec<_>>()}})
        };
        let bot = "agentic-bot";
        // Name, the issue's text, the runs in flight, the open pull requests; the decision.
        type Case<'a> = (&'a str, &'static str, Vec<&'static str>, Value, Decision);
        let independent = |why: &str| Decision::Independent(why.into());
        let wait = |why: &str, pulls: &[u64]| Decision::Wait(why.into(), pulls.to_vec());
        let none = "no open pull request of agentic-bot changes a path it names";
        let cases: Vec<Case> = vec![
            (
                "no paths",
                "Fix the flaky test.",
                vec![],
                json!([pull(5, bot, &["src/a.rs"])]),
                independent("the issue names no path"),
            ),
            (
                "nothing open",
                "Fix src/a.rs.",
                vec![],
                json!([]),
                independent(none),
            ),
            // One overlapping pull request is waited for, not stacked on: the
            // caller has no base input and the poll would not hold a dependent.
            (
                "one pull request",
                "Fix `a.rs`.",
                vec![],
                json!([
                    pull(5, bot, &["src/a.rs", "src/b.rs"]),
                    pull(6, bot, &["docs/x.md"])
                ]),
                wait("open pull request example/work#5 changes `src/a.rs`", &[5]),
            ),
            // A path git allows but the record must not quote is skipped.
            (
                "crafted path",
                "Fix a.rs.",
                vec![],
                json!([pull(5, bot, &["src/`x`\n/a.rs", "x/a.rs"])]),
                wait("open pull request example/work#5 changes `x/a.rs`", &[5]),
            ),
            (
                "Bot login case",
                "Fix src/a.rs.",
                vec![],
                json!([pull(5, "Agentic-Bot", &["src/a.rs"])]),
                wait("open pull request example/work#5 changes `src/a.rs`", &[5]),
            ),
            // Only the bot's pull requests are waited for.
            (
                "someone else's",
                "Fix src/a.rs.",
                vec![],
                json!([pull(5, "visitor", &["src/a.rs"])]),
                independent(none),
            ),
            (
                "two pull requests",
                "Fix src/a.rs and docs/x.md.",
                vec![],
                json!([pull(5, bot, &["src/a.rs"]), pull(6, bot, &["docs/x.md"])]),
                wait(
                    "open pull request example/work#5 changes `src/a.rs`; \
                     example/work#6 changes `docs/x.md`",
                    &[5, 6],
                ),
            ),
            // A hidden directory keeps its dot, and so overlaps.
            (
                "workflow path",
                "Fix ./.github/workflows/ci.yml.",
                vec![],
                json!([pull(5, bot, &[".github/workflows/ci.yml"])]),
                wait(
                    "open pull request example/work#5 changes `.github/workflows/ci.yml`",
                    &[5],
                ),
            ),
            // A run in flight on the same repository waits for its pull request.
            (
                "run in flight",
                "Fix src/a.rs.",
                vec![
                    "implement example/work#11 (claude)",
                    "implement example/other#12",
                    "fix example/work#13",
                    "implement example/work#10",
                ],
                json!([pull(5, bot, &["src/a.rs"])]),
                wait("the run for example/work#11 in flight names `a.rs`", &[]),
            ),
            (
                "run elsewhere",
                "Fix src/a.rs.",
                vec!["implement example/work#14"],
                json!([]),
                independent(none),
            ),
            // Two briefs that both say `.patch` name an extension, not the
            // same file (agentic-job#636 and #882).
            (
                "shared extension",
                "Fix src/a.rs and the `.patch` it writes.",
                vec!["implement example/work#15"],
                json!([]),
                independent(none),
            ),
            // Two briefs that both say "e.g." name no file (#204).
            (
                "shared abbreviation",
                "Make the retry bounded, e.g. five attempts, in src/retry.rs.",
                vec!["implement example/work#17"],
                json!([]),
                independent(none),
            ),
            // A bare dotfile still overlaps a path with a directory...
            (
                "dotfile in flight",
                "Add target/ to .gitignore.",
                vec!["implement example/work#16"],
                json!([]),
                wait(
                    "the run for example/work#16 in flight names `sub/.gitignore`",
                    &[],
                ),
            ),
            // ...and the file a pull request changes.
            (
                "dotfile pull request",
                "Add target/ to .gitignore.",
                vec![],
                json!([pull(5, bot, &[".gitignore"])]),
                wait(
                    "open pull request example/work#5 changes `.gitignore`",
                    &[5],
                ),
            ),
        ];
        let texts = |text| {
            [
                ("example/work#10", text),
                ("example/work#11", "Touches a.rs."),
                ("example/work#14", "Touches docs/y.md."),
                ("example/work#15", "Hand back a `.patch` (*.patch)."),
                ("example/work#16", "Touches sub/.gitignore."),
                (
                    "example/work#17",
                    "Document the pools, e.g. claude, in docs/pools.md.",
                ),
                ("example/tracker#11", "Touches a.rs."),
            ]
            .into()
        };
        for (name, text, runs, pulls, expected) in cases {
            let forge = Forge {
                runs,
                pulls,
                // The overlapping pull request of another repository is not waited for.
                tracker_pulls: json!([pull(5, bot, &["src/a.rs"])]),
                texts: texts(text),
                fail: false,
            };
            assert_eq!(
                decide(&forge, &issue, &issue, "Do it.", bot, &target),
                expected,
                "{name}"
            );
        }
        // A tracker issue whose plan changes example/work#20: its text is read
        // from the tracker, and the pull requests and runs are example/work's.
        let tracker = ItemRef {
            repository: "example/tracker".into(),
            ..issue.clone()
        };
        let dest = ItemRef {
            number: 20,
            ..issue.clone()
        };
        for (name, runs, pulls, expected) in [
            (
                "tracker's own pull request and run",
                vec!["implement example/tracker#11", "implement example/work#20"],
                json!([pull(7, bot, &["docs/x.md"])]),
                independent(none),
            ),
            (
                "target's pull request",
                vec![],
                json!([pull(7, bot, &["src/a.rs"])]),
                wait("open pull request example/work#7 changes `src/a.rs`", &[7]),
            ),
            (
                "target's run",
                vec!["implement example/work#11"],
                json!([]),
                wait("the run for example/work#11 in flight names `a.rs`", &[]),
            ),
        ] {
            let forge = Forge {
                runs,
                pulls,
                tracker_pulls: json!([pull(5, bot, &["src/a.rs"])]),
                texts: [
                    ("example/tracker#10", "Fix src/a.rs."),
                    ("example/work#11", "Touches a.rs."),
                    ("example/tracker#11", "Touches a.rs."),
                ]
                .into(),
                fail: false,
            };
            assert_eq!(
                decide(&forge, &tracker, &dest, "Do it.", bot, &target),
                expected,
                "{name}"
            );
        }
        let forge = Forge {
            runs: vec![],
            pulls: json!([]),
            tracker_pulls: json!([]),
            texts: Default::default(),
            fail: true,
        };
        let Decision::Independent(why) =
            decide(&forge, &issue, &issue, "Fix src/a.rs.", bot, &target)
        else {
            panic!("an unreadable check runs independently");
        };
        assert!(
            why.contains("could not read (GitHub transport failed)"),
            "{why}"
        );
        // A run the pass took as ended (agent-board#376) is not waited for.
        let forge = Forge {
            runs: vec!["implement example/work#11"],
            pulls: json!([]),
            tracker_pulls: json!([]),
            texts: texts("Fix src/a.rs."),
            fail: false,
        };
        let strict = |ended: &BTreeSet<u64>| {
            let waiver = Waiver::default();
            decide_strict(
                &forge,
                (&issue, &issue),
                "Do it.",
                bot,
                &target,
                (ended, &waiver),
            )
        };
        let running = wait("the run for example/work#11 in flight names `a.rs`", &[]);
        assert_eq!(strict(&BTreeSet::new()), running);
        assert_eq!(strict(&BTreeSet::from([1])), independent(none));
        let many = format!(
            "open pull request example/work#5 changes {}",
            shown(&["a/1.rs", "a/2.rs", "a/3.rs", "a/4.rs"].map(String::from))
        );
        assert_eq!(
            wait(&many, &[]).sentence(),
            "Waiting: open pull request example/work#5 changes `a/1.rs`, `a/2.rs`, `a/3.rs`, 1 more."
        );
    }

    /// The pass's `overlap` setting (agent-board#426): a pull request it
    /// waives does not hold the run, and the decision says which and why;
    /// every other overlap holds as without it, the default.
    #[test]
    fn waiver_table() {
        let target = Target {
            caller: "cgwalters-forge/tracker".into(),
            workflow: "dispatch.yml".into(),
            git_ref: "main".into(),
        };
        let issue = ItemRef {
            host: "github.com".into(),
            repository: "example/work".into(),
            number: 10,
        };
        let now = OffsetDateTime::parse("2026-10-28T18:00:00Z", &Rfc3339).unwrap();
        // Commit dates are deliberately old even when the push is fresh.
        // Activity belongs to the fork's branch, not the base repository.
        let pull = |n: u64, files: &[&str], draft: bool, (opened, pushed): (i64, i64)| {
            let at = |h: i64| (now - time::Duration::hours(h)).format(&Rfc3339).unwrap();
            json!({"number": n, "author": {"login": "agentic-bot"}, "isDraft": draft,
                "createdAt": at(opened),
                "headRefOid": "a".repeat(40), "headRefName": format!("draft-{n}"),
                "headRepository": {"nameWithOwner": "example/fork"},
                "activityLink": null, "activityStatus": 200,
                "commits": {"nodes": [{"commit": {"committedDate": at(100)}}]},
                "activity": [
                    {"ref": format!("refs/heads/draft-{n}"), "activity_type": "push",
                     "timestamp": at(pushed), "before": "b".repeat(40), "after": "a".repeat(40)},
                    {"ref": format!("refs/heads/draft-{n}"), "activity_type": "branch_creation",
                     "timestamp": at(100), "before": "0".repeat(40), "after": "b".repeat(40)}],
                "files": {"nodes": files.iter().map(|p| json!({"path": p})).collect::<Vec<_>>()}})
        };
        let mut unknown = pull(5, &["src/a.rs"], true, (9, 9));
        unknown["activity"] = Value::Null;
        let off = Waiver::default();
        let stale = Waiver {
            stale_draft: Some((now, 4)),
            top: false,
        };
        let top = Waiver {
            stale_draft: None,
            top: true,
        };
        let both = Waiver {
            stale_draft: Some((now, 4)),
            top: true,
        };
        let wait = |why: &str, pulls: &[u64]| {
            Decision::Wait(format!("open pull request {why}"), pulls.to_vec())
        };
        let ignored = |why: &str| Decision::Ignored(format!("open pull request {why}"));
        let code = "example/work#5 changes `src/a.rs`";
        let docs = "example/work#5 changes `README.md`, `docs/a.md`";
        let only_docs = "it shares only documentation, and the issue is in the first Priority";
        let (fix_code, fix_docs) = ("Fix src/a.rs.", "Fix docs/a.md and README.md.");
        // Name, the issue's text, the open pull requests, the waiver; the decision.
        let mut cases = vec![
            (
                "off, a stale draft holds",
                fix_code,
                json!([pull(5, &["src/a.rs"], true, (9, 5))]),
                off,
                wait(code, &[5]),
            ),
            (
                "stale draft",
                fix_code,
                json!([pull(5, &["src/a.rs"], true, (9, 5))]),
                stale,
                ignored(&format!(
                    "{code}, but it is a draft whose head has not changed for 5 hours"
                )),
            ),
            (
                "old commit freshly pushed to an old draft",
                fix_code,
                json!([pull(5, &["src/a.rs"], true, (9, 3))]),
                both,
                wait(code, &[5]),
            ),
            (
                "draft opened since",
                fix_code,
                json!([pull(5, &["src/a.rs"], true, (2, 9))]),
                stale,
                wait(code, &[5]),
            ),
            (
                "draft of unknown age",
                fix_code,
                json!([unknown]),
                stale,
                wait(code, &[5]),
            ),
            (
                "not a draft",
                fix_code,
                json!([pull(5, &["src/a.rs"], false, (9, 9))]),
                stale,
                wait(code, &[5]),
            ),
            (
                "off, documentation holds",
                fix_docs,
                json!([pull(5, &["docs/a.md", "README.md"], false, (1, 1))]),
                off,
                wait(docs, &[5]),
            ),
            (
                "documentation, first Priority",
                fix_docs,
                json!([pull(5, &["docs/a.md", "README.md"], false, (1, 1))]),
                top,
                ignored(&format!("{docs}, but {only_docs}")),
            ),
            (
                "documentation, another Priority",
                fix_docs,
                json!([pull(5, &["docs/a.md", "README.md"], false, (1, 1))]),
                stale,
                wait(docs, &[5]),
            ),
            (
                "documentation and code",
                "Fix docs/a.md and src/a.rs.",
                json!([pull(5, &["docs/a.md", "src/a.rs"], false, (1, 1))]),
                top,
                wait("example/work#5 changes `docs/a.md`, `src/a.rs`", &[5]),
            ),
            (
                "one waived, one holds",
                "Fix docs/a.md and src/b.rs.",
                json!([
                    pull(5, &["docs/a.md"], false, (1, 1)),
                    pull(6, &["src/b.rs"], false, (1, 1))
                ]),
                both,
                wait("example/work#6 changes `src/b.rs`", &[6]),
            ),
            (
                "more stale drafts than the request allowance",
                fix_code,
                (5..=5 + (DRAFT_READS / 2) as u64)
                    .map(|n| pull(n, &["src/a.rs"], true, (9, 9)))
                    .collect(),
                stale,
                wait(
                    &format!("example/work#{} changes `src/a.rs`", 5 + DRAFT_READS / 2),
                    &[5 + (DRAFT_READS / 2) as u64],
                ),
            ),
        ];
        for (name, field, value) in [
            ("missing head", "/headRefOid", Value::Null),
            ("missing fork", "/headRepository", Value::Null),
            ("missing activity", "/activity", Value::Null),
            ("empty activity", "/activity", json!([])),
            ("truncated activity", "/activityLink", json!("next page")),
            ("unreadable activity", "/activityStatus", json!(403)),
            ("head mismatch", "/activity/0/after", json!("c".repeat(40))),
            ("ref mismatch", "/activity/0/ref", json!("refs/heads/other")),
            ("unknown timestamp", "/activity/0/timestamp", Value::Null),
            (
                "unknown activity",
                "/activity/0/activity_type",
                json!("unknown"),
            ),
            ("broken chain", "/activity/0/before", json!("c".repeat(40))),
            (
                "missing beginning",
                "/activity/1/before",
                json!("c".repeat(40)),
            ),
            (
                "future push",
                "/activity/0/timestamp",
                json!("2026-10-29T18:00:00Z"),
            ),
            (
                "unordered history",
                "/activity/1/timestamp",
                json!("2026-10-28T17:00:00Z"),
            ),
        ] {
            let mut draft = pull(5, &["src/a.rs"], true, (9, 5));
            *draft
                .pointer_mut(field)
                .unwrap_or_else(|| panic!("{field}")) = value;
            cases.push((name, fix_code, json!([draft]), stale, wait(code, &[5])));
        }
        for (name, pushed, ignored_head) in [
            ("fresh force push of an old commit", 1, false),
            ("idle force-pushed head", 5, true),
        ] {
            let mut draft = pull(5, &["src/a.rs"], true, (9, pushed));
            draft["activity"][0]["activity_type"] = json!("force_push");
            cases.push((
                name,
                fix_code,
                json!([draft]),
                stale,
                if ignored_head {
                    ignored(&format!(
                        "{code}, but it is a draft whose head has not changed for 5 hours"
                    ))
                } else {
                    wait(code, &[5])
                },
            ));
        }
        let mut full = pull(5, &["src/a.rs"], true, (9, 5));
        full["activity"] = vec![full["activity"][0].clone(); FILE_READS as usize].into();
        cases.push((
            "activity read cap exhausted",
            fix_code,
            json!([full]),
            stale,
            wait(code, &[5]),
        ));
        for (name, text, pulls, waiver, expected) in cases {
            let forge = Forge {
                runs: vec![],
                pulls,
                tracker_pulls: json!([]),
                texts: [("example/work#10", text)].into(),
                fail: false,
            };
            let checks = (&BTreeSet::new(), &waiver);
            let decision = decide_strict(
                &forge,
                (&issue, &issue),
                "-",
                "agentic-bot",
                &target,
                checks,
            );
            assert_eq!(decision, expected, "{name}");
            // Neither waiver may release an overlap with a run still in flight.
            let forge = Forge {
                runs: vec!["implement example/work#11"],
                texts: [("example/work#10", text), ("example/work#11", text)].into(),
                ..forge
            };
            assert!(
                matches!(decide_strict(&forge, (&issue, &issue), "-", "agentic-bot", &target, checks),
                Decision::Wait(why, pulls) if why.contains("in flight") && pulls.is_empty()),
                "{name}"
            );
        }
        assert_eq!(
            ignored(&format!("{docs}, but {only_docs}")).sentence(),
            format!(
                "Run independently: overlap ignored: open pull request {docs}, but {only_docs}."
            )
        );
    }
}
