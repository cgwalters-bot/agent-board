//! Opt-in staging fixture. Only locally held creation IDs authorize cleanup.
use anyhow::{Context, Result, bail, ensure};
use board_core::{Capability, ItemRef, Policy, Status};
use board_forge::{
    Forge,
    github::{
        AuthenticationRequired, Client, DEFAULT_HOST, DiagnosticTransport, GitHub, Transport,
    },
};
use clap::Args;
use serde_json::{Value, json};

const DELETE: &str =
    "mutation($id:ID!){deleteProjectV2(input:{projectId:$id}){deletedProjectV2Id}}";
const CLOSE: &str = "mutation($id:ID!){closeIssue(input:{issueId:$id}){issue{id state}}}";
const REMOVE: &str = "mutation($project:ID!,$item:ID!){deleteProjectV2Item(input:{projectId:$project,itemId:$item}){deletedItemId}}";

#[derive(Args)]
pub struct Options {
    /// Create and delete a throwaway project instead of using --project.
    #[arg(long, conflicts_with = "project")]
    create_project: bool,
    /// Sweeping remains disabled: names and descriptions cannot authorize deletion.
    #[arg(long)]
    sweep: bool,
}

pub fn error_exit_code(error: &anyhow::Error) -> i32 {
    if error.downcast_ref::<AuthenticationRequired>().is_some() {
        4
    } else if error
        .downcast_ref::<board_forge::epic_apply::PlanChanged>()
        .is_some()
    {
        3
    } else {
        1
    }
}

pub fn run(
    options: &Options,
    owner: Option<&str>,
    repo: Option<&str>,
    project: Option<u64>,
) -> Result<()> {
    ensure!(
        !options.sweep,
        "unsupported_capability: sweeping requires independently trusted creation receipts; use manual recovery"
    );
    let owner = owner.context("test project requires --owner")?;
    let repo = repo.context("test project requires --repo OWNER/REPO")?;
    ensure!(
        options.create_project || project.is_some(),
        "test project requires --project NUMBER or --create-project"
    );
    ensure!(
        project.is_none_or(|n| n > 0),
        "project number must be positive"
    );
    ensure!(
        owner == "cgwalters-forge-stage",
        "live fixture owner must be cgwalters-forge-stage"
    );
    let (repo_owner, name) = repo.split_once('/').context("expected OWNER/REPO")?;
    ensure!(
        repo_owner == owner && !name.is_empty() && !name.contains('/'),
        "scratch repository must belong to the staging owner"
    );
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("generate fixture nonce"))?;
    let title = format!(
        "agent-board-ci-{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    lifecycle(&Client::from_env()?, owner, repo, &title, project)
}

struct Borrowed<'a, T>(&'a T);

impl<T: Transport> Transport for Borrowed<'_, T> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.0.graphql(query, variables)
    }
}

fn string(value: &Value, pointer: &str) -> Result<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("missing fixture response {pointer}; mutation may be ambiguous"))
}

fn lifecycle(
    t: &impl Transport,
    owner: &str,
    repo: &str,
    title: &str,
    kept: Option<u64>,
) -> Result<()> {
    let (_, name) = repo.split_once('/').context("expected OWNER/REPO")?;
    let diagnostic = DiagnosticTransport {
        transport: t,
        project: kept.map_or_else(|| format!("{owner}/new"), |n| format!("{owner}/{n}")),
        repository: repo.into(),
    };
    let t = &diagnostic;
    let project_read = kept.map(|number| {
        let result = t.graphql("query($owner:String!,$number:Int!){organization(login:$owner){projectV2(number:$number){id number viewerCanUpdate}}}", json!({"owner":owner,"number":number})).and_then(|data| {
            string(&data, "/organization/projectV2/id")
                .with_context(|| format!("reading project {owner}/{number}"))?;
            // Read access alone caused the scheduled fixture to create and close
            // scratch issues every day before its project-add mutation failed.
            // This is an early diagnostic, not authorization for later writes.
            ensure!(
                data["organization"]["projectV2"]["viewerCanUpdate"] == true,
                "project {owner}/{number} is not writable (viewerCanUpdate is false or missing); in {repo}, update environment Main secret PAT: fine-grained token resource owner {owner}, organization Projects Read and write, repository Issues Read and write (plus Metadata read); ensure organization approval and the token owner's project write access"
            );
            Ok(data)
        });
        eprintln!("reading project {owner}/{number}: {}", match &result { Ok(_) => "ok".into(), Err(e) => format!("refused: {e:#}") });
        result
    });
    let scope = t.graphql("query($owner:String!,$repo:String!){organization(login:$owner){id} repository(owner:$owner,name:$repo){id nameWithOwner}}", json!({"owner":owner,"repo":name})).and_then(|data| {
        string(&data, "/repository/id")
            .with_context(|| format!("reading repository {repo}"))?;
        ensure!(data["repository"]["nameWithOwner"].as_str() == Some(repo), "reading repository {repo}: scratch repository identity mismatch");
        Ok(data)
    });
    eprintln!(
        "reading repository {repo}: {}",
        match &scope {
            Ok(_) => "ok".into(),
            Err(e) => format!("refused: {e:#}"),
        }
    );
    if project_read.as_ref().is_some_and(Result::is_err) || scope.is_err() {
        let project_report = match &project_read {
            Some(Err(error)) => format!("{error:#}"),
            Some(Ok(_)) => "ok".into(),
            None => "not applicable: creating a throwaway project".into(),
        };
        let repository_report = match &scope {
            Err(error) => format!("{error:#}"),
            Ok(_) => "ok".into(),
        };
        let error = project_read
            .and_then(Result::err)
            .or_else(|| scope.err())
            .context("missing preflight failure")?;
        return Err(error.context(format!("live preflight failed; no writes attempted; project: {project_report}; repository: {repository_report}")));
    }
    let project_read = project_read.transpose()?;
    let scope = scope?;
    ensure!(
        scope["repository"]["nameWithOwner"].as_str() == Some(repo),
        "scratch repository identity mismatch"
    );
    let organization = string(&scope, "/organization/id")?;
    let repository = string(&scope, "/repository/id")?;
    let created = if let Some(data) = project_read {
        json!({"createProjectV2":{"projectV2":data["organization"]["projectV2"]}})
    } else {
        t.graphql("mutation($owner:ID!,$title:String!){createProjectV2(input:{ownerId:$owner,title:$title}){projectV2{id number}}}", json!({"owner":organization,"title":title}))
        ?
    };
    let project_id = string(&created, "/createProjectV2/projectV2/id")?;
    eprintln!(
        "Fixture project {project_id}; throwaway: {}.",
        kept.is_none()
    );
    let mut issue_id = None;
    let mut item_id = None;
    let result = (|| -> Result<()> {
        let number = created["createProjectV2"]["projectV2"]["number"]
            .as_u64()
            .context("missing created project number")?;
        ensure!(
            kept.is_none_or(|expected| number == expected),
            "kept project number mismatch"
        );
        let issue = t.graphql("mutation($repo:ID!,$title:String!){createIssue(input:{repositoryId:$repo,title:$title,body:\"Disposable agent-board live fixture\"}){issue{id number}}}", json!({"repo":repository,"title":title}))?;
        issue_id = Some(string(&issue, "/createIssue/issue/id")?);
        eprintln!(
            "Created scratch issue {}; cleanup will close it.",
            issue_id.as_deref().context("missing local issue ID")?
        );
        let issue_number = issue["createIssue"]["issue"]["number"]
            .as_u64()
            .context("missing created issue number")?;
        let (field_name, value, field_id) = if kept.is_some() {
            ("Status", "Done", None)
        } else {
            let field = t.graphql("mutation($project:ID!){createProjectV2Field(input:{projectId:$project,dataType:TEXT,name:\"Fixture nonce\"}){projectV2Field{... on ProjectV2Field{id}}}}", json!({"project":project_id}))?;
            (
                "Fixture nonce",
                title,
                Some(string(&field, "/createProjectV2Field/projectV2Field/id")?),
            )
        };
        // The same path as `item add --priority`/`item edit --field --apply`: schema,
        // issue lookup, add, then a field set checked against GitHub's receipt.
        let github = GitHub {
            transport: Borrowed(t),
            owner: owner.into(),
            number,
        };
        let identity = ItemRef {
            host: DEFAULT_HOST.into(),
            repository: repo.into(),
            number: issue_number,
        };
        crate::item::write(
            &github,
            &identity,
            &[format!("{field_name}={value}")],
            crate::item::Mode::Add,
            true,
            &mut item_id,
            &mut std::io::sink(),
        )?;
        let snapshot = GitHub {
            transport: Borrowed(t),
            owner: owner.into(),
            number,
        }
        .snapshot()?;
        ensure!(
            snapshot.coverage.complete,
            "live snapshot did not return complete fixture membership"
        );
        ensure!(
            snapshot.project.as_str()
                == format!("https://github.com/orgs/{owner}/projects/{number}"),
            "live project readback mismatch"
        );
        let matches: Vec<_> = snapshot
            .items
            .iter()
            .filter(|item| item.identity.repository == repo && item.identity.number == issue_number)
            .collect();
        ensure!(
            matches.len() == 1,
            "live snapshot must contain exactly one scratch issue"
        );
        let item = matches[0];
        ensure!(
            item.identity.repository == repo
                && item.identity.number == issue_number
                && item.title == title
                && item.state == "open"
                && item.fields_complete
                && item.content_kind == "issue",
            "live issue readback mismatch"
        );
        ensure!(
            if kept.is_some() {
                item.status == Some(Status::Done)
            } else {
                item.fields.get(field_name).and_then(|v| v["text"].as_str()) == Some(title)
            },
            "live field readback mismatch"
        );
        ensure!(
            field_id.is_none_or(|id| snapshot
                .fields
                .iter()
                .any(|f| f["id"].as_str() == Some(&id))),
            "live schema readback mismatch"
        );
        ensure!(
            item.labels.is_empty()
                && item.assignees.is_empty()
                && item.timestamps.contains_key("createdAt")
                && item.timestamps.contains_key("updatedAt"),
            "live issue facts mismatch"
        );
        let policy = Policy {
            project: snapshot.project.clone(),
            repositories: vec![repo.into()],
            capabilities: vec![Capability::ProjectFieldEdit, Capability::CloseIssue],
        };
        let plan = board_core::reconcile(&snapshot, &policy, &snapshot.clock)?;
        let actions: Vec<_> = plan
            .actions
            .iter()
            .filter(|action| match &action.intent {
                board_core::Intent::SetStatus { item: target, .. }
                | board_core::Intent::CloseIssue { item: target, .. } => target == &item.identity,
            })
            .collect();
        ensure!(
            if kept.is_some() || !snapshot.has_status_option("Triage") {
                actions.is_empty() && plan.diagnostics.contains_key(&item.identity.display())
            } else {
                actions.len() == 1
                    && matches!(
                        actions[0].intent,
                        board_core::Intent::SetStatus {
                            value: Status::Triage,
                            ..
                        }
                    )
            },
            "live reconcile plan mismatch"
        );
        Ok(())
    })();
    // Attempt every cleanup even if another fails. Only returned creation IDs
    // authorize item removal and issue closure; never act on a title or marker.
    let remove = if let Some(id) = &item_id {
        t.graphql(REMOVE, json!({"project":project_id,"item":id}))
            .and_then(|v| {
                ensure!(
                    v["deleteProjectV2Item"]["deletedItemId"].as_str() == Some(id),
                    "item cleanup receipt mismatch"
                );
                Ok(())
            })
    } else {
        Ok(())
    };
    let close = if let Some(id) = &issue_id {
        t.graphql(CLOSE, json!({"id":id})).and_then(|v| {
            ensure!(
                v["closeIssue"]["issue"]["id"].as_str() == Some(id)
                    && v["closeIssue"]["issue"]["state"] == "CLOSED",
                "issue cleanup receipt mismatch"
            );
            Ok(())
        })
    } else {
        Ok(())
    };
    let delete = if kept.is_none() {
        t.graphql(DELETE, json!({"id":project_id})).and_then(|v| {
            ensure!(
                v["deleteProjectV2"]["deletedProjectV2Id"].as_str() == Some(&project_id),
                "project cleanup receipt mismatch"
            );
            Ok(())
        })
    } else {
        Ok(())
    };
    if close.is_err() || delete.is_err() || remove.is_err() {
        bail!(
            "fixture cleanup failed; manual recovery: project {project_id}, issue {}, item {}; issue cleanup: {}; project cleanup: {}; fixture: {}; item cleanup: {}",
            issue_id.as_deref().unwrap_or("not created or ambiguous"),
            item_id.as_deref().unwrap_or("not created or ambiguous"),
            close
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_else(|| "ok".into()),
            delete
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_else(|| "ok".into()),
            result
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_else(|| "ok".into()),
            remove
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_else(|| "ok".into())
        );
    }
    result.context("live fixture failed; cleanup succeeded")?;
    println!(
        "Fixture schema, membership, issue facts, field, reconcile plan and cleanup verified."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Recorded {
        calls: RefCell<Vec<String>>,
        fail: usize,
    }

    impl Transport for Recorded {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            let mut calls = self.calls.borrow_mut();
            calls.push(query.into());
            if calls.len() == self.fail {
                bail!("GitHub GraphQL: FORBIDDEN: recorded refusal");
            }
            if query == DELETE {
                assert_eq!(variables["id"], "P_created");
                return Ok(json!({"deleteProjectV2":{"deletedProjectV2Id":"P_created"}}));
            }
            if query == CLOSE {
                assert_eq!(variables["id"], "I_created");
                return Ok(json!({"closeIssue":{"issue":{"id":"I_created","state":"CLOSED"}}}));
            }
            if query == REMOVE {
                assert_eq!(variables["project"], "P_created");
                assert_eq!(variables["item"], "M_created");
                return Ok(json!({"deleteProjectV2Item":{"deletedItemId":"M_created"}}));
            }
            if query.contains("projectV2(number") && query.contains("organization(login") {
                assert_eq!(variables["number"], 2);
                return Ok(
                    json!({"organization":{"projectV2":{"id":"P_created","number":2,"viewerCanUpdate":true}}}),
                );
            }
            if query.contains("organization(login") {
                return Ok(
                    json!({"organization":{"id":"O_stage"},"repository":{"id":"R_scratch","nameWithOwner":"cgwalters-forge-stage/board-test"}}),
                );
            }
            if query.contains("createProjectV2(") {
                return Ok(json!({"createProjectV2":{"projectV2":{"id":"P_created","number":1}}}));
            }
            if query.contains("createIssue(") {
                return Ok(json!({"createIssue":{"issue":{"id":"I_created","number":1}}}));
            }
            if query.contains("addProjectV2ItemById(") {
                assert_eq!(variables["project"], "P_created");
                assert_eq!(variables["issue"], "I_created");
                return Ok(json!({"addProjectV2ItemById":{"item":{"id":"M_created"}}}));
            }
            if query.contains("createProjectV2Field(") {
                return Ok(json!({"createProjectV2Field":{"projectV2Field":{"id":"F_created"}}}));
            }
            if query.contains("updateProjectV2ItemFieldValue(") {
                assert_eq!(variables["project"], "P_created");
                assert_eq!(variables["item"], "M_created");
                let value = &variables["value"];
                return Ok(
                    json!({"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"M_created","fieldValueByName":{"optionId":value["singleSelectOptionId"],"text":value["text"]}}}}),
                );
            }
            if query.contains("issue(number:") {
                return Ok(
                    json!({"repository":{"issue":{"id":"I_created","number":1,"repository":{"nameWithOwner":"cgwalters-forge-stage/board-test"},"projectItems":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}),
                );
            }
            if query.contains("repositoryOwner(") {
                return Ok(
                    json!({"repositoryOwner":{"projectV2":{"id":"P_created","url":format!("https://github.com/orgs/cgwalters-forge-stage/projects/{}", variables["number"])}}}),
                );
            }
            let kept = calls
                .iter()
                .any(|q| q.contains("projectV2(number") && q.contains("organization(login"));
            let (key, nodes) = if query.contains("fields(first") {
                (
                    "fields",
                    json!([{"id":"F_created","name":"Fixture nonce","dataType":"TEXT"},{"id":"F_status","name":"Status","dataType":"SINGLE_SELECT","options":[{"id":"todo","name":"Todo"},{"id":"done","name":"Done"}]}]),
                )
            } else if query.contains("items(first") {
                (
                    "items",
                    json!([{"id":"M_created","content":{"__typename":"Issue","id":"I_created","number":1,"title":"nonce","state":"OPEN","createdAt":"2026-10-08T00:00:00Z","updatedAt":"2026-10-08T00:00:00Z","closedAt":null,"repository":{"nameWithOwner":"cgwalters-forge-stage/board-test"}}}]),
                )
            } else if query.contains("fieldValues(first") {
                (
                    "fieldValues",
                    if kept {
                        json!([{"__typename":"ProjectV2ItemFieldSingleSelectValue","name":"Done","optionId":"done","field":{"name":"Status"}}])
                    } else {
                        json!([{"__typename":"ProjectV2ItemFieldTextValue","text":"nonce","field":{"name":"Fixture nonce"}}])
                    },
                )
            } else if query.contains("labels(first") {
                ("labels", json!([]))
            } else {
                ("assignees", json!([]))
            };
            Ok(
                json!({"node":{key:{"nodes":nodes,"pageInfo":{"hasNextPage":false,"endCursor":null}}}}),
            )
        }
    }

    #[test]
    fn lifecycle_failure_table() {
        for kept in [None, Some(2)] {
            for fail in 0..=19 {
                let t = Recorded {
                    calls: RefCell::new(vec![]),
                    fail,
                };
                let result = lifecycle(
                    &t,
                    "cgwalters-forge-stage",
                    "cgwalters-forge-stage/board-test",
                    "nonce",
                    kept,
                );
                // Kept: project read, scope, issue, schema (2), lookup, add, set, snapshot (6),
                // remove, close. Throwaway: scope, project, issue, field, then as kept, delete.
                let (total, add) = if kept.is_some() { (16, 7) } else { (18, 8) };
                assert_eq!(
                    result.is_ok(),
                    fail == 0 || fail > total,
                    "failure at {fail}, kept {kept:?}: {result:?}"
                );
                let calls = t.calls.borrow();
                assert_eq!(
                    calls.iter().any(|q| q == DELETE),
                    kept.is_none() && fail != 1 && fail != 2
                );
                assert_eq!(
                    calls.iter().any(|q| q.contains("createProjectV2(")),
                    kept.is_none() && fail != 1
                );
                assert!(
                    !calls
                        .iter()
                        .any(|q| kept.is_some() && q.contains("createProjectV2Field("))
                );
                if fail == 0 || fail >= 4 {
                    assert!(calls.iter().any(|q| q == CLOSE));
                }
                // Only an item ID returned by the add authorizes its removal.
                assert_eq!(
                    calls.iter().any(|q| q == REMOVE),
                    fail == 0 || fail > add,
                    "failure at {fail}, kept {kept:?}"
                );
            }
        }
    }

    #[test]
    fn existing_membership_is_neither_added_nor_removed() {
        // Project automation enrolled the scratch issue before the lookup.
        struct Enrolled {
            recorded: Recorded,
            fail_set: bool,
        }

        impl Transport for Enrolled {
            fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
                if self.fail_set && query.contains("updateProjectV2ItemFieldValue(") {
                    self.recorded.calls.borrow_mut().push(query.into());
                    bail!("GitHub GraphQL: FORBIDDEN: recorded refusal");
                }
                let mut response = self.recorded.graphql(query, variables)?;
                if query.contains("issue(number:") {
                    response["repository"]["issue"]["projectItems"]["nodes"] =
                        json!([{"id":"M_created","project":{"id":"P_created"}}]);
                }
                Ok(response)
            }
        }

        for kept in [None, Some(2)] {
            for fail_set in [false, true] {
                let transport = Enrolled {
                    recorded: Recorded {
                        calls: RefCell::new(vec![]),
                        fail: 0,
                    },
                    fail_set,
                };
                let result = lifecycle(
                    &transport,
                    "cgwalters-forge-stage",
                    "cgwalters-forge-stage/board-test",
                    "nonce",
                    kept,
                );
                assert_eq!(result.is_ok(), !fail_set, "{result:?}");
                let calls = transport.recorded.calls.borrow();
                assert!(
                    calls
                        .iter()
                        .any(|q| q.contains("updateProjectV2ItemFieldValue("))
                );
                assert!(!calls.iter().any(|q| q.contains("addProjectV2ItemById(")));
                assert!(!calls.iter().any(|q| q == REMOVE));
                assert!(calls.iter().any(|q| q == CLOSE));
            }
        }
    }

    #[test]
    fn unwritable_kept_project_refuses_before_scratch_creation() {
        struct Unwritable {
            recorded: Recorded,
            permission: Value,
        }

        impl Transport for Unwritable {
            fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
                let mut response = self.recorded.graphql(query, variables)?;
                if query.contains("projectV2(number") {
                    assert!(query.contains("viewerCanUpdate"));
                    response["organization"]["projectV2"]["viewerCanUpdate"] =
                        self.permission.clone();
                }
                Ok(response)
            }
        }

        for permission in [json!(false), Value::Null, json!("true")] {
            let transport = Unwritable {
                recorded: Recorded {
                    calls: RefCell::new(vec![]),
                    fail: 0,
                },
                permission,
            };
            let error = lifecycle(
                &transport,
                "cgwalters-forge-stage",
                "cgwalters-forge-stage/board-test",
                "nonce",
                Some(2),
            )
            .unwrap_err();
            let message = format!("{error:#}");
            for expected in [
                "no writes attempted",
                "project cgwalters-forge-stage/2 is not writable",
                "environment Main secret PAT",
                "organization Projects Read and write",
                "repository Issues Read and write",
            ] {
                assert!(message.contains(expected), "{message}");
            }
            let calls = transport.recorded.calls.borrow();
            assert_eq!(calls.len(), 2);
            assert!(calls.iter().all(|query| query.starts_with("query")));
        }
    }

    #[test]
    fn preflight_attempts_both_reads_without_writes() {
        struct Refused(RefCell<Vec<String>>);

        impl Transport for Refused {
            fn graphql(&self, query: &str, _: Value) -> Result<Value> {
                self.0.borrow_mut().push(query.into());
                bail!("GitHub GraphQL: FORBIDDEN: recorded refusal")
            }
        }

        let transport = Refused(RefCell::new(vec![]));
        let error = lifecycle(
            &transport,
            "cgwalters-forge-stage",
            "cgwalters-forge-stage/board-test",
            "nonce",
            Some(2),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("reading project cgwalters-forge-stage/2"));
        let calls = transport.0.borrow();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].contains("projectV2(number"));
        assert!(calls[1].contains("repository(owner:"));
        assert!(calls.iter().all(|query| query.starts_with("query")));
    }

    #[test]
    fn kept_project_with_other_items_and_readback_mismatches() {
        struct Readback {
            recorded: Recorded,
            corrupt: bool,
        }

        impl Transport for Readback {
            fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
                let mut response = self.recorded.graphql(query, variables)?;
                if query.contains("items(first") {
                    let nodes = response["node"]["items"]["nodes"].as_array_mut().unwrap();
                    let mut other = nodes[0].clone();
                    other["id"] = "M_unrelated".into();
                    other["content"]["id"] = "I_unrelated".into();
                    other["content"]["number"] = 99.into();
                    nodes.push(other);
                    if self.corrupt {
                        nodes[0]["content"]["title"] = "wrong".into();
                    }
                }
                Ok(response)
            }
        }

        for corrupt in [false, true] {
            let transport = Readback {
                recorded: Recorded {
                    calls: RefCell::new(vec![]),
                    fail: 0,
                },
                corrupt,
            };
            let result = lifecycle(
                &transport,
                "cgwalters-forge-stage",
                "cgwalters-forge-stage/board-test",
                "nonce",
                Some(2),
            );
            assert_eq!(result.is_ok(), !corrupt, "{result:?}");
            let calls = transport.recorded.calls.borrow();
            assert!(calls.iter().any(|q| q == REMOVE));
            assert!(calls.iter().any(|q| q == CLOSE));
            assert!(!calls.iter().any(|q| q == DELETE));
        }
    }
}
