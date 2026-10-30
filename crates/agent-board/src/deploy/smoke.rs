//! The applied stage deployment smoke path: a fresh scratch repository and
//! project, one harmless task through an in-process fake engine, and the
//! pass's derived Status. Only identities that creation returned, matching
//! what was requested, authorize cleanup; everything else is reported.
use super::Config;
use crate::live::Borrowed;
use anyhow::{Context, Result, bail, ensure};
use board_core::{Capability, Intent, Item, ItemRef, Policy, Snapshot, Status};
use board_forge::{
    Forge,
    dispatch::{self, Start, StartOutcome, Target},
    github::{API_ROOT, DEFAULT_HOST, GitHub, Transport},
    poll::Rest,
};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::fmt;
use tokio::runtime::Runtime;

/// The smoke steps' writes: repository, project, issue, item add, Todo,
/// result comment, close and Done. Cleanup's deletions, one per receipt,
/// come on top so that a capped run can still clean up.
pub(crate) const SMOKE_WRITES: usize = 8;

/// Where the fake engine is told the job would run; it sends nothing.
const DISPATCH_REF: &str = "main";

/// The run the fake engine names; no workflow runs.
const FAKE_RUN_ID: u64 = 1;

const FAKE_KIND: &str = "research";

const FAKE_TASK: &str = "Harmless stage deploy smoke task: reply once, then close as completed.";

const ORGANIZATION: &str = "query($owner:String!){organization(login:$owner){id login}}";

const CREATE_REPOSITORY: &str = "mutation($owner:ID!,$name:String!){createRepository(input:{ownerId:$owner,name:$name,visibility:PRIVATE,hasIssuesEnabled:true}){repository{id nameWithOwner}}}";

const CREATE_PROJECT: &str = "mutation($owner:ID!,$repository:ID!,$title:String!){createProjectV2(input:{ownerId:$owner,repositoryId:$repository,title:$title}){projectV2{id number title owner{... on Organization{login}}}}}";

const DELETE_PROJECT: &str =
    "mutation($id:ID!){deleteProjectV2(input:{projectId:$id}){projectV2{id}}}";

const CREATE_ISSUE: &str = "mutation($repository:ID!,$title:String!){createIssue(input:{repositoryId:$repository,title:$title,body:\"Disposable agent-board stage deploy smoke task\"}){issue{id number repository{nameWithOwner}}}}";

const COMMENT: &str = "mutation($issue:ID!,$body:String!){addComment(input:{subjectId:$issue,body:$body}){commentEdge{node{id}}}}";

const CLOSE: &str =
    "mutation($id:ID!){closeIssue(input:{issueId:$id,stateReason:COMPLETED}){issue{id state}}}";

const RESULT: &str = "query($id:ID!){node(id:$id){... on Issue{id state stateReason comments(first:2){totalCount nodes{id body}}}}}";

const REPOSITORY_NODE: &str = "query($id:ID!){node(id:$id){... on Repository{id nameWithOwner}}}";

/// The deadline passed before a step: no further step starts, cleanup runs.
#[derive(Debug)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("stage deploy smoke cancelled; no further steps started")
    }
}

impl std::error::Error for Cancelled {}

/// Counts every mutation and refuses those past [`SMOKE_WRITES`].
struct Counting<'a, T> {
    inner: &'a T,
    writes: Cell<usize>,
}

impl<T: Transport> Transport for Counting<'_, T> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        if query.trim_start().starts_with("mutation") {
            ensure!(
                self.writes.get() < SMOKE_WRITES,
                "smoke write cap of {SMOKE_WRITES} reached"
            );
            self.writes.set(self.writes.get() + 1);
        }
        self.inner.graphql(query, variables)
    }
}

fn string(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

struct Run<'a, T> {
    runtime: &'a Runtime,
    config: &'a Config,
    t: &'a T,
    counting: Counting<'a, T>,
    nonce: &'a str,
    steps: Vec<Value>,
    /// Authenticated creation receipts: (node ID, nameWithOwner).
    repository: Option<(String, String)>,
    /// (node ID, number).
    project: Option<(String, u64)>,
    issue: Option<(String, u64)>,
    receipts: serde_json::Map<String, Value>,
    leftovers: Vec<Value>,
}

impl<T: Transport + board_forge::github::GraphQl> Run<'_, T> {
    fn step<R>(
        &mut self,
        cancelled: &mut dyn FnMut() -> bool,
        name: &str,
        f: impl FnOnce(&mut Self) -> Result<R>,
    ) -> Result<R> {
        if cancelled() {
            self.steps.push(json!({"step":name,"state":"cancelled"}));
            return Err(Cancelled.into());
        }
        let result = f(self);
        if let Err(error) = &result {
            // Like the preflight, keep transport errors out of the report.
            eprintln!("stage deploy smoke {name}: {error:#}");
        }
        let state = if result.is_ok() { "ok" } else { "failed" };
        self.steps.push(json!({"step":name,"state":state}));
        result
    }

    /// A creation's returned identity that is not the one requested is never
    /// cleaned up: it is reported, so a confused or hostile answer cannot
    /// direct a deletion outside the scratch objects this run made.
    fn leftover(&mut self, resource: &str, answer: &Value, reason: &str) {
        self.leftovers
            .push(json!({"resource":resource,"returned":answer,"reason":reason}));
    }

    /// A creation whose answer was lost may still have happened: report the
    /// name to look for, and never delete by it.
    fn create(&mut self, resource: &str, query: &str, variables: Value) -> Result<Value> {
        let result = self.counting.graphql(query, variables);
        if result.is_err() {
            self.leftovers.push(json!({"resource":resource,"name":self.nonce,"reason":"creation outcome unknown; look for this name in the staging owner; not cleaned up"}));
        }
        result
    }

    fn board(&self) -> Result<GitHub<Borrowed<'_, Counting<'_, T>>>> {
        let (_, number) = self.project.as_ref().context("missing project receipt")?;
        Ok(GitHub {
            transport: Borrowed(&self.counting),
            owner: self.config.owner.clone(),
            number: *number,
        })
    }

    fn identity(&self) -> Result<ItemRef> {
        let (_, repository) = self.repository.as_ref().context("missing receipt")?;
        let (_, number) = self.issue.as_ref().context("missing issue receipt")?;
        Ok(ItemRef {
            host: DEFAULT_HOST.into(),
            repository: repository.clone(),
            number: *number,
        })
    }

    fn set_status(&self, status: &str, mode: crate::item::Mode) -> Result<()> {
        let mut added = None;
        crate::item::write(
            &self.board()?,
            &self.identity()?,
            &[format!("Status={status}")],
            mode,
            true,
            &mut added,
            &mut std::io::sink(),
        )
    }

    /// The pass's view: a complete snapshot holding exactly the scratch item.
    fn snapshot(&self) -> Result<(Snapshot, Item)> {
        let (_, number) = self.project.as_ref().context("missing project receipt")?;
        let github = GitHub {
            transport: Borrowed(self.t),
            owner: self.config.owner.clone(),
            number: *number,
        };
        let snapshot = self.runtime.block_on(github.snapshot())?;
        ensure!(snapshot.coverage.complete, "incomplete scratch snapshot");
        ensure!(
            snapshot.project.as_str()
                == format!(
                    "https://github.com/orgs/{}/projects/{number}",
                    self.config.owner
                ),
            "scratch project readback mismatch"
        );
        let identity = self.identity()?;
        ensure!(
            snapshot.items.len() == 1 && snapshot.items[0].identity == identity,
            "scratch project must hold exactly the scratch issue"
        );
        let item = snapshot.items[0].clone();
        ensure!(
            item.title == self.nonce && item.content_kind == "issue" && item.fields_complete,
            "scratch issue readback mismatch"
        );
        Ok((snapshot, item))
    }

    /// The pass's Status derivation for the scratch item, nothing else.
    fn plan(&self, snapshot: &Snapshot) -> Result<Vec<(String, Intent)>> {
        let policy = Policy {
            project: snapshot.project.clone(),
            repositories: vec![self.identity()?.repository],
            capabilities: vec![Capability::ProjectFieldEdit],
        };
        let plan = board_core::reconcile(snapshot, &policy, &snapshot.clock)?;
        ensure!(plan.diagnostics.is_empty(), "unexpected pass diagnostics");
        Ok(plan
            .actions
            .into_iter()
            .map(|a| (a.reason, a.intent))
            .collect())
    }

    fn steps(&mut self, cancelled: &mut dyn FnMut() -> bool) -> Result<()> {
        let owner = self.step(cancelled, "scope", |run| {
            let data = run
                .counting
                .graphql(ORGANIZATION, json!({"owner":run.config.owner}))?;
            ensure!(
                data["organization"]["login"] == run.config.owner,
                "staging organization identity mismatch"
            );
            string(&data, "/organization/id").context("missing organization ID")
        })?;
        self.step(cancelled, "repository", |run| {
            let name = format!("{}/{}", run.config.owner, run.nonce);
            let data = run.create(
                "repository",
                CREATE_REPOSITORY,
                json!({"owner":owner,"name":run.nonce}),
            )?;
            let id = string(&data, "/createRepository/repository/id");
            match id {
                Some(id) if data["createRepository"]["repository"]["nameWithOwner"] == name => {
                    eprintln!("created repository {name} ({id}); cleanup deletes it by this ID");
                    run.receipts
                        .insert("repository".into(), json!({"id":id,"nameWithOwner":name}));
                    run.repository = Some((id, name));
                    Ok(())
                }
                _ => {
                    run.leftover(
                        "repository",
                        &data,
                        "returned identity is not the requested staging repository; not cleaned up",
                    );
                    bail!("wrong-owner or ambiguous repository creation for {name}")
                }
            }
        })?;
        self.step(cancelled, "project", |run| {
            let (repository, _) = run.repository.clone().context("missing receipt")?;
            let data = run.create(
                "project",
                CREATE_PROJECT,
                json!({"owner":owner,"repository":repository,"title":run.nonce}),
            )?;
            let project = &data["createProjectV2"]["projectV2"];
            let id = string(&data, "/createProjectV2/projectV2/id");
            let number = project["number"]
                .as_u64()
                .filter(|n| (1..=i32::MAX as u64).contains(n));
            match (id, number) {
                (Some(id), Some(number))
                    if project["owner"]["login"] == run.config.owner
                        && project["title"] == run.nonce =>
                {
                    eprintln!("created project {number} ({id}); cleanup deletes it by this ID");
                    run.receipts
                        .insert("project".into(), json!({"id":id,"number":number}));
                    run.project = Some((id, number));
                    Ok(())
                }
                _ => {
                    run.leftover(
                        "project",
                        &data,
                        "returned identity is not a project of the staging owner; not cleaned up",
                    );
                    bail!("wrong-owner or ambiguous project creation")
                }
            }
        })?;
        self.step(cancelled, "issue", |run| {
            let (repository, name) = run.repository.clone().context("missing receipt")?;
            let data = run.counting.graphql(
                CREATE_ISSUE,
                json!({"repository":repository,"title":run.nonce}),
            )?;
            let issue = &data["createIssue"]["issue"];
            let id = string(&data, "/createIssue/issue/id").context("missing issue ID")?;
            let number = issue["number"].as_u64().context("missing issue number")?;
            ensure!(
                issue["repository"]["nameWithOwner"] == name,
                "issue created outside the scratch repository"
            );
            eprintln!("created issue {name}#{number} ({id}); deleted with its repository");
            run.receipts
                .insert("issue".into(), json!({"id":id,"number":number}));
            run.issue = Some((id, number));
            Ok(())
        })?;
        self.step(cancelled, "item", |run| {
            run.set_status("Todo", crate::item::Mode::Add)
        })?;
        self.step(cancelled, "pass", |run| {
            let (snapshot, item) = run.snapshot()?;
            ensure!(
                item.state == "open" && item.status == Some(Status::Todo),
                "scratch item is not an open Todo"
            );
            ensure!(
                run.plan(&snapshot)?.is_empty(),
                "pass proposed changes before dispatch"
            );
            Ok(())
        })?;
        let handle = self.step(cancelled, "dispatch", |run| {
            let target = Target {
                caller: run.config.repository.clone(),
                workflow: run.config.workflow.clone(),
                git_ref: DISPATCH_REF.into(),
            };
            target.validate()?;
            let identity = run.identity()?;
            let start = Start {
                repository: &identity.repository,
                item: identity.number,
                kind: FAKE_KIND,
                brief: FAKE_TASK,
                pool: None,
                runner: None,
                base: None,
            };
            let backend = dispatch::GitHub {
                target: &target,
                run_details: true,
            };
            let expected = backend.path();
            // The fake engine: the exact request a real dispatch would send,
            // answered in process. Nothing leaves the machine and no model runs.
            let request = RefCell::new(None);
            let outcome = backend.start(
                &start,
                || Ok(()),
                |path, body| {
                    *request.borrow_mut() = Some((path.to_owned(), body.clone()));
                    Ok((200, json!({"workflow_run_id":FAKE_RUN_ID})))
                },
            );
            let StartOutcome::Started(handle) = outcome else {
                bail!("fake engine did not start: {outcome:?}");
            };
            let (path, body) = request.into_inner().context("no dispatch request")?;
            let item = identity.number.to_string();
            ensure!(
                path == expected
                    && handle.id == FAKE_RUN_ID.to_string()
                    && body["ref"] == DISPATCH_REF
                    && body["inputs"]["repo"] == identity.repository.as_str()
                    && body["inputs"]["item"] == item.as_str()
                    && body["inputs"]["kind"] == FAKE_KIND,
                "dispatch request mismatch"
            );
            run.receipts
                .insert("dispatch".into(), json!({"id":handle.id,"engine":"fake"}));
            Ok(handle)
        })?;
        self.step(cancelled, "result", |run| {
            let (issue, _) = run.issue.clone().context("missing issue receipt")?;
            let body = format!(
                "agent-board fake-engine result: run {}, task {}",
                handle.id, run.nonce
            );
            let data = run
                .counting
                .graphql(COMMENT, json!({"issue":issue,"body":body}))?;
            let comment =
                string(&data, "/addComment/commentEdge/node/id").context("missing comment ID")?;
            let data = run.counting.graphql(CLOSE, json!({"id":issue}))?;
            ensure!(
                data["closeIssue"]["issue"]["id"] == issue.as_str()
                    && data["closeIssue"]["issue"]["state"] == "CLOSED",
                "close receipt mismatch"
            );
            let data = run.counting.graphql(RESULT, json!({"id":issue}))?;
            let node = &data["node"];
            ensure!(
                node["id"] == issue.as_str()
                    && node["state"] == "CLOSED"
                    && node["stateReason"] == "COMPLETED"
                    && node["comments"]["totalCount"] == 1
                    && node["comments"]["nodes"][0]["id"] == comment.as_str()
                    && node["comments"]["nodes"][0]["body"] == body.as_str(),
                "fake-engine result readback mismatch"
            );
            run.receipts.insert("comment".into(), json!({"id":comment}));
            Ok(())
        })?;
        self.step(cancelled, "status", |run| {
            let (snapshot, _) = run.snapshot()?;
            let plan = run.plan(&snapshot)?;
            ensure!(
                matches!(
                    plan.as_slice(),
                    [(reason, Intent::SetStatus { value: Status::Done, .. })]
                        if reason == "closed_issue_not_done"
                ),
                "pass did not derive Done for the completed task"
            );
            run.set_status("Done", crate::item::Mode::Edit)?;
            let (_, item) = run.snapshot()?;
            ensure!(
                item.state == "closed" && item.status == Some(Status::Done),
                "derived Status readback mismatch"
            );
            Ok(())
        })
    }

    /// Every cleanup is attempted; each acts only on its creation receipt.
    fn cleanup(&mut self, rest: &impl Rest) -> (Vec<Value>, usize) {
        let mut cleanup = Vec::new();
        let mut writes = 0;
        if let Some((id, number)) = self.project.clone() {
            writes += 1;
            let result =
                Transport::graphql(self.t, DELETE_PROJECT, json!({"id":id})).and_then(|v| {
                    ensure!(
                        v["deleteProjectV2"]["projectV2"]["id"] == id.as_str(),
                        "project cleanup receipt mismatch"
                    );
                    Ok(())
                });
            self.cleaned(
                &mut cleanup,
                "project",
                json!({"id":id,"number":number}),
                result,
            );
        }
        if let Some((id, name)) = self.repository.clone() {
            // The name is what REST deletes by: read it back from the receipt's
            // node ID first, so a rename or reuse cannot redirect the deletion.
            let result =
                Transport::graphql(self.t, REPOSITORY_NODE, json!({"id":id})).and_then(|v| {
                    ensure!(
                        v["node"]["id"] == id.as_str()
                            && v["node"]["nameWithOwner"] == name.as_str(),
                        "repository identity changed since creation; not deleted"
                    );
                    writes += 1;
                    let (status, _) =
                        rest.send("DELETE", &format!("{API_ROOT}repos/{name}"), &Value::Null)?;
                    ensure!(status == 204, "repository deletion answered {status}");
                    Ok(())
                });
            self.cleaned(
                &mut cleanup,
                "repository",
                json!({"id":id,"nameWithOwner":name}),
                result,
            );
        }
        (cleanup, writes)
    }

    fn cleaned(&mut self, cleanup: &mut Vec<Value>, resource: &str, id: Value, result: Result<()>) {
        let state = match &result {
            Ok(()) => "deleted",
            Err(error) => {
                eprintln!("stage deploy smoke cleanup {resource}: {error:#}");
                self.leftovers.push(json!({"resource":resource,"receipt":id,"reason":"cleanup failed; delete manually by this ID"}));
                "failed"
            }
        };
        cleanup.push(json!({"resource":resource,"receipt":id,"state":state}));
    }
}

/// Run the smoke path and clean up; the report section, its writes and the outcome.
pub(crate) fn run(
    runtime: &Runtime,
    config: &Config,
    t: &(impl Transport + board_forge::github::GraphQl),
    rest: &impl Rest,
    nonce: &str,
    cancelled: &mut dyn FnMut() -> bool,
) -> (Value, usize, Result<()>) {
    if let Err(error) = config.validate() {
        return (Value::Null, 0, Err(error));
    }
    let mut run = Run {
        runtime,
        config,
        t,
        counting: Counting {
            inner: t,
            writes: Cell::new(0),
        },
        nonce,
        steps: Vec::new(),
        repository: None,
        project: None,
        issue: None,
        receipts: serde_json::Map::new(),
        leftovers: Vec::new(),
    };
    let result = run.steps(cancelled);
    let (cleanup, deletions) = run.cleanup(rest);
    let writes = run.counting.writes.get() + deletions;
    let result = match result {
        Err(error) => Err(error),
        Ok(()) if !run.leftovers.is_empty() => Err(anyhow::anyhow!(
            "stage deploy smoke passed but left resources behind"
        )),
        Ok(()) => Ok(()),
    };
    let report = json!({
        "nonce": nonce,
        "steps": run.steps,
        "receipts": run.receipts,
        "cleanup": cleanup,
        "leftovers": run.leftovers,
    });
    (report, writes, result)
}

#[cfg(test)]
mod tests;
