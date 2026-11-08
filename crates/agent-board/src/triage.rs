//! Explicit operator decisions, not an automatic consumer of agent output.
//! The cap prevents a mistaken bulk list from making unbounded forge writes.
use anyhow::{Context, Result, ensure};
use board_forge::github::{DEFAULT_HOST, is_github_login, parse_github_issue};
use board_forge::poll::Rest;
use clap::Args;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::PathBuf;

const MAX_WRITES: usize = 100;
const MAX_INPUT_BYTES: u64 = 1024 * 1024;

#[derive(Args)]
pub struct Apply {
    /// Operator-reviewed JSON array of {item, verdict, decision}; not fetched from agent comments.
    #[arg(long, value_name = "FILE")]
    verdicts: PathBuf,
    /// Operator login to unassign; other assignees are preserved.
    #[arg(long, value_name = "LOGIN")]
    operator: String,
    /// Print the planned requests without writing (the default).
    #[arg(long, conflicts_with = "apply")]
    dry_run: bool,
    /// Apply with your GH_TOKEN; this is the operator's acceptance of the list.
    #[arg(long)]
    apply: bool,
    /// Maximum write requests for the whole list, 0..100; refused before any writes if exceeded.
    #[arg(long, default_value_t = 20)]
    max_writes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Verdict {
    item: String,
    verdict: String,
    decision: String,
}

pub fn run<C: Rest>(
    args: &Apply,
    repo: Option<&str>,
    host: &str,
    client: impl FnOnce() -> Result<C>,
    out: &mut impl Write,
) -> Result<()> {
    ensure!(
        host == DEFAULT_HOST,
        "triage apply supports only github.com"
    );
    ensure!(is_github_login(&args.operator), "invalid operator login");
    ensure!(
        args.max_writes <= MAX_WRITES,
        "--max-writes must be 0..{MAX_WRITES}"
    );
    let mut input = String::new();
    std::fs::File::open(&args.verdicts)
        .context("open verdict list")?
        .take(MAX_INPUT_BYTES + 1)
        .read_to_string(&mut input)
        .context("read verdict list")?;
    let items = parse(&input, repo, host)?;
    // Every row always posts one comment. Bound reads even when no labels or
    // assignments need changing, without introducing a second caller control.
    ensure!(
        items.len() <= args.max_writes,
        "verdict list exceeds --max-writes"
    );
    if items.is_empty() {
        return Ok(());
    }
    let client = client()?;
    let mut requests = Vec::new();
    for (item, verdict) in items {
        let url = format!(
            "https://api.github.com/repos/{}/issues/{}",
            item.repository, item.number
        );
        let response = client
            .get(&url, None)
            .with_context(|| format!("read {}", item.display()))?;
        ensure!(
            response.status == 200,
            "GET {url}: HTTP {}",
            response.status
        );
        let issue = response.body;
        ensure!(
            issue.get("pull_request").is_none(),
            "{} is a pull request, not an issue",
            item.display()
        );
        let labels = issue["labels"].as_array().context("issue labels missing")?;
        let assignees = issue["assignees"]
            .as_array()
            .context("issue assignees missing")?;
        requests.push(json!({"method":"POST", "url":format!("{url}/comments"), "body":{"body":format!("Triage verdict: {}\n\n{}", verdict.verdict, verdict.decision)}}));
        for label in ["needs-human", "needs-operator"] {
            if labels.iter().any(|l| l["name"] == label) {
                requests.push(
                    json!({"method":"DELETE", "url":format!("{url}/labels/{label}"), "body":null}),
                );
            }
        }
        if assignees.iter().any(|a| {
            a["login"]
                .as_str()
                .is_some_and(|login| login.eq_ignore_ascii_case(&args.operator))
        }) {
            requests.push(json!({"method":"DELETE", "url":format!("{url}/assignees"), "body":{"assignees":[args.operator]}}));
        }
    }
    ensure!(
        requests.len() <= args.max_writes,
        "{} writes exceed --max-writes {}; nothing changed",
        requests.len(),
        args.max_writes
    );
    for (done, request) in requests.iter().enumerate() {
        if args.apply {
            let method = request["method"].as_str().context("request method")?;
            let url = request["url"].as_str().context("request URL")?;
            let (status, _) = client
                .send(method, url, &request["body"])
                .with_context(|| {
                    format!(
                        "{done} writes completed; {method} {url} failed; inspect before retrying"
                    )
                })?;
            ensure!(
                (200..300).contains(&status),
                "{done} writes completed; {method} {url}: HTTP {status}; inspect before retrying"
            );
        }
        serde_json::to_writer(&mut *out, request)?;
        writeln!(out)?;
    }
    if !args.apply {
        eprintln!("Dry run; nothing changed. Pass --apply to accept and apply these decisions.");
    }
    Ok(())
}

fn parse(
    input: &str,
    repo: Option<&str>,
    host: &str,
) -> Result<Vec<(board_core::ItemRef, Verdict)>> {
    ensure!(
        input.len() as u64 <= MAX_INPUT_BYTES,
        "verdict list exceeds 1 MiB"
    );
    let verdicts: Vec<Verdict> = serde_json::from_str(input).context("decode verdict list")?;
    let mut seen = BTreeSet::new();
    verdicts
        .into_iter()
        .map(|v| {
            ensure!(
                !v.verdict.trim().is_empty() && !v.decision.trim().is_empty(),
                "verdict and decision must not be blank"
            );
            let item = parse_github_issue(&v.item, repo, host)?;
            ensure!(
                seen.insert((item.repository.to_ascii_lowercase(), item.number)),
                "duplicate item {}",
                item.display()
            );
            Ok((item, v))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use board_forge::poll::RestResponse;
    use serde_json::Value;
    use std::cell::RefCell;

    struct Forge {
        issue: Value,
        reads: RefCell<Vec<String>>,
        writes: RefCell<Vec<Value>>,
        fail_at: Option<usize>,
    }

    impl Forge {
        fn new(issue: Value) -> Self {
            Self {
                issue,
                reads: RefCell::default(),
                writes: RefCell::default(),
                fail_at: None,
            }
        }
    }

    impl Rest for &Forge {
        fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
            self.reads.borrow_mut().push(url.into());
            Ok(RestResponse {
                status: 200,
                etag: None,
                last_modified: None,
                link: None,
                body: self.issue.clone(),
            })
        }

        fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
            self.writes
                .borrow_mut()
                .push(json!({"method":method, "url":url, "body":body}));
            let status = if self.fail_at == Some(self.writes.borrow().len()) {
                403
            } else {
                200
            };
            Ok((status, Value::Null))
        }
    }

    fn args(name: &str, input: &str) -> Apply {
        let path = std::env::temp_dir().join(format!("triage-{name}-{}.json", std::process::id()));
        std::fs::write(&path, input).unwrap();
        Apply {
            verdicts: path,
            operator: "operator".into(),
            dry_run: false,
            apply: false,
            max_writes: 20,
        }
    }

    const LIST: &str = r#"[{"item":"example/work#1","verdict":"proceed","decision":"Use the recommended option."}]"#;

    #[test]
    fn preview_and_apply_use_the_same_requests_and_preserve_other_state() {
        let args = args("preview", LIST);
        let forge = Forge::new(
            json!({"labels":[{"name":"needs-human"},{"name":"needs-operator"},{"name":"keep"}],"assignees":[{"login":"OPERATOR"},{"login":"someone-else"}]}),
        );
        let mut preview = Vec::new();
        run(&args, None, DEFAULT_HOST, || Ok(&forge), &mut preview).unwrap();
        assert!(forge.writes.borrow().is_empty());
        let requests: Vec<Value> = String::from_utf8(preview.clone())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests[0]["body"]["body"],
            "Triage verdict: proceed\n\nUse the recommended option."
        );
        assert!(
            requests[1]["url"]
                .as_str()
                .unwrap()
                .ends_with("/labels/needs-human")
        );
        assert!(
            requests[2]["url"]
                .as_str()
                .unwrap()
                .ends_with("/labels/needs-operator")
        );
        assert_eq!(requests[3]["body"], json!({"assignees":["operator"]}));
        let mut applied = Vec::new();
        run(
            &Apply {
                apply: true,
                ..args
            },
            None,
            DEFAULT_HOST,
            || Ok(&forge),
            &mut applied,
        )
        .unwrap();
        assert_eq!(preview, applied);
        assert_eq!(*forge.writes.borrow(), requests);
    }

    #[test]
    fn absent_holds_only_post_the_decision() {
        let args = args("absent", LIST);
        let forge = Forge::new(json!({"labels":[],"assignees":[{"login":"other"}]}));
        run(
            &Apply {
                apply: true,
                max_writes: 1,
                ..args
            },
            None,
            DEFAULT_HOST,
            || Ok(&forge),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(forge.writes.borrow().len(), 1);
    }

    #[test]
    fn cap_and_unreadable_facts_refuse_before_writes() {
        for (name, issue, cap, expected) in [
            (
                "cap",
                json!({"labels":[{"name":"needs-human"}],"assignees":[]}),
                1,
                "2 writes exceed",
            ),
            (
                "pr",
                json!({"pull_request":{},"labels":[],"assignees":[]}),
                20,
                "not an issue",
            ),
            ("missing", json!({"labels":[]}), 20, "assignees missing"),
        ] {
            let args = args(name, LIST);
            let forge = Forge::new(issue);
            let error = run(
                &Apply {
                    apply: true,
                    max_writes: cap,
                    ..args
                },
                None,
                DEFAULT_HOST,
                || Ok(&forge),
                &mut Vec::new(),
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
            assert!(forge.writes.borrow().is_empty());
        }
    }

    #[test]
    fn invalid_lists_are_checked_whole_before_credentials() {
        for (i, input) in [
            "{}".to_owned(),
            r#"[{"item":"example/work#1","verdict":"","decision":"yes"}]"#.into(),
            r#"[{"item":"https://github.com/example/work/pull/1","verdict":"done","decision":"yes"}]"#.into(),
            r#"[{"item":"https://elsewhere.example/example/work/issues/1","verdict":"done","decision":"yes"}]"#.into(),
            r#"[{"item":"example/work#1","verdict":"done","decision":"yes","extra":true}]"#.into(),
            format!("{},{}]", LIST.trim_end_matches(']'), LIST.trim_start_matches('[')),
            format!("{},{{\"item\":\"invalid\",\"verdict\":\"done\",\"decision\":\"yes\"}}]", LIST.trim_end_matches(']')),
        ].into_iter().enumerate() {
            let args = args(&format!("invalid-{i}"), &input);
            assert!(run(&args, None, DEFAULT_HOST, || -> Result<&Forge> { panic!("credentials read") }, &mut Vec::new()).is_err());
        }
        for (i, cap, host, operator) in [
            (0, 101, DEFAULT_HOST, "operator"),
            (1, 0, DEFAULT_HOST, "operator"),
            (2, 20, "other.example", "operator"),
            (3, 20, DEFAULT_HOST, "bad/login"),
        ] {
            let args = args(&format!("invalid-options-{i}"), LIST);
            assert!(
                run(
                    &Apply {
                        max_writes: cap,
                        operator: operator.into(),
                        ..args
                    },
                    None,
                    host,
                    || -> Result<&Forge> { panic!("credentials read") },
                    &mut Vec::new()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn failure_stops_without_retrying_or_releasing_further_holds() {
        for fail_at in 1..=4 {
            let args = args(&format!("fail-{fail_at}"), LIST);
            let mut forge = Forge::new(
                json!({"labels":[{"name":"needs-human"},{"name":"needs-operator"}],"assignees":[{"login":"operator"}]}),
            );
            forge.fail_at = Some(fail_at);
            let error = run(
                &Apply {
                    apply: true,
                    ..args
                },
                None,
                DEFAULT_HOST,
                || Ok(&forge),
                &mut Vec::new(),
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("{} writes completed", fail_at - 1))
            );
            assert_eq!(forge.writes.borrow().len(), fail_at);
        }
    }

    #[test]
    fn whole_batch_is_preflighted_before_the_first_write() {
        let input = LIST.replace(
            "}]",
            "},{\"item\":\"example/work#2\",\"verdict\":\"done\",\"decision\":\"Accepted.\"}]",
        );
        let args = args("batch", &input);
        let forge = Forge::new(json!({"labels":[{"name":"needs-human"}],"assignees":[]}));
        let error = run(
            &Apply {
                apply: true,
                max_writes: 3,
                ..args
            },
            None,
            DEFAULT_HOST,
            || Ok(&forge),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("4 writes exceed"));
        assert_eq!(forge.reads.borrow().len(), 2);
        assert!(forge.writes.borrow().is_empty());
    }

    #[test]
    fn cli_defaults_to_preview_and_dry_run_conflicts_with_apply() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from([
            "agent-board",
            "triage",
            "apply",
            "--verdicts",
            "decisions.json",
            "--operator",
            "operator",
        ])
        .unwrap();
        let crate::Command::Triage {
            command: crate::Triage::Apply(args),
        } = cli.command
        else {
            panic!("wrong command")
        };
        assert!(!args.apply);
        assert_eq!(args.max_writes, 20);
        assert!(
            crate::Cli::try_parse_from([
                "agent-board",
                "triage",
                "apply",
                "--verdicts",
                "decisions.json",
                "--operator",
                "operator",
                "--dry-run",
                "--apply"
            ])
            .is_err()
        );
    }
}
