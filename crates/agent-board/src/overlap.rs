//! `agent-board overlap` (agent-board#297; docs/job-plans.md#the-overlap-command):
//! the open pull requests, of any author, and the implement runs in flight
//! that share a path with a change, as the dispatch overlap check reads
//! them, read-only.
use anyhow::{Context, Result, ensure};
use board_core::ItemRef;
use board_forge::github::{DEFAULT_HOST, Transport};
use board_forge::org_status::{clean, duration};
use board_forge::overlap::{self, DRAFT_READS, PULL_PAGES, PULL_READS};
use board_forge::pass::{STALE_DRAFT_HOURS, STALE_DRAFT_HOURS_LIMIT};
use board_forge::plan::Target;
use board_forge::poll::Rest;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// The report's fields, for `--json`.
pub const FIELDS: &[&str] = &["repository", "paths", "pulls", "runs", "complete"];

/// The reads [`overlap::idle_draft`] takes to prove a draft stale: its
/// facts and its branch activity.
const DRAFT_PROOF_READS: usize = 2;

#[derive(clap::Args)]
#[command(group(clap::ArgGroup::new("change").required(true).args(["paths", "branch", "pr"])))]
pub struct Overlap {
    /// Paths the change touches, as files or directories.
    #[arg(long, num_args = 1.., value_name = "PATH")]
    paths: Vec<String>,
    /// Branch of -R whose changes against its default branch are the change.
    #[arg(long, value_name = "BRANCH")]
    branch: Option<String>,
    /// Pull request of -R whose files (at most 100) are the change; it is not listed itself.
    #[arg(long, value_name = "N")]
    pr: Option<u64>,
    /// Repository whose --workflow runs are the dispatch runs; from the caller's configuration.
    #[arg(long, value_name = "OWNER/REPO")]
    caller: String,
    /// Workflow file in --caller that starts the runs, such as dispatch.yml.
    #[arg(long, value_name = "FILE")]
    workflow: String,
    /// Hours a draft's head must be proved unchanged for it to be flagged stale, as the pass's `overlap.staleDraftHours`.
    #[arg(long, value_name = "HOURS", default_value_t = STALE_DRAFT_HOURS,
        value_parser = clap::value_parser!(u64).range(1..=STALE_DRAFT_HOURS_LIMIT))]
    stale_draft_hours: u64,
}

/// A path given with --paths, as the dispatch check would name it: without
/// a `./` prefix or a trailing `/`, and only of plain path characters.
fn given(path: &str) -> Result<String> {
    let mut plain = path;
    while let Some(rest) = plain.strip_prefix("./") {
        plain = rest;
    }
    let plain = plain.trim_end_matches('/');
    ensure!(
        !plain.is_empty()
            && !plain.starts_with(['/', '-'])
            && !plain.contains("..")
            && !plain.contains("//")
            && plain
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_./-".contains(c)),
        "invalid path {path:?}"
    );
    Ok(plain.to_owned())
}

fn at(time: Option<OffsetDateTime>) -> Value {
    time.and_then(|t| t.format(&Rfc3339).ok())
        .map_or(Value::Null, Value::String)
}

fn since(time: Option<OffsetDateTime>, now: OffsetDateTime) -> Value {
    time.map_or(Value::Null, |t| json!((now - t).whole_seconds().max(0)))
}

/// The report for `repo`. Every input is checked before `client` is asked
/// for a token.
pub fn report<C: Transport + Rest>(
    args: &Overlap,
    repo: Option<&str>,
    client: impl FnOnce() -> Result<C>,
    now: OffsetDateTime,
) -> Result<Value> {
    let repo = repo.context("overlap requires -R OWNER/REPO")?;
    board_forge::poll::validate_repository(repo)?;
    let target = Target {
        caller: args.caller.clone(),
        workflow: args.workflow.clone(),
        git_ref: "main".into(),
    };
    target.validate()?;
    let mut named = args
        .paths
        .iter()
        .map(|p| given(p))
        .collect::<Result<BTreeSet<_>>>()?;
    if let Some(branch) = &args.branch {
        overlap::validate_branch(branch)?;
    }
    let client = client()?;
    if let Some(number) = args.pr {
        named = overlap::pull_paths(&client, repo, number)?;
    } else if let Some(branch) = &args.branch {
        named = overlap::branch_paths(&client, repo, branch)?;
    }
    // The --pr is not its own overlap; nothing else is numbered 0.
    let dest = ItemRef {
        host: DEFAULT_HOST.into(),
        repository: repo.into(),
        number: args.pr.unwrap_or(0),
    };
    let found = if named.is_empty() {
        overlap::Overlap {
            complete: true,
            ..Default::default()
        }
    } else {
        let scope = overlap::Scope::ALL;
        overlap::intersecting(&client, &named, (&dest, &target), &BTreeSet::new(), scope)?
    };
    let mut reads = DRAFT_READS;
    let mut pulls = Vec::new();
    for pull in found.pulls.iter().filter(|p| p.number != dest.number) {
        // The rule by which the pass waives a draft's overlap, within its
        // allowance of reads; past it, a draft's staleness is unknown (null).
        let (stale, unchanged) = if !pull.draft {
            (json!(false), None)
        } else if reads < DRAFT_PROOF_READS {
            (Value::Null, None)
        } else {
            let idle = overlap::idle_draft(
                &client,
                (&dest, pull.number),
                (now, args.stale_draft_hours),
                &mut reads,
            )?;
            (json!(idle.is_some()), idle)
        };
        pulls.push(json!({
            "number": pull.number,
            "url": format!("https://{DEFAULT_HOST}/{repo}/pull/{}", pull.number),
            "author": pull.author,
            "draft": pull.draft,
            "created_at": at(pull.created),
            "updated_at": at(pull.updated),
            "age_seconds": since(pull.created, now),
            "idle_seconds": since(pull.updated, now),
            "stale": stale,
            "unchanged_hours": unchanged,
            "shared": pull.shared,
        }));
    }
    let runs: Vec<Value> = found
        .runs
        .iter()
        .map(|run| {
            json!({
                "item": run.item.display(),
                "id": run.id,
                "url": run.url,
                "created_at": at(run.created),
                "updated_at": at(run.updated),
                "age_seconds": since(run.created, now),
                "idle_seconds": since(run.updated, now),
                "shared": run.shared,
            })
        })
        .collect();
    Ok(json!({
        "repository": repo,
        "paths": named,
        "pulls": pulls,
        "runs": runs,
        "complete": found.complete,
    }))
}

fn ago(seconds: &Value) -> String {
    seconds
        .as_i64()
        .map_or_else(|| "unknown".into(), |s| format!("{} ago", duration(s)))
}

fn quoted(paths: &Value) -> String {
    let paths: Vec<String> = paths
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| format!("`{}`", clean(p)))
        .collect();
    paths.join(", ")
}

/// The report for a terminal, one line a pull request or run.
pub fn text(report: &Value) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    let repo = clean(&report["repository"]);
    let list = |key: &str| report[key].as_array().cloned().unwrap_or_default();
    let (pulls, runs) = (list("pulls"), list("runs"));
    let paths = report["paths"].as_array().map_or(0, Vec::len);
    if paths == 0 {
        line(format!(
            "{repo}: the change names no path that can be compared"
        ));
    } else if pulls.is_empty() && runs.is_empty() {
        line(format!(
            "{repo}: no open pull request or run in flight shares a path with the {paths} given"
        ));
    }
    if !pulls.is_empty() {
        line(format!(
            "{repo}: open pull requests sharing a path with the {paths} given:"
        ));
    }
    for pull in &pulls {
        let mut state = String::new();
        if pull["draft"] == true {
            state.push_str(", draft");
        }
        if let Some(hours) = pull["unchanged_hours"].as_i64() {
            state.push_str(&format!(", stale: head unchanged for {hours}h"));
        } else if pull["stale"].is_null() {
            state.push_str(", staleness not checked");
        }
        line(format!(
            "  #{} by {}{state}; opened {}, last activity {}: {}",
            pull["number"],
            clean(&pull["author"]),
            ago(&pull["age_seconds"]),
            ago(&pull["idle_seconds"]),
            quoted(&pull["shared"])
        ));
    }
    if !runs.is_empty() {
        line(format!(
            "{repo}: implement runs in flight whose issue names a path of them:"
        ));
    }
    for run in &runs {
        line(format!(
            "  {}; started {}, last activity {}: {} {}",
            clean(&run["item"]),
            ago(&run["age_seconds"]),
            ago(&run["idle_seconds"]),
            quoted(&run["shared"]),
            clean(&run["url"])
        ));
    }
    if report["complete"] != true {
        line(format!(
            "  (only the newest {} open pull requests were read)",
            PULL_PAGES as u64 * PULL_READS
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use board_forge::poll::RestResponse;
    use clap::Parser;

    #[derive(clap::Parser)]
    struct Cli {
        #[command(flatten)]
        args: Overlap,
    }

    fn args(extra: &[&str]) -> Overlap {
        let base = [
            "overlap",
            "--caller",
            "example/control",
            "--workflow",
            "dispatch.yml",
        ];
        Cli::try_parse_from(base.iter().chain(extra)).unwrap().args
    }

    /// Pull request 5 of example/work changes `src/a.rs`, a draft whose head
    /// last changed 50 hours ago; pull request 6 changes `docs/x.md`; the
    /// run for example/work#11 in flight names `a.rs`.
    struct Fake;

    const NOW: &str = "2026-10-28T18:00:00Z";

    fn hours_ago(h: i64) -> String {
        (OffsetDateTime::parse(NOW, &Rfc3339).unwrap() - time::Duration::hours(h))
            .format(&Rfc3339)
            .unwrap()
    }

    impl Transport for Fake {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            if query.contains("pullRequests(") {
                let pull = |n: u64, draft: bool, path: &str| {
                    json!({"number": n, "author": {"login": "agentic-bot"}, "isDraft": draft,
                        "createdAt": hours_ago(100), "updatedAt": hours_ago(50),
                        "files": {"nodes": [{"path": path}]}})
                };
                return Ok(json!({"repository": {"pullRequests": {
                    "nodes": [pull(5, true, "src/a.rs"), pull(6, false, "docs/x.md")],
                    "pageInfo": {"hasNextPage": false}}}}));
            }
            if query.contains("headRefOid") {
                return Ok(json!({"repository": {"pullRequest": {"isDraft": true,
                    "createdAt": hours_ago(100), "headRefOid": "a".repeat(40),
                    "headRefName": "draft", "headRepository": {"nameWithOwner": "example/work"}}}}));
            }
            assert_eq!(variables["number"], 11, "{query}");
            Ok(json!({"repository": {"issue": {"title": "T", "body": "Touches a.rs."}}}))
        }
    }

    impl Rest for Fake {
        fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
            let body = if url.contains("/activity?") {
                let event = |kind: &str, h: i64, before: String, after: String| {
                    json!({"ref": "refs/heads/draft", "activity_type": kind,
                        "timestamp": hours_ago(h), "before": before, "after": after})
                };
                json!([
                    event("push", 50, "b".repeat(40), "a".repeat(40)),
                    event("branch_creation", 100, "0".repeat(40), "b".repeat(40))
                ])
            } else if url.contains("/pulls/6/files") {
                json!([{"filename": "src/a.rs"}, {"filename": "docs/x.md"}])
            } else if url.contains("status=in_progress") {
                json!({"workflow_runs": [{"id": 1, "display_title": "implement example/work#11",
                    "created_at": hours_ago(3), "updated_at": hours_ago(1),
                    "html_url": "https://github.com/example/control/actions/runs/1"}]})
            } else {
                assert!(url.contains("status=queued"), "{url}");
                json!({"workflow_runs": []})
            };
            Ok(RestResponse {
                status: 200,
                etag: None,
                last_modified: None,
                link: None,
                body,
            })
        }

        fn send(&self, method: &str, url: &str, _: &Value) -> Result<(u16, Value)> {
            panic!("overlap must not write ({method} {url})")
        }
    }

    #[test]
    fn report_and_text() {
        let now = OffsetDateTime::parse(NOW, &Rfc3339).unwrap();
        for (name, extra, expected) in [
            (
                "paths",
                vec!["--paths", "./src/a.rs", "docs/"],
                "\
example/work: open pull requests sharing a path with the 2 given:
  #5 by agentic-bot, draft, stale: head unchanged for 50h; opened 100h00m ago, last activity 50h00m ago: `src/a.rs`
  #6 by agentic-bot; opened 100h00m ago, last activity 50h00m ago: `docs/x.md`
example/work: implement runs in flight whose issue names a path of them:
  example/work#11; started 3h00m ago, last activity 1h00m ago: `a.rs` https://github.com/example/control/actions/runs/1
",
            ),
            // A pull request is not listed as its own overlap, and a draft
            // is stale only past --stale-draft-hours.
            (
                "pull request",
                vec!["--pr", "6", "--stale-draft-hours", "51"],
                "\
example/work: open pull requests sharing a path with the 2 given:
  #5 by agentic-bot, draft; opened 100h00m ago, last activity 50h00m ago: `src/a.rs`
example/work: implement runs in flight whose issue names a path of them:
  example/work#11; started 3h00m ago, last activity 1h00m ago: `a.rs` https://github.com/example/control/actions/runs/1
",
            ),
            (
                "nothing shared",
                vec!["--paths", "README.md"],
                "example/work: no open pull request or run in flight shares a path with the 1 given\n",
            ),
        ] {
            let report = report(&args(&extra), Some("example/work"), || Ok(Fake), now)
                .unwrap_or_else(|e| panic!("{name}: {e:#}"));
            assert_eq!(text(&report), expected, "{name}");
        }
        let report = report(
            &args(&["--paths", "src/a.rs"]),
            Some("example/work"),
            || Ok(Fake),
            now,
        )
        .unwrap();
        assert_eq!(report["pulls"][0]["unchanged_hours"], 50);
        assert_eq!(report["runs"][0]["age_seconds"], 3 * 3600);
        // Reports text alone shows: a draft past the read allowance, a
        // change with no comparable path, and pull requests left unread.
        let unchecked = json!({"number": 7, "author": "a", "draft": true, "stale": null,
            "unchanged_hours": null, "age_seconds": 60, "idle_seconds": 60, "shared": ["a.rs"]});
        for (report, expected) in [
            (
                json!({"repository": "o/r", "paths": ["a.rs"], "pulls": [unchecked], "runs": [],
                    "complete": true}),
                "o/r: open pull requests sharing a path with the 1 given:\n  \
                 #7 by a, draft, staleness not checked; opened 1m ago, last activity 1m ago: `a.rs`\n",
            ),
            (
                json!({"repository": "o/r", "paths": [], "pulls": [], "runs": [],
                    "complete": true}),
                "o/r: the change names no path that can be compared\n",
            ),
            (
                json!({"repository": "o/r", "paths": ["a.rs"], "pulls": [], "runs": [],
                    "complete": false}),
                "o/r: no open pull request or run in flight shares a path with the 1 given\n  \
                 (only the newest 250 open pull requests were read)\n",
            ),
        ] {
            assert_eq!(text(&report), expected);
        }
    }

    /// Inputs are refused before a token is read.
    #[test]
    fn inputs_are_checked_first() {
        let now = OffsetDateTime::now_utc();
        let refuse = || -> Result<Fake> { panic!("no token may be read") };
        for (extra, repo, why) in [
            (vec!["--paths", "a.rs"], None, "requires -R"),
            (vec!["--paths", "a.rs"], Some("no-slash"), "repository"),
            (
                vec!["--paths", "../etc/passwd"],
                Some("o/r"),
                "invalid path",
            ),
            (vec!["--paths", "a`b`.rs"], Some("o/r"), "invalid path"),
            (vec!["--paths", "/"], Some("o/r"), "invalid path"),
            (vec!["--branch", "a..b"], Some("o/r"), "invalid branch"),
            (vec!["--branch", "a?b"], Some("o/r"), "invalid branch"),
        ] {
            let error = report(&args(&extra), repo, refuse, now).unwrap_err();
            assert!(format!("{error:#}").contains(why), "{extra:?}: {error:#}");
        }
        for extra in [
            vec![],
            vec!["--paths", "a.rs", "--pr", "5"],
            vec!["--pr", "5", "--stale-draft-hours", "0"],
        ] {
            let base = ["overlap", "--caller", "o/c", "--workflow", "dispatch.yml"];
            assert!(
                Cli::try_parse_from(base.iter().chain(&extra)).is_err(),
                "{extra:?}"
            );
        }
    }
}
