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
use crate::github::{API_ROOT, Transport};
use crate::plan::Target;
use crate::poll::{Rest, validate_repository};
use anyhow::{Context, Result, ensure};
use board_core::ItemRef;
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// Paths read from one issue's text, at most, so that a pasted listing
/// costs a bounded comparison.
pub const PATH_LIMIT: usize = 50;
/// Open pull requests read, newest first, and files read of each. An older
/// one, or a file past the limit, is not compared.
pub const PULL_READS: u64 = 50;
pub const FILE_READS: u64 = 100;
/// Running issues whose text is read, at most.
pub const RUN_READS: usize = 10;
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

/// What the check decided for a dispatch.
#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    /// Nothing overlaps, or the check was off or could not read; the note
    /// says which.
    Independent(String),
    /// Dispatch nothing now: the next pass decides again.
    Wait(String),
}

impl Decision {
    /// The sentence the dispatch record carries.
    pub fn sentence(&self) -> String {
        match self {
            Self::Independent(why) => format!("Run independently: {why}."),
            Self::Wait(why) => format!("Waiting: {why}."),
        }
    }
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
    match check(client, source, dest, brief, bot, target, &BTreeSet::new()) {
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
/// in `ended` are not in flight, whatever GitHub says (agent-board#376).
pub fn decide_strict<C: Transport + Rest>(
    client: &C,
    source: &ItemRef,
    dest: &ItemRef,
    brief: &str,
    bot: &str,
    target: &Target,
    ended: &BTreeSet<u64>,
) -> Decision {
    check(client, source, dest, brief, bot, target, ended).unwrap_or_else(|error| {
        Decision::Wait(format!("the overlap check could not read ({error:#})"))
    })
}

fn check<C: Transport + Rest>(
    client: &C,
    source: &ItemRef,
    dest: &ItemRef,
    brief: &str,
    bot: &str,
    target: &Target,
    ended: &BTreeSet<u64>,
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
            return Ok(Decision::Wait(format!(
                "the run for {} in flight names {}",
                running.display(),
                shown(&theirs)
            )));
        }
    }
    let mut overlapping = Vec::new();
    for (number, files) in open_pulls(client, dest, bot)? {
        let theirs = shared(&named, &files);
        if !theirs.is_empty() {
            let pull = ItemRef {
                number,
                ..dest.clone()
            };
            overlapping.push(format!("{} changes {}", pull.display(), shown(&theirs)));
        }
    }
    Ok(if overlapping.is_empty() {
        Decision::Independent(format!(
            "no open pull request of {bot} changes a path it names"
        ))
    } else {
        Decision::Wait(format!("open pull request {}", overlapping.join("; ")))
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
            assert_eq!(query, TEXT_QUERY);
            let key = format!("{repo}#{}", variables["number"]);
            let body = self.texts.get(key.as_str()).copied();
            Ok(json!({"repository": {"issue": {"title": "T", "body": body.unwrap_or_default()}}}))
        }
    }

    impl Rest for Forge {
        fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
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
        let wait = |why: &str| Decision::Wait(why.into());
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
                wait("open pull request example/work#5 changes `src/a.rs`"),
            ),
            // A path git allows but the record must not quote is skipped.
            (
                "crafted path",
                "Fix a.rs.",
                vec![],
                json!([pull(5, bot, &["src/`x`\n/a.rs", "x/a.rs"])]),
                wait("open pull request example/work#5 changes `x/a.rs`"),
            ),
            (
                "Bot login case",
                "Fix src/a.rs.",
                vec![],
                json!([pull(5, "Agentic-Bot", &["src/a.rs"])]),
                wait("open pull request example/work#5 changes `src/a.rs`"),
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
                ),
            ),
            // A hidden directory keeps its dot, and so overlaps.
            (
                "workflow path",
                "Fix ./.github/workflows/ci.yml.",
                vec![],
                json!([pull(5, bot, &[".github/workflows/ci.yml"])]),
                wait("open pull request example/work#5 changes `.github/workflows/ci.yml`"),
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
                wait("the run for example/work#11 in flight names `a.rs`"),
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
                wait("the run for example/work#16 in flight names `sub/.gitignore`"),
            ),
            // ...and the file a pull request changes.
            (
                "dotfile pull request",
                "Add target/ to .gitignore.",
                vec![],
                json!([pull(5, bot, &[".gitignore"])]),
                wait("open pull request example/work#5 changes `.gitignore`"),
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
                wait("open pull request example/work#7 changes `src/a.rs`"),
            ),
            (
                "target's run",
                vec!["implement example/work#11"],
                json!([]),
                wait("the run for example/work#11 in flight names `a.rs`"),
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
            decide_strict(&forge, &issue, &issue, "Do it.", bot, &target, ended)
        };
        let running = wait("the run for example/work#11 in flight names `a.rs`");
        assert_eq!(strict(&BTreeSet::new()), running);
        assert_eq!(strict(&BTreeSet::from([1])), independent(none));
        let many = format!(
            "open pull request example/work#5 changes {}",
            shown(&["a/1.rs", "a/2.rs", "a/3.rs", "a/4.rs"].map(String::from))
        );
        assert_eq!(
            wait(&many).sentence(),
            "Waiting: open pull request example/work#5 changes `a/1.rs`, `a/2.rs`, `a/3.rs`, 1 more."
        );
    }
}
