use super::*;
use board_forge::github::Blocking;
use board_forge::poll::RestResponse;

const NONCE: &str = "agent-board-smoke-test";
const REPO: &str = "cgwalters-forge-stage/agent-board-smoke-test";
/// Calls on success: scope, repository, project, issue, item (schema 2,
/// lookup, add, set), pass (snapshot 3), result (comment, close, read),
/// status (snapshot 3, schema 2, lookup, set, snapshot 3), cleanup (project,
/// repository read, REST delete).
const CALLS: usize = 28;

/// A staging organization holding what the smoke path creates.
#[derive(Default)]
struct Stage {
    calls: RefCell<Vec<String>>,
    /// Fail the call with this 1-based index; 0 never.
    fail: usize,
    /// "repository" or "project": its creation answers another owner;
    /// "renamed": the repository's name changes before cleanup.
    case: &'static str,
    member: Cell<bool>,
    status: RefCell<Option<String>>,
    closed: Cell<bool>,
    comment: RefCell<Option<String>>,
}

impl Stage {
    fn record(&self, call: String) -> Result<()> {
        let mut calls = self.calls.borrow_mut();
        calls.push(call);
        if calls.len() == self.fail {
            bail!("GitHub GraphQL: FORBIDDEN: recorded refusal");
        }
        Ok(())
    }

    fn writes(&self) -> usize {
        let calls = self.calls.borrow();
        calls
            .iter()
            .filter(|q| q.starts_with("mutation") || q.starts_with("DELETE"))
            .count()
    }

    fn called(&self, query: &str) -> bool {
        self.calls.borrow().iter().any(|q| q == query)
    }
}

impl Transport for Stage {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.record(query.into())?;
        let owner = |case| {
            if self.case == case {
                "production"
            } else {
                "cgwalters-forge-stage"
            }
        };
        if query == ORGANIZATION {
            return Ok(json!({"organization":{"id":"O_stage","login":"cgwalters-forge-stage"}}));
        }
        if query == CREATE_REPOSITORY {
            assert_eq!(variables, json!({"owner":"O_stage","name":NONCE}));
            return Ok(
                json!({"createRepository":{"repository":{"id":"R_scratch","nameWithOwner":format!("{}/{NONCE}", owner("repository"))}}}),
            );
        }
        if query == CREATE_PROJECT {
            assert_eq!(variables["repository"], "R_scratch");
            return Ok(
                json!({"createProjectV2":{"projectV2":{"id":"P_scratch","number":3,"title":NONCE,"owner":{"login":owner("project")}}}}),
            );
        }
        if query == CREATE_ISSUE {
            return Ok(
                json!({"createIssue":{"issue":{"id":"I_scratch","number":1,"repository":{"nameWithOwner":REPO}}}}),
            );
        }
        if query == COMMENT {
            assert_eq!(variables["issue"], "I_scratch");
            *self.comment.borrow_mut() = variables["body"].as_str().map(str::to_owned);
            return Ok(json!({"addComment":{"commentEdge":{"node":{"id":"C_result"}}}}));
        }
        if query == CLOSE {
            self.closed.set(true);
            return Ok(json!({"closeIssue":{"issue":{"id":"I_scratch","state":"CLOSED"}}}));
        }
        if query == RESULT {
            let comments: Vec<_> = self
                .comment
                .borrow()
                .iter()
                .map(|body| json!({"id":"C_result","body":body}))
                .collect();
            let (state, reason) = if self.closed.get() {
                ("CLOSED", json!("COMPLETED"))
            } else {
                ("OPEN", Value::Null)
            };
            return Ok(
                json!({"node":{"id":"I_scratch","state":state,"stateReason":reason,"comments":{"totalCount":comments.len(),"nodes":comments}}}),
            );
        }
        if query == DELETE_PROJECT {
            assert_eq!(variables["id"], "P_scratch");
            return Ok(json!({"deleteProjectV2":{"projectV2":{"id":"P_scratch"}}}));
        }
        if query == REPOSITORY_NODE {
            assert_eq!(variables["id"], "R_scratch");
            let name = if self.case == "renamed" {
                "cgwalters-forge-stage/reused"
            } else {
                REPO
            };
            return Ok(json!({"node":{"id":"R_scratch","nameWithOwner":name}}));
        }
        assert!(
            !query.contains("createRepository(") && !query.contains("deleteProjectV2("),
            "unexpected write: {query}"
        );
        if query.contains("repositoryOwner(") {
            assert_eq!(variables["number"], 3);
            return Ok(
                json!({"repositoryOwner":{"projectV2":{"id":"P_scratch","url":"https://github.com/orgs/cgwalters-forge-stage/projects/3"}}}),
            );
        }
        if query.contains("issue(number:") {
            let nodes = if self.member.get() {
                json!([{"id":"M_scratch","project":{"id":"P_scratch"}}])
            } else {
                json!([])
            };
            return Ok(
                json!({"repository":{"issue":{"id":"I_scratch","number":1,"repository":{"nameWithOwner":REPO},"projectItems":{"nodes":nodes,"pageInfo":{"hasNextPage":false}}}}}),
            );
        }
        if query.contains("addProjectV2ItemById(") {
            assert_eq!(variables["issue"], "I_scratch");
            self.member.set(true);
            return Ok(json!({"addProjectV2ItemById":{"item":{"id":"M_scratch"}}}));
        }
        if query.contains("updateProjectV2ItemFieldValue(") {
            assert_eq!(variables["item"], "M_scratch");
            let option = variables["value"]["singleSelectOptionId"].clone();
            *self.status.borrow_mut() = match option.as_str() {
                Some("todo") => Some("Todo".into()),
                Some("done") => Some("Done".into()),
                other => panic!("unexpected Status option {other:?}"),
            };
            return Ok(
                json!({"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"M_scratch","fieldValueByName":{"optionId":option,"text":null}}}}),
            );
        }
        let last =
            |nodes: Value| json!({"nodes":nodes,"pageInfo":{"hasNextPage":false,"endCursor":null}});
        let (key, nodes) = if query.contains("fields(first") {
            (
                "fields",
                json!([{"id":"F_status","name":"Status","dataType":"SINGLE_SELECT","options":[{"id":"todo","name":"Todo"},{"id":"progress","name":"In Progress"},{"id":"done","name":"Done"}]}]),
            )
        } else {
            assert!(query.contains("items(first"), "unexpected read: {query}");
            let values: Vec<_> = self
                .status
                .borrow()
                .iter()
                .map(|name| json!({"__typename":"ProjectV2ItemFieldSingleSelectValue","name":name,"optionId":name.to_lowercase(),"field":{"name":"Status"}}))
                .collect();
            let (state, reason) = if self.closed.get() {
                ("CLOSED", json!("COMPLETED"))
            } else {
                ("OPEN", Value::Null)
            };
            let items = if self.member.get() {
                json!([{"id":"M_scratch","fieldValues":last(json!(values)),"content":{"__typename":"Issue","id":"I_scratch","number":1,"title":NONCE,"state":state,"stateReason":reason,"createdAt":"2026-10-30T00:00:00Z","updatedAt":"2026-10-30T00:00:00Z","closedAt":null,"repository":{"nameWithOwner":REPO},"labels":last(json!([])),"assignees":last(json!([]))}}])
            } else {
                json!([])
            };
            ("items", items)
        };
        Ok(json!({"node":{key:last(nodes)}}))
    }
}

impl Rest for Stage {
    fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
        panic!("unexpected GET {url}")
    }

    fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        assert_eq!(
            (method, url, body),
            (
                "DELETE",
                format!("{API_ROOT}repos/{REPO}").as_str(),
                &Value::Null
            )
        );
        self.record(format!("{method} {url}"))?;
        Ok((204, Value::Null))
    }
}

fn config() -> Config {
    serde_json::from_str(include_str!("../../../../../fixtures/deploy-config.json")).unwrap()
}

/// [`run`] on a current-thread runtime, as the CLI's, cancelled before step
/// `cancel_at` (0-based) if given.
fn smoke(stage: &Stage, config: &Config, cancel_at: Option<usize>) -> (Value, usize, Result<()>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut checked = 0;
    run(
        &runtime,
        config,
        &Blocking(stage),
        stage,
        NONCE,
        &mut || {
            checked += 1;
            cancel_at.is_some_and(|at| checked > at)
        },
    )
}

fn states(report: &Value, key: &str) -> Vec<String> {
    report[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["state"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn success_verifies_each_stage_and_cleans_up_by_receipt() {
    let stage = Stage::default();
    let (report, writes, result) = smoke(&stage, &config(), None);
    result.unwrap();
    assert_eq!(stage.calls.borrow().len(), CALLS);
    assert_eq!(states(&report, "steps"), ["ok"; 9]);
    assert_eq!(states(&report, "cleanup"), ["deleted"; 2]);
    assert_eq!(report["leftovers"], json!([]));
    assert_eq!(report["receipts"]["dispatch"]["engine"], "fake");
    assert_eq!(report["receipts"]["repository"]["nameWithOwner"], REPO);
    assert_eq!(writes, SMOKE_WRITES + 2);
    assert_eq!(writes, stage.writes());
    assert_eq!(*stage.status.borrow(), Some("Done".into()));
}

#[test]
fn write_cap_refuses_mutations_past_the_smoke_writes() {
    let stage = Stage::default();
    let counting = Counting {
        inner: &stage,
        writes: Cell::new(0),
    };
    for n in 0..=SMOKE_WRITES {
        let result = counting.graphql(CLOSE, json!({"id":"I_scratch"}));
        assert_eq!(result.is_ok(), n < SMOKE_WRITES, "write {n}");
    }
    assert_eq!(stage.calls.borrow().len(), SMOKE_WRITES);
    counting
        .graphql(ORGANIZATION, json!({"owner":"cgwalters-forge-stage"}))
        .unwrap();
}

#[test]
fn partial_failure_cleans_up_only_what_creation_returned() {
    for fail in 1..=CALLS {
        let stage = Stage {
            fail,
            ..Stage::default()
        };
        let (report, writes, result) = smoke(&stage, &config(), None);
        assert!(result.is_err(), "failure at {fail}");
        assert_eq!(writes, stage.writes(), "failure at {fail}");
        assert!(writes <= SMOKE_WRITES + 2);
        // Call 2 returns the repository receipt and call 3 the project's.
        assert_eq!(stage.called(DELETE_PROJECT), fail > 3, "failure at {fail}");
        assert_eq!(
            stage.called(&format!("DELETE {API_ROOT}repos/{REPO}")),
            fail > 2 && fail != CALLS - 1,
            "failure at {fail}"
        );
        // A failed cleanup leaves its object, reported by ID; a lost creation
        // answer may have left one, reported by the name to look for.
        let leftovers = report["leftovers"].as_array().unwrap();
        let unknown = fail == 2 || fail == 3;
        assert_eq!(
            leftovers.len(),
            usize::from(fail > CALLS - 3 || unknown),
            "failure at {fail}"
        );
        assert!(leftovers.iter().all(|l| if unknown {
            l["name"] == NONCE && l["receipt"].is_null()
        } else {
            l["receipt"]["id"].is_string()
        }));
        assert!(!states(&report, "steps").contains(&"cancelled".to_owned()));
    }
}

#[test]
fn cancellation_starts_no_further_step_and_cleans_up() {
    for at in 0..9 {
        let stage = Stage::default();
        let (report, writes, result) = smoke(&stage, &config(), Some(at));
        let error = result.unwrap_err();
        assert!(error.downcast_ref::<Cancelled>().is_some(), "{error:#}");
        assert_eq!(crate::live::error_exit_code(&error), 2);
        let steps = states(&report, "steps");
        assert_eq!(steps.len(), at + 1);
        assert_eq!(steps.last().unwrap(), "cancelled");
        assert!(steps[..at].iter().all(|s| s == "ok"));
        assert_eq!(stage.called(DELETE_PROJECT), at > 2, "cancelled at {at}");
        assert_eq!(
            stage.called(&format!("DELETE {API_ROOT}repos/{REPO}")),
            at > 1,
            "cancelled at {at}"
        );
        assert_eq!(report["leftovers"], json!([]));
        assert_eq!(writes, stage.writes());
    }
}

#[test]
fn wrong_owner_is_refused_and_reported_never_deleted() {
    // A wrong configured owner is refused before any request.
    let stage = Stage::default();
    let mut production = config();
    production.owner = "cgwalters-forge".into();
    production.repository = "cgwalters-forge/agent-board".into();
    let (_, writes, result) = smoke(&stage, &production, None);
    assert!(result.is_err());
    assert_eq!((writes, stage.calls.borrow().len()), (0, 0));

    for (case, steps, leftover, repository_deleted) in [
        ("repository", 2, "repository", false),
        ("project", 3, "project", true),
        ("renamed", 9, "repository", false),
    ] {
        let stage = Stage {
            case,
            ..Stage::default()
        };
        let (report, _, result) = smoke(&stage, &config(), None);
        assert!(result.is_err(), "{case}");
        assert_eq!(states(&report, "steps").len(), steps, "{case}");
        let leftovers = report["leftovers"].as_array().unwrap();
        assert_eq!(leftovers.len(), 1, "{case}");
        assert_eq!(leftovers[0]["resource"], leftover, "{case}");
        assert!(!stage.called(DELETE_PROJECT) || case == "renamed", "{case}");
        assert_eq!(
            stage.called(&format!("DELETE {API_ROOT}repos/{REPO}")),
            repository_deleted,
            "{case}"
        );
        assert!(!stage.called(CREATE_ISSUE) || case == "renamed", "{case}");
    }
}
