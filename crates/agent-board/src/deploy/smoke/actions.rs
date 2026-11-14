//! Fixed, model-free workflow in a freshly receipted repository. No stage
//! credential is installed there; the job uses only its own GITHUB_TOKEN.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::time::Duration;

const WORKFLOW: &str = "stage-smoke-task.yml";
pub(super) const SOURCE: &str = include_str!("../../../../../live/stage-smoke-task.yml");
pub(super) const POLLS: usize = 40;
/// Reads of a just-installed workflow before giving up on its registration.
pub(super) const REGISTRATION_POLLS: usize = 12;
pub(super) const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub(super) fn result_body(id: &str) -> String {
    format!("agent-board Actions smoke result: run {id}")
}

pub(super) fn start(
    rest: &impl Rest,
    repository_id: &str,
    repository: &str,
    item: u64,
    mut sleep: impl FnMut(),
) -> Result<(dispatch::Handle, String)> {
    let root = format!("{API_ROOT}repos/{repository}");
    // First file initializes the empty repository's default branch. Only
    // compiled-in code is uploaded, never configuration or issue-provided code.
    let (status, answer) = rest.send(
        "PUT",
        &format!("{root}/contents/.github/workflows/{WORKFLOW}"),
        &json!({"message":"Install model-free stage smoke task", "content":STANDARD.encode(SOURCE)}),
    )?;
    ensure!(status == 201, "workflow installation answered {status}");
    let sha = string(&answer, "/commit/sha").context("missing workflow commit")?;
    ensure!(
        sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid workflow commit"
    );
    let answer = rest.get_live(&root)?;
    ensure!(
        answer.status == 200
            && answer.body["node_id"] == repository_id
            && answer.body["full_name"] == repository,
        "scratch repository identity mismatch"
    );
    let branch = string(&answer.body, "/default_branch").context("missing default branch")?;
    // Actions registers a pushed workflow asynchronously; dispatching before
    // then is refused, and the single dispatch attempt must not be spent on it.
    let url = format!("{root}/actions/workflows/{WORKFLOW}");
    for attempt in 0.. {
        let (status, workflow) = rest.send("GET", &url, &Value::Null)?;
        if status == 200 {
            ensure!(
                workflow["path"] == format!(".github/workflows/{WORKFLOW}")
                    && workflow["state"] == "active",
                "installed workflow is not the active smoke task"
            );
            break;
        }
        ensure!(
            status == 404,
            "workflow registration read answered {status}"
        );
        ensure!(
            attempt + 1 < REGISTRATION_POLLS,
            "workflow not registered within {REGISTRATION_POLLS} reads"
        );
        sleep();
    }
    let target = Target {
        caller: repository.into(),
        workflow: WORKFLOW.into(),
        git_ref: branch,
    };
    target.validate()?;
    let backend = dispatch::GitHub {
        target: &target,
        run_details: true,
    };
    let start = Start {
        issue_repository: None,
        repository,
        item,
        kind: FAKE_KIND,
        brief: FAKE_TASK,
        pool: None,
        effort: None,
        runner: None,
        base: None,
    };
    let mut sent = false;
    let outcome = backend.start(
        &start,
        || {
            ensure!(!sent, "smoke dispatch is never retried");
            sent = true;
            Ok(())
        },
        |path, body| rest.send("POST", &format!("{API_ROOT}{path}"), body),
    );
    let StartOutcome::Started(handle) = outcome else {
        bail!("Actions dispatch did not return an attributable run; not retried: {outcome:?}");
    };
    ensure!(
        handle.id.parse::<u64>().is_ok_and(|id| id > 0),
        "invalid Actions run ID"
    );
    Ok((handle, sha))
}

pub(super) fn wait(
    rest: &impl Rest,
    repository: &str,
    id: &str,
    sha: &str,
    cancelled: &mut dyn FnMut() -> bool,
    mut sleep: impl FnMut(),
) -> Result<()> {
    for attempt in 0..POLLS {
        if cancelled() {
            return Err(Cancelled.into());
        }
        let url = format!("{API_ROOT}repos/{repository}/actions/runs/{id}");
        let (status, run) = rest.send("GET", &url, &Value::Null)?;
        // The dispatch answer can precede the run's read visibility.
        if status == 404 {
            if attempt + 1 < POLLS {
                sleep();
            }
            continue;
        }
        ensure!(
            status == 200
                && run["id"].as_u64().map(|v| v.to_string()).as_deref() == Some(id)
                && run["repository"]["full_name"] == repository
                && run["head_sha"] == sha
                && run["event"] == "workflow_dispatch"
                && run["path"] == format!(".github/workflows/{WORKFLOW}"),
            "Actions run identity mismatch"
        );
        if run["status"] == "completed" {
            ensure!(
                run["conclusion"] == "success",
                "Actions smoke task did not succeed"
            );
            return Ok(());
        }
        ensure!(
            matches!(
                run["status"].as_str(),
                Some("queued" | "in_progress" | "waiting" | "pending" | "requested")
            ),
            "unknown Actions run status"
        );
        if attempt + 1 < POLLS {
            sleep();
        }
    }
    bail!("Actions smoke task did not complete within {POLLS} polls")
}
