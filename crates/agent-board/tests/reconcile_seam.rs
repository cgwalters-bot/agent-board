use assert_cmd::Command;
use board_core::{Action, Intent, ItemRef, PLAN_SCHEMA, Plan};
use board_forge::{GitHubSafeOutputs, OutputBackend};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const BOUNDS: &str = include_str!("../../../.github/agentic-job/reconcile.toml");
const WORKFLOW: &str = include_str!("../../../.github/workflows/reconcile.yml");

// The subset of agentic-job's TOML bounds that reconcile.toml uses, read as
// JSON without adding a TOML dependency to this crate. Any other line fails
// the test, so a bound added later cannot be silently skipped by the oracle.
fn parse_bounds(text: &str) -> Value {
    let mut bounds = json!({"outputs": {}});
    // The table lines go to: the top level, then [outputs], then [outputs.TYPE].
    let mut table: Vec<String> = vec![];
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            table = name.split('.').map(str::to_owned).collect();
            assert_eq!(table[0], "outputs", "{line}");
            assert!(table.len() <= 2, "{line}");
            continue;
        }
        let (key, value) = line.split_once(" = ").expect(line);
        let value = value.replace("{ max = ", "{\"max\": ");
        let value: Value = serde_json::from_str(&value).expect(line);
        let target = table
            .iter()
            .fold(&mut bounds, |target, name| &mut target[name.as_str()]);
        target[key] = value;
    }
    bounds
}

// Test oracle for agentic-job's documented checks on these types
// (docs/safe-outputs.md), NOT its independent checker: that runs in the
// workflow's check job and remains the boundary.
fn fits_bounds(proposals: &[Value], bounds: &Value) -> bool {
    if proposals.len() as u64 > bounds["max_outputs"].as_u64().unwrap() {
        return false;
    }
    let repos = bounds["repos"].as_array().unwrap();
    let mut counts = BTreeMap::new();
    for proposal in proposals {
        let kind = proposal["type"].as_str().unwrap();
        let Some(max) = bounds["outputs"][kind]["max"].as_u64() else {
            return false;
        };
        let count = counts.entry(kind).or_insert(0);
        *count += 1;
        if *count > max {
            return false;
        }
        let fits = match kind {
            "update_project" => {
                let limits = &bounds["outputs"]["update_project"];
                let fields = limits["fields"].as_array().unwrap();
                repos.contains(&proposal["target_repo"])
                    && limits["projects"]
                        .as_array()
                        .unwrap()
                        .contains(&proposal["project"])
                    && proposal["content_type"] == "issue"
                    && proposal["content_number"].as_u64().is_some_and(|n| n > 0)
                    && proposal["fields"].as_object().is_some_and(|f| {
                        !f.is_empty() && f.keys().all(|k| fields.contains(&json!(k)))
                    })
            }
            "close_issue" => {
                repos.contains(&proposal["repo"])
                    && proposal["issue_number"].as_u64().is_some_and(|n| n > 0)
                    && proposal["body"].as_str().is_some_and(|b| !b.is_empty())
            }
            _ => false,
        };
        if !fits {
            return false;
        }
    }
    true
}

fn staging_proposals() -> Vec<Value> {
    let root = format!("{}/../..", env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(assert_cmd::cargo::cargo_bin!("agent-board"))
        .args([
            "reconcile",
            "plan",
            "--snapshot",
            &format!("{root}/fixtures/staging-reconcile.json"),
            "--emit",
        ])
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    std::str::from_utf8(&output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

// No staging rule proposes a closure yet, so lower one directly: the format
// is what agentic-job's check sees whichever rule produces it.
fn close_issue(repository: &str, number: u64) -> Value {
    let plan = Plan {
        schema: PLAN_SCHEMA.into(),
        project: "https://github.com/orgs/cgwalters-forge-stage/projects/2"
            .parse()
            .unwrap(),
        actions: vec![Action {
            key: "close".into(),
            reason: "accepted_completion".into(),
            intent: Intent::CloseIssue {
                item: ItemRef {
                    host: "github.com".into(),
                    repository: repository.into(),
                    number,
                },
            },
            prerequisites: vec![],
            evidence: vec![],
        }],
        diagnostics: Default::default(),
    };
    GitHubSafeOutputs.lower(&plan).unwrap().remove(0)
}

fn with(proposal: &Value, replacement: Value) -> Value {
    let mut proposal = proposal.clone();
    for (key, value) in replacement.as_object().unwrap() {
        proposal[key] = value.clone();
    }
    proposal
}

#[test]
fn staging_proposals_and_bounds_contract() {
    let bounds = parse_bounds(BOUNDS);
    let proposals = staging_proposals();
    assert_eq!(proposals.len(), 2);
    assert!(proposals.iter().all(|p| p["type"] == "update_project"));
    assert!(fits_bounds(&proposals, &bounds));

    let close = close_issue("cgwalters-forge-stage/board-test", 3);
    let mut with_close = proposals.clone();
    with_close.push(close.clone());
    assert!(fits_bounds(&with_close, &bounds), "a closure fits");

    let update = &proposals[0];
    let mut no_body = close.as_object().unwrap().clone();
    no_body.remove("body");
    for (name, invalid) in [
        (
            "another project",
            with(
                update,
                json!({"project": "https://github.com/orgs/cgwalters-forge-stage/projects/3"}),
            ),
        ),
        (
            "production project",
            with(
                update,
                json!({"project": "https://github.com/orgs/cgwalters-forge/projects/1"}),
            ),
        ),
        (
            "unlisted repository",
            with(
                update,
                json!({"target_repo": "cgwalters-forge-stage/other"}),
            ),
        ),
        (
            "unlisted field",
            with(update, json!({"fields": {"Priority": "P1"}})),
        ),
        (
            "pull request content",
            with(update, json!({"content_type": "pull_request"})),
        ),
        (
            "third output type",
            with(update, json!({"type": "create_issue"})),
        ),
        ("noop", json!({"type":"noop","message":"x"})),
        (
            "closure elsewhere",
            close_issue("cgwalters-forge-stage/other", 3),
        ),
        // The format lowering used to emit; agentic-job requires `repo`.
        (
            "closure with target_repo",
            json!({"type":"close_issue","target_repo":"cgwalters-forge-stage/board-test","issue_number":3}),
        ),
        ("closure without body", Value::Object(no_body)),
    ] {
        assert!(!fits_bounds(&[invalid], &bounds), "{name}");
    }
    for (proposal, count) in [(update, 4), (&close, 3)] {
        assert!(
            !fits_bounds(&vec![proposal.clone(); count], &bounds),
            "{} cap",
            proposal["type"]
        );
    }
    // Three updates and two closures fill both type caps and the total.
    let mut full = vec![update.clone(); 3];
    full.extend([close.clone(), with(&close, json!({"issue_number": 4}))]);
    assert!(fits_bounds(&full, &bounds));
    // The total is checked on its own, not only through the type caps.
    let mut smaller_total = bounds.clone();
    smaller_total["max_outputs"] = json!(full.len() - 1);
    assert!(!fits_bounds(&full, &smaller_total), "total cap");
}

#[test]
fn apply_call_stays_within_bounds() {
    let bounds = parse_bounds(BOUNDS);
    let with_input = |name: &str| {
        WORKFLOW
            .lines()
            .find_map(|line| line.trim().strip_prefix(&format!("{name}: ")))
            .unwrap_or_else(|| panic!("no {name} input"))
            .trim_matches('\'')
            .to_owned()
    };
    assert_eq!(with_input("allow"), ".github/agentic-job/reconcile.toml");
    let max: u64 = with_input("max-outputs").parse().unwrap();
    assert!(max <= bounds["max_outputs"].as_u64().unwrap());
    let outputs: &Map<String, Value> = bounds["outputs"].as_object().unwrap();
    for kind in with_input("outputs").split(',') {
        assert!(outputs.contains_key(kind), "{kind} not in bounds");
    }
    // agentic-job refuses update_project without an apply environment.
    assert_eq!(with_input("apply-environment"), "staging-project-apply");
    // The board is configuration (repository variables and the bounds file),
    // not workflow code.
    assert!(!WORKFLOW.contains("cgwalters-forge-stage"));
    assert!(WORKFLOW.contains("repo: ${{ vars.RECONCILE_REPO }}"));
}

#[test]
fn staging_reader_uses_dedicated_environment() {
    // Regression guard for the job's binding, not proof of GitHub-side protection.
    assert!(WORKFLOW.contains("\n    environment: staging-project-read\n"));
    assert!(WORKFLOW.contains("GH_TOKEN: ${{ secrets.STAGING_PROJECT_READ_TOKEN }}"));
    let documentation = include_str!("../../../docs/setup.md");
    assert!(documentation.contains("only a **branch** rule for `main` and no tag rules"));
    assert!(documentation.contains("Remove any repository or organization secret"));
}

#[test]
fn missing_read_token_skips_instead_of_failing() {
    // Until the environment secret is provisioned, the scheduled run must
    // log one notice and succeed rather than fail every ten minutes.
    let (propose, apply) = WORKFLOW.split_once("\n  apply:\n").expect("no apply job");
    let steps: Vec<&str> = propose.split("\n      - ").skip(1).collect();
    assert!(steps.len() >= 2, "no steps found");
    let (first, rest) = steps.split_first().unwrap();
    assert!(first.contains("id: read-token"));
    // A typo in either output value would silently disable the gates below.
    assert!(first.contains("present=true"));
    assert!(first.contains("present=false"));
    assert!(first.contains("::notice::"));
    assert!(
        first.contains("STAGING_PROJECT_READ_TOKEN: ${{ secrets.STAGING_PROJECT_READ_TOKEN }}")
    );
    // Gating the check on its own output would make every run a silent no-op.
    assert!(!first.contains("\n        if:"));
    for step in rest {
        assert!(
            step.contains("if: steps.read-token.outputs.present == 'true'"),
            "step missing skip condition: {}",
            step.lines().next().unwrap_or(step)
        );
    }
    // Apply runs only on proposals, and so never on a skip or an empty plan.
    assert!(propose.contains("proposals: ${{ steps.plan.outputs.proposals }}"));
    assert!(propose.contains("echo \"proposals=true\" >> \"$GITHUB_OUTPUT\""));
    assert!(apply.contains("if: needs.propose.outputs.proposals == 'true'"));
    // The producer holds only the read token; only apply names the write token.
    assert!(propose.contains("# Never give this job the staging write token."));
    assert_eq!(
        propose.matches("secrets.").count(),
        propose
            .matches("secrets.STAGING_PROJECT_READ_TOKEN")
            .count()
    );
    assert_eq!(apply.matches("secrets.").count(), 1);
    assert!(apply.contains("SAFE_OUTPUTS_PAT: ${{ secrets.SAFE_OUTPUTS_PAT }}"));
    assert!(!WORKFLOW.contains("secrets: inherit"));
}
