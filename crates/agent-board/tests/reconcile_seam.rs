use assert_cmd::Command;
use serde_json::{Value, json};
use std::collections::BTreeMap;

// Test oracle for the proposed profile, NOT agentic-job's independent checker.
fn fits_bounds(proposals: &[Value], bounds: &Value) -> bool {
    if proposals.len() as u64 > bounds["max_outputs"].as_u64().unwrap() {
        return false;
    }
    let mut counts = BTreeMap::new();
    for proposal in proposals {
        let kind = proposal["type"].as_str().unwrap();
        let Some(max) = bounds["outputs"][kind].as_u64() else {
            return false;
        };
        let count = counts.entry(kind).or_insert(0);
        *count += 1;
        if *count > max
            || !bounds["repositories"]
                .as_array()
                .unwrap()
                .contains(&proposal["target_repo"])
            || (kind == "update_project" && proposal["project"] != bounds["project"])
        {
            return false;
        }
    }
    true
}

#[test]
fn staging_proposals_and_bounds_contract() {
    let root = format!("{}/../..", env!("CARGO_MANIFEST_DIR"));
    let bounds: Value =
        serde_json::from_str(include_str!("../../../live/reconcile-bounds.json")).unwrap();
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
    let proposals: Vec<Value> = std::str::from_utf8(&output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(proposals.len(), 2);
    assert_eq!(
        proposals
            .iter()
            .filter(|p| p["type"] == "update_project")
            .count(),
        2
    );
    assert_eq!(
        proposals
            .iter()
            .filter(|p| p["type"] == "close_issue")
            .count(),
        0
    );
    assert!(fits_bounds(&proposals, &bounds));

    for (name, replacement) in [
        (
            "another project",
            json!({"project": "https://github.com/orgs/cgwalters-forge-stage/projects/3"}),
        ),
        (
            "production project",
            json!({"project": "https://github.com/orgs/cgwalters-forge/projects/1"}),
        ),
        (
            "unlisted repository",
            json!({"target_repo": "cgwalters-forge-stage/other"}),
        ),
        ("third output type", json!({"type": "create_issue"})),
    ] {
        let mut invalid = proposals.clone();
        let update = invalid
            .iter_mut()
            .find(|p| p["type"] == "update_project")
            .unwrap();
        for (key, value) in replacement.as_object().unwrap() {
            update[key] = value.clone();
        }
        assert!(!fits_bounds(&invalid, &bounds), "{name}");
    }
    for (kind, count) in [("update_project", 4), ("close_issue", 3)] {
        let fallback = json!({"type":"close_issue","target_repo":"cgwalters-forge-stage/board-test","issue_number":2});
        let proposal = proposals
            .iter()
            .find(|p| p["type"] == kind)
            .unwrap_or(&fallback);
        assert!(
            !fits_bounds(&vec![proposal.clone(); count], &bounds),
            "{kind} cap"
        );
    }
    // The original proposals fit both per-type caps; only the total cap changes.
    let mut smaller_total = bounds.clone();
    smaller_total["max_outputs"] = json!(proposals.len() - 1);
    assert!(!fits_bounds(&proposals, &smaller_total), "total cap");
}

#[test]
fn staging_reader_uses_dedicated_environment() {
    // Regression guard for the job's binding, not proof of GitHub-side protection.
    let workflow = include_str!("../../../.github/workflows/reconcile.yml");
    assert!(workflow.contains("\n    environment: staging-project-read\n"));
    assert!(workflow.contains("GH_TOKEN: ${{ secrets.STAGING_PROJECT_READ_TOKEN }}"));
    let documentation = include_str!("../../../README.md");
    assert!(documentation.contains("only a **branch** rule for `main` and no tag rules"));
    assert!(documentation.contains("Remove any repository or organization secret"));
}

#[test]
fn missing_read_token_skips_instead_of_failing() {
    // Until the environment secret is provisioned, the scheduled preview must
    // log one notice and succeed rather than fail every ten minutes.
    let workflow = include_str!("../../../.github/workflows/reconcile.yml");
    let steps: Vec<&str> = workflow.split("\n      - ").skip(1).collect();
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
    // The no-write-token rule stays.
    assert!(workflow.contains("# Preview only. Never give this job the staging write token."));
    // Every secret reference is the read token, never the write token.
    assert_eq!(
        workflow.matches("secrets.").count(),
        workflow
            .matches("secrets.STAGING_PROJECT_READ_TOKEN")
            .count()
    );
}
