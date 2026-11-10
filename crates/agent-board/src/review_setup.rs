//! Explicit setup diagnostic: keep Variables-read credentials out of the pass.
use anyhow::{Context, Result, ensure};
use board_forge::{github::Client, pass::Config, poll::Rest};
use clap::Args;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

const MAX_TARGETS: usize = 50;
const WORKFLOW_PAGE: usize = 100;
const VARIABLE_PAGE: usize = 30;
const DEADLINE: Duration = Duration::from_secs(120);
const REQUIRED: [&str; 4] = [
    "INFERENCE_URL",
    "INFERENCE_AUDIENCE",
    "AGENT_RUNNER",
    "AGENT_REAL_REVIEWS",
];

#[derive(Args)]
pub struct Options {
    /// Trusted pass config; checks workRepositories and pr.reviewWorkflow.
    #[arg(long, value_name = "FILE")]
    config: PathBuf,
}

fn targets(config: &Config) -> Result<Vec<String>> {
    ensure!(
        config.forgejo_board().is_none(),
        "review-setup supports GitHub only"
    );
    ensure!(
        config.pr.review_workflow.is_some(),
        "review-setup needs pr.reviewWorkflow"
    );
    let mut targets = config.work_repositories.clone();
    targets.sort_by_key(|r| r.to_ascii_lowercase());
    targets.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    ensure!(!targets.is_empty(), "review-setup needs workRepositories");
    // Refuse rather than silently check only a prefix of dispatch targets.
    ensure!(
        targets.len() <= MAX_TARGETS,
        "review-setup supports at most {MAX_TARGETS} targets"
    );
    Ok(targets)
}

fn complete<'a>(value: &'a Value, field: &str, limit: usize) -> Result<&'a [Value]> {
    let entries = value[field].as_array().context("invalid listing")?;
    ensure!(
        entries.len() <= limit && value["total_count"].as_u64() == Some(entries.len() as u64),
        "incomplete listing"
    );
    Ok(entries)
}

fn variables(value: &Value) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for entry in complete(value, "variables", VARIABLE_PAGE)? {
        let name = entry["name"]
            .as_str()
            .context("invalid variable name")?
            .to_ascii_uppercase();
        let value = entry["value"].as_str().context("invalid variable value")?;
        ensure!(
            result.insert(name, value.into()).is_none(),
            "duplicate variable"
        );
    }
    Ok(result)
}

fn inspect(
    repository: &str,
    workflow: &str,
    read: &mut impl FnMut(&str) -> Result<Value>,
) -> Result<String> {
    let root = format!("repos/{repository}/actions");
    let value = read(&format!("{root}/workflows?per_page={WORKFLOW_PAGE}"))?;
    let workflows = complete(&value, "workflows", WORKFLOW_PAGE)?;
    let path = format!(".github/workflows/{workflow}");
    // A 404, denied read or truncated listing is never proof of absence.
    for item in workflows {
        ensure!(item["path"].is_string(), "invalid workflow path");
    }
    let Some(found) = workflows.iter().find(|w| w["path"] == path) else {
        return Ok("not applicable (no configured review workflow)".into());
    };
    ensure!(found["state"] == "active", "review workflow is not active");
    let mut effective = variables(&read(&format!(
        "{root}/organization-variables?per_page={VARIABLE_PAGE}"
    ))?)?;
    // Repository values override shared organization values, including empty ones.
    effective.extend(variables(&read(&format!(
        "{root}/variables?per_page={VARIABLE_PAGE}"
    ))?)?);
    let missing: Vec<_> = REQUIRED
        .into_iter()
        .filter(|name| effective.get(*name).is_none_or(|v| v.trim().is_empty()))
        .collect();
    ensure!(
        missing.is_empty(),
        "missing or empty variables: {}",
        missing.join(", ")
    );
    ensure!(
        effective["AGENT_REAL_REVIEWS"] == "true",
        "AGENT_REAL_REVIEWS must be true"
    );
    Ok("ready (review wrapper variables configured)".into())
}

fn check(
    config: &Config,
    targets: &[String],
    mut read: impl FnMut(&str) -> Result<Value>,
    out: &mut impl Write,
) -> Result<()> {
    let mut failures = 0;
    for repository in targets {
        let workflow = board_forge::poll::routed_review_workflow(
            &config.pr.poll.review_dispatch_repositories,
            config.pr.review_workflow.as_deref(),
            repository,
        )
        .context("pr.reviewWorkflow required")?;
        match inspect(repository, workflow, &mut read) {
            Ok(state) => writeln!(out, "review-setup {repository}: {state}")?,
            Err(error) => {
                failures += 1;
                writeln!(
                    out,
                    "review-setup {repository}: blocked or unverified: {error}"
                )?;
            }
        }
    }
    ensure!(
        failures == 0,
        "review setup failed for {failures} repositories"
    );
    Ok(())
}

pub fn run(options: &Options) -> Result<()> {
    let config = crate::config::pass(
        &std::fs::read_to_string(&options.config).context("read pass config")?,
    )?;
    let targets = targets(&config)?;
    let started = Instant::now();
    let client = Client::from_env()?.retry_within(started + DEADLINE);
    check(
        &config,
        &targets,
        |path| {
            ensure!(started.elapsed() < DEADLINE, "setup read deadline reached");
            read(&client, path)
        },
        &mut std::io::stdout().lock(),
    )
}

fn read(client: &impl Rest, path: &str) -> Result<Value> {
    // Transport errors/bodies may contain values or addresses: never print them.
    let url = format!("{}{path}", board_forge::github::API_ROOT);
    let response = client.get(&url, None).map_err(|_| {
        anyhow::anyhow!("GitHub read failed; verify Actions and Variables read permissions")
    })?;
    ensure!(
        response.status == 200
            && response
                .link
                .as_deref()
                .and_then(board_forge::poll::next_link)
                .is_none(),
        "GitHub listing unavailable or incomplete (HTTP {})",
        response.status
    );
    Ok(response.body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vars() -> Value {
        json!({"total_count":4,"variables":REQUIRED.map(|name| json!({"name":name,"value":if name == "AGENT_REAL_REVIEWS" {"true"} else {"configured"}}))})
    }

    #[test]
    fn review_setup_config_to_transport_reports_ready_and_routed_missing() {
        use std::cell::RefCell;
        struct Fake(RefCell<Vec<String>>);
        impl Rest for Fake {
            fn get(
                &self,
                url: &str,
                _: Option<(&str, &str)>,
            ) -> Result<board_forge::poll::RestResponse> {
                self.0.borrow_mut().push(url.into());
                let (repo, endpoint) = url
                    .strip_prefix("https://api.github.com/repos/owner/")
                    .unwrap()
                    .split_once("/actions/")
                    .unwrap();
                let body = match endpoint {
                    "workflows?per_page=100" => {
                        json!({"total_count":1,"workflows":[{"path":format!(".github/workflows/{}", if repo == "routed" {"dispatch.yml"} else {"review.yml"}),"state":"active"}]})
                    }
                    "organization-variables?per_page=30" => json!({"total_count":0,"variables":[]}),
                    "variables?per_page=30" if repo == "ready" => vars(),
                    "variables?per_page=30" => json!({"total_count":0,"variables":[]}),
                    _ => panic!("unexpected endpoint {url}"),
                };
                Ok(board_forge::poll::RestResponse {
                    status: 200,
                    etag: None,
                    last_modified: None,
                    link: None,
                    body,
                })
            }
            fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
                panic!("must never write")
            }
        }
        let mut value: Value =
            serde_json::from_str(include_str!("../../../fixtures/pass/config.json")).unwrap();
        value["workRepositories"] = json!(["owner/ready", "owner/routed"]);
        value["follow"] = value["workRepositories"].clone();
        value["pr"]["reviewWorkflow"] = json!("review.yml");
        value["pr"]["reviewPools"] = json!(["claude"]);
        value["pr"]["fixPools"] = json!(["claude"]);
        value["pr"]["reviewDispatchRepositories"] = json!(["owner/routed"]);
        let config = Config::parse(&value.to_string()).unwrap();
        let fake = Fake(RefCell::new(Vec::new()));
        let mut out = Vec::new();
        let result = check(
            &config,
            &targets(&config).unwrap(),
            |path| read(&fake, path),
            &mut out,
        );
        assert!(result.is_err());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("owner/ready: ready"));
        assert!(text.contains(
            "owner/routed: blocked or unverified: missing or empty variables: INFERENCE_URL"
        ));
        assert_eq!(fake.0.borrow().len(), 6);
    }

    #[test]
    fn review_setup_target_scope_and_bound() {
        let mut config = Config::parse(include_str!("../../../fixtures/pass/config.json")).unwrap();
        config.pr.review_workflow = Some("review.yml".into());
        config.work_repositories = vec!["owner/b".into(), "owner/a".into(), "OWNER/A".into()];
        assert_eq!(targets(&config).unwrap().len(), 2);
        config.work_repositories = (0..MAX_TARGETS).map(|n| format!("owner/r{n}")).collect();
        assert_eq!(targets(&config).unwrap().len(), MAX_TARGETS);
        config.work_repositories.push("owner/extra".into());
        assert!(
            targets(&config)
                .unwrap_err()
                .to_string()
                .contains("at most")
        );
        config.work_repositories.clear();
        assert!(targets(&config).is_err());
        config.work_repositories.push("owner/a".into());
        config.pr.review_workflow = None;
        assert!(targets(&config).is_err());
    }

    #[test]
    fn review_setup_transport_origin_errors_and_pagination() {
        struct Fake(u16, bool);
        impl Rest for Fake {
            fn get(
                &self,
                url: &str,
                _: Option<(&str, &str)>,
            ) -> Result<board_forge::poll::RestResponse> {
                assert_eq!(
                    url,
                    "https://api.github.com/repos/owner/repo/actions/variables?per_page=30"
                );
                if self.0 == 0 {
                    anyhow::bail!("sensitive transport detail")
                }
                Ok(board_forge::poll::RestResponse {
                    status: self.0,
                    etag: None,
                    last_modified: None,
                    link: self
                        .1
                        .then(|| "<https://api.github.com/next>; rel=\"next\"".into()),
                    body: json!({"message":"sensitive response detail"}),
                })
            }
            fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
                panic!("diagnostic must never write")
            }
        }
        for (status, next) in [(0, false), (403, false), (404, false), (200, true)] {
            let error = read(
                &Fake(status, next),
                "repos/owner/repo/actions/variables?per_page=30",
            )
            .unwrap_err();
            assert!(!format!("{error:#}").contains("sensitive"));
        }
        assert!(
            read(
                &Fake(200, false),
                "repos/owner/repo/actions/variables?per_page=30"
            )
            .is_ok()
        );
    }

    #[test]
    fn review_setup_permutations() {
        for (name, workflow, org, repo, expected, reads) in [
            (
                "ready",
                json!({"path":".github/workflows/review.yml","state":"active"}),
                vars(),
                json!({"total_count":0,"variables":[]}),
                "ready",
                3,
            ),
            (
                "repository",
                json!({"path":".github/workflows/review.yml","state":"active"}),
                json!({"total_count":0,"variables":[]}),
                vars(),
                "ready",
                3,
            ),
            (
                "absent",
                json!({"path":".github/workflows/ci.yml","state":"active"}),
                vars(),
                vars(),
                "not applicable",
                1,
            ),
            (
                "disabled",
                json!({"path":".github/workflows/review.yml","state":"disabled_manually"}),
                vars(),
                vars(),
                "not active",
                1,
            ),
            (
                "missing",
                json!({"path":".github/workflows/review.yml","state":"active"}),
                json!({"total_count":0,"variables":[]}),
                json!({"total_count":0,"variables":[]}),
                "INFERENCE_URL, INFERENCE_AUDIENCE, AGENT_RUNNER, AGENT_REAL_REVIEWS",
                3,
            ),
            (
                "override",
                json!({"path":".github/workflows/review.yml","state":"active"}),
                vars(),
                json!({"total_count":1,"variables":[{"name":"INFERENCE_URL","value":" "}]}),
                "INFERENCE_URL",
                3,
            ),
            (
                "false",
                json!({"path":".github/workflows/review.yml","state":"active"}),
                vars(),
                json!({"total_count":1,"variables":[{"name":"AGENT_REAL_REVIEWS","value":"false"}]}),
                "must be true",
                3,
            ),
        ] {
            let mut replies =
                [json!({"total_count":1,"workflows":[workflow]}), org, repo].into_iter();
            let mut paths = Vec::new();
            let result = inspect("owner/repo", "review.yml", &mut |path| {
                paths.push(path.to_owned());
                Ok(replies.next().unwrap())
            });
            let text = result.unwrap_or_else(|e| e.to_string());
            assert!(text.contains(expected), "{name}: {text}");
            assert_eq!(paths.len(), reads, "{name}");
            assert_eq!(paths[0], "repos/owner/repo/actions/workflows?per_page=100");
        }
    }

    #[test]
    fn review_setup_incomplete_or_malformed_is_not_ready() {
        for listing in [
            json!({}),
            json!({"total_count":1,"variables":[]}),
            json!({"total_count":1,"variables":[{"name":"INFERENCE_URL"}]}),
            json!({"total_count":2,"variables":[{"name":"A","value":"x"},{"name":"a","value":"x"}]}),
        ] {
            assert!(variables(&listing).is_err());
        }
        assert!(
            complete(
                &json!({"total_count":101,"workflows":[]}),
                "workflows",
                WORKFLOW_PAGE
            )
            .is_err()
        );
    }

    #[test]
    fn review_setup_continues_after_failure_and_returns_failure() {
        let mut out = Vec::new();
        let mut calls = 0;
        let mut config = Config::parse(include_str!("../../../fixtures/pass/config.json")).unwrap();
        config.pr.review_workflow = Some("review.yml".into());
        let result = check(
            &config,
            &["owner/a".into(), "owner/b".into()],
            |_| {
                calls += 1;
                if calls == 1 {
                    anyhow::bail!("denied")
                }
                Ok(json!({"total_count":0,"workflows":[]}))
            },
            &mut out,
        );
        assert!(result.is_err());
        assert_eq!(calls, 2);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("owner/a: blocked or unverified"));
        assert!(text.contains("owner/b: not applicable"));
    }
}
