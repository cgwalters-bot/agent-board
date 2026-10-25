//! Starting an agent job: the one request `dispatch ISSUE --apply`, the poll
//! and `supervise execute` share ([docs/dispatch.md](../../../docs/dispatch.md)).
//! [`GitHub`], a `workflow_dispatch` request to the caller's workflow, is its
//! only backend. Each caller keeps its own transport and passes it as a
//! closure, so what its fakes record is unchanged.
use crate::poll::{validate_repository, validate_workflow};
use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Value, json};

/// The job to start: the plan's or the caller's content, never its route.
#[derive(Clone, Copy, Debug)]
pub struct Start<'a> {
    /// Target repository, OWNER/NAME.
    pub repository: &'a str,
    /// Issue or pull request number in `repository`.
    pub item: u64,
    pub kind: &'a str,
    /// The task, sent as the `task` input.
    pub brief: &'a str,
    /// `None` leaves the input out; an empty one is sent empty.
    pub pool: Option<&'a str>,
    /// Not sent: the caller workflow has one runner and no such input.
    pub runner: Option<&'a str>,
    /// A stacked plan's base branch; only then is a `base` input sent.
    pub base: Option<&'a str>,
}

/// Where jobs are dispatched: the caller's workflow and the ref it runs
/// from. These come from the caller's own configuration, never from a plan.
pub struct Target {
    /// Caller repository, OWNER/NAME.
    pub caller: String,
    /// Its workflow file that takes a plan's inputs, such as dispatch.yml.
    pub workflow: String,
    pub git_ref: String,
}

impl Target {
    /// Each part goes into an API path that carries the dispatch token.
    pub fn validate(&self) -> Result<()> {
        validate_repository(&self.caller).context("invalid caller repository")?;
        validate_workflow(&self.workflow)?;
        let git_ref = &self.git_ref;
        ensure!(
            !git_ref.is_empty()
                && !git_ref.starts_with('-')
                && !git_ref.contains("..")
                && git_ref
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)),
            "invalid caller ref"
        );
        Ok(())
    }
}

/// A started run, as the backend names it.
#[derive(Clone, Debug, PartialEq)]
pub struct Handle {
    /// Opaque to callers: GitHub's `workflow_run_id`, in decimal.
    pub id: String,
    /// The run's page, for people.
    pub url: String,
}

/// What a start did.
#[derive(Debug)]
pub enum StartOutcome {
    Started(Handle),
    /// Started, but the backend did not name the run.
    Unnamed,
    /// Refused before anything could run.
    NotStarted(anyhow::Error),
    /// The outcome is unknown: the caller records it and never retries it.
    Ambiguous(anyhow::Error),
}

/// The `workflow_dispatch` backend. It does not validate `target`; callers
/// keep their own checks, [`Target::validate`] among them.
pub struct GitHub<'a> {
    pub target: &'a Target,
    /// Ask for `return_run_details`, which names the run, and on a 422 ask
    /// again without it, as GitHub instances without the field refuse it.
    pub run_details: bool,
}

impl GitHub<'_> {
    /// The request's path below the API root.
    pub fn path(&self) -> String {
        let Target {
            caller, workflow, ..
        } = self.target;
        format!("repos/{caller}/actions/workflows/{workflow}/dispatches")
    }

    /// The request body, without `return_run_details`. The inputs are the
    /// caller workflow's, all strings.
    pub fn body(&self, start: &Start) -> Value {
        let mut body = json!({
            "ref": self.target.git_ref,
            "inputs": {
                "task": start.brief,
                "repo": start.repository,
                "item": start.item.to_string(),
                "kind": start.kind,
            },
        });
        if let Some(pool) = start.pool {
            body["inputs"]["pool"] = pool.into();
        }
        if let Some(base) = start.base {
            body["inputs"]["base"] = base.into();
        }
        body
    }

    /// Send the request with `send`, which takes the path and body and
    /// answers the status and body; its `Err` is a lost answer. `admit` is
    /// asked before each request, and its `Err` starts nothing.
    pub fn start(
        &self,
        start: &Start,
        mut admit: impl FnMut() -> Result<()>,
        mut send: impl FnMut(&str, &Value) -> Result<(u16, Value)>,
    ) -> StartOutcome {
        let path = self.path();
        let mut body = self.body(start);
        if self.run_details {
            body["return_run_details"] = true.into();
        }
        loop {
            if let Err(error) = admit() {
                return StartOutcome::NotStarted(error);
            }
            let (status, answer) = match send(&path, &body) {
                Ok(sent) => sent,
                Err(error) => return StartOutcome::Ambiguous(error),
            };
            if (200..300).contains(&status) {
                return match answer["workflow_run_id"].as_u64() {
                    Some(id) => StartOutcome::Started(Handle {
                        id: id.to_string(),
                        url: format!(
                            "https://github.com/{}/actions/runs/{id}",
                            self.target.caller
                        ),
                    }),
                    None => StartOutcome::Unnamed,
                };
            }
            if status == 422
                && body
                    .as_object_mut()
                    .and_then(|b| b.remove("return_run_details"))
                    .is_some()
            {
                continue;
            }
            let error = anyhow!(
                "POST {path}: GitHub HTTP status {status}: {}",
                answer["message"].as_str().unwrap_or("message unavailable")
            );
            // A refusal started nothing; a server error may have.
            return match (400..500).contains(&status) {
                true => StartOutcome::NotStarted(error),
                false => StartOutcome::Ambiguous(error),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    #[test]
    fn github_start_table() {
        let target = Target {
            caller: "example/tracker".into(),
            workflow: "dispatch.yml".into(),
            git_ref: "main".into(),
        };
        let start = Start {
            repository: "example/app",
            item: 7,
            kind: "fix",
            brief: "Fix it.",
            pool: None,
            runner: Some("hosted"),
            base: None,
        };
        let path = "repos/example/tracker/actions/workflows/dispatch.yml/dispatches";
        let plain = json!({"ref": "main", "inputs": {"task": "Fix it.", "repo": "example/app", "item": "7", "kind": "fix"}});
        let mut detailed = plain.clone();
        detailed["return_run_details"] = true.into();
        // Answers in turn, `None` a lost one.
        type Answers = Vec<Option<(u16, Value)>>;
        // Whether run details are asked for; the answers; the bodies sent;
        // what came of it.
        let cases: &[(&str, bool, Answers, Vec<&Value>, &str)] = &[
            (
                "handle",
                true,
                vec![Some((200, json!({"workflow_run_id": 42})))],
                vec![&detailed],
                "started 42 https://github.com/example/tracker/actions/runs/42",
            ),
            (
                "422, then without run details",
                true,
                vec![Some((422, json!({}))), Some((204, Value::Null))],
                vec![&detailed, &plain],
                "unnamed",
            ),
            (
                "422 twice",
                true,
                vec![
                    Some((422, json!({"message": "no"}))),
                    Some((422, json!({}))),
                ],
                vec![&detailed, &plain],
                &format!("not started: POST {path}: GitHub HTTP status 422: message unavailable"),
            ),
            (
                "422 without asking for run details",
                false,
                vec![Some((422, json!({"message": "no"})))],
                vec![&plain],
                &format!("not started: POST {path}: GitHub HTTP status 422: no"),
            ),
            (
                "refused",
                true,
                vec![Some((404, json!({"message": "Not Found"})))],
                vec![&detailed],
                &format!("not started: POST {path}: GitHub HTTP status 404: Not Found"),
            ),
            (
                "server error",
                true,
                vec![Some((500, json!({"message": "boom"})))],
                vec![&detailed],
                &format!("ambiguous: POST {path}: GitHub HTTP status 500: boom"),
            ),
            (
                "lost answer",
                true,
                vec![None],
                vec![&detailed],
                "ambiguous: connection reset",
            ),
            (
                "no handle without run details",
                false,
                vec![Some((204, Value::Null))],
                vec![&plain],
                "unnamed",
            ),
        ];
        for (name, run_details, answers, sent, expected) in cases {
            let github = GitHub {
                target: &target,
                run_details: *run_details,
            };
            let mut answers = answers.iter();
            let mut bodies = vec![];
            let outcome = github.start(
                &start,
                || Ok(()),
                |p, body| {
                    assert_eq!(p, path, "{name}");
                    bodies.push(body.clone());
                    match answers.next().unwrap() {
                        Some(answer) => Ok(answer.clone()),
                        None => bail!("connection reset"),
                    }
                },
            );
            let got = match outcome {
                StartOutcome::Started(h) => format!("started {} {}", h.id, h.url),
                StartOutcome::Unnamed => "unnamed".into(),
                StartOutcome::NotStarted(e) => format!("not started: {e:#}"),
                StartOutcome::Ambiguous(e) => format!("ambiguous: {e:#}"),
            };
            assert_eq!(got, *expected, "{name}");
            assert_eq!(bodies.iter().collect::<Vec<_>>(), *sent, "{name}");
        }
    }

    #[test]
    fn github_start_admits_each_request() {
        let target = Target {
            caller: "example/tracker".into(),
            workflow: "dispatch.yml".into(),
            git_ref: "main".into(),
        };
        let start = Start {
            repository: "example/app",
            item: 7,
            kind: "triage",
            brief: "Look.",
            pool: Some("small"),
            runner: None,
            base: Some("stack/lower"),
        };
        let github = GitHub {
            target: &target,
            run_details: true,
        };
        assert_eq!(
            github.body(&start)["inputs"],
            json!({"task": "Look.", "repo": "example/app", "item": "7", "kind": "triage", "pool": "small", "base": "stack/lower"})
        );
        // One request is admitted: the retry after a 422 is not, and nothing
        // more is sent.
        let mut budget = 1;
        let mut sent = 0;
        let outcome = github.start(
            &start,
            || {
                ensure!(budget > 0, "budget spent");
                budget -= 1;
                Ok(())
            },
            |_, _| {
                sent += 1;
                Ok((422, json!({})))
            },
        );
        assert!(
            matches!(&outcome, StartOutcome::NotStarted(e) if e.to_string() == "budget spent"),
            "{outcome:?}"
        );
        assert_eq!(sent, 1);
    }
}
