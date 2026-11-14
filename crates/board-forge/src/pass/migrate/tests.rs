use super::*;

fn config() -> Config {
    let mut value: Value =
        serde_json::from_str(include_str!("../../../../../fixtures/pass/config.json")).unwrap();
    value["midstream"] = json!({"owner":"cgwalters-forge", "topic":"bot-midstream"});
    value["pr"] = json!({"fixPools":["claude"]});
    let mut config = Config::parse(&value.to_string()).unwrap();
    config.resolved_midstreams = Some(vec![
        "cgwalters-forge/bootc".into(),
        "cgwalters-forge/ostree".into(),
    ]);
    config
}

fn node(number: u64) -> Value {
    json!({"__typename":"Issue", "id":format!("issue-{number}"), "number":number,
        "url":format!("https://github.com/cgwalters-forge/tracker/issues/{number}"),
        "repository":{"nameWithOwner":"cgwalters-forge/tracker"},
        "title":"bootc: repair regression", "body":"", "updatedAt":"2026-11-13T00:00:00Z", "state":"OPEN",
        "parent":{"id":"epic", "url":"https://github.com/cgwalters-forge/tracker/issues/1599"},
        "subIssues":{"totalCount":0},
        "milestone":null,
        "labels":{"nodes":[],"pageInfo":{"hasNextPage":false}},
        "assignees":{"nodes":[{"id":"user-1"},{"id":"user-2"}],"pageInfo":{"hasNextPage":false}},
        "closedByPullRequestsReferences":{"nodes":[],"totalCount":0,"pageInfo":{"hasNextPage":false}},
        "timelineItems":{"nodes":[],"pageInfo":{"hasNextPage":false}},
        "projectItems":{"nodes":[{"id":format!("item-{number}"),"project":{"id":"board", "number":1,"owner":{"login":"cgwalters-forge"}},
            "fieldValues":{"nodes":[{"optionId":"priority-1","name":"P1","field":{"name":"Priority"}}],"pageInfo":{"hasNextPage":false}}}],
            "pageInfo":{"hasNextPage":false}}})
}

#[derive(Default)]
struct Forge {
    moved: RefCell<BTreeSet<String>>,
    transfer_ids: RefCell<Vec<String>>,
    transfers: Cell<usize>,
    snapshots: Cell<usize>,
    readback_failed: Cell<bool>,
    failure: &'static str,
    disabled: bool,
    incomplete: bool,
    board_incomplete: bool,
    metadata: &'static str,
    active: bool,
    active_repo: &'static str,
    open_pr_repo: &'static str,
    incomplete_references: &'static str,
    labeled: bool,
    milestoned: bool,
    missing_metadata: &'static str,
    edit_during_preflight: bool,
    linked: bool,
    cross_reference: bool,
    references_after_transfer: bool,
    plan: bool,
    stale: bool,
    children: Option<Value>,
    children_during_preflight: bool,
    assignees: Option<Value>,
    assignees_during_preflight: bool,
    count: u64,
    body: RefCell<String>,
}

impl Rest for Forge {
    fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
        let path = url.strip_prefix(API_ROOT).unwrap();
        let body = if path == "user" {
            json!({"login":"cgwalters-bot"})
        } else if path.contains("/actions/runs?") {
            let active = self.active
                && (self.active_repo.is_empty()
                    || path.starts_with(&format!("repos/{}/", self.active_repo)));
            json!({"total_count":u64::from(active),"workflow_runs":if active {vec![json!({"id":1})]} else {vec![]}})
        } else if path.ends_with("/pulls?state=open&per_page=1") {
            if !self.open_pr_repo.is_empty()
                && path.starts_with(&format!("repos/{}/", self.open_pr_repo))
            {
                json!([{"number":7}])
            } else {
                json!([])
            }
        } else if path == "repos/cgwalters-forge/bootc" || path == "repos/cgwalters-forge/ostree" {
            if self.metadata == "inaccessible" {
                bail!("HTTP 404");
            }
            let repository = path.strip_prefix("repos/").unwrap();
            let mut metadata = json!({"full_name":repository,"fork":true,"node_id":repository,
                "has_issues":!self.disabled,"archived":false,"topics":["bot-midstream"],
                "parent":{"full_name":if repository.ends_with("bootc") {"bootc-dev/bootc"} else {"ostreedev/ostree"}}});
            if self.metadata == "incomplete" {
                metadata["parent"] = Value::Null;
            }
            metadata
        } else if path == "repos/bootc-dev/bootc/issues/42" {
            if self.edit_during_preflight {
                self.body
                    .borrow_mut()
                    .push_str("\nHuman edit during preflight");
            }
            json!({"number":42,"repository_url":format!("{API_ROOT}repos/bootc-dev/bootc")})
        } else {
            bail!("unrecorded GET {path}")
        };
        Ok(RestResponse {
            status: 200,
            body,
            etag: None,
            last_modified: None,
            link: None,
        })
    }

    fn send(&self, method: &str, _: &str, body: &Value) -> Result<(u16, Value)> {
        panic!("migration must not edit issue bodies: {method} {body}")
    }
}

impl Transport for Forge {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        if query == BOARD {
            return Ok(
                json!({"repositoryOwner":{"projectV2":{"id":"board","number":1,
                "field":{"id":"priority","name":"Priority"},
                "items":{"pageInfo":if self.board_incomplete {Value::Null} else {json!({"hasNextPage":false})},
                    "nodes":(1..=self.count.max(1)).map(|n| json!({"id":format!("item-{n}"),"content":node(n)})).collect::<Vec<_>>()}}}}),
            );
        }
        if query == plan::COMMENTS_QUERY {
            if self.plan {
                let plan = json!({"schema":JOB_PLAN_SCHEMA,"repo":"cgwalters-forge/tracker","item":1,
                    "target_repo":"cgwalters-forge/bootc","kind":"implement","runner":"hosted","brief":"Repair regression"});
                return Ok(json!({"repository":{"issue":{"comments":{"nodes":[{
                    "url":"https://github.com/cgwalters-forge/tracker/issues/1#issuecomment-1",
                    "body":format!("```{PLAN_FENCE}\n{plan}\n```"),"author":{"login":"cgwalters-bot"},
                    "createdAt":"2026-11-13T00:00:00Z","updatedAt":"2026-11-13T00:00:00Z",
                    "lastEditedAt":null,"editor":null
                }],"pageInfo":{"hasNextPage":false}}}}}));
            }
            return Ok(
                json!({"repository":{"issue":{"comments":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}),
            );
        }
        if query == DESTINATION {
            return Ok(json!({"repository":{
                "nameWithOwner":format!("{}/{}", variables["owner"].as_str().unwrap(), variables["name"].as_str().unwrap()),
                "labels":{"nodes":if self.missing_metadata == "labels" {vec![]} else {vec![json!({"name":"migration: retained"})]},"pageInfo":{"hasNextPage":false}},
                "milestones":{"nodes":if self.missing_metadata == "milestone" {vec![]} else {vec![json!({"title":"Next","dueOn":null})]},"pageInfo":{"hasNextPage":false}}
            }}));
        }
        if query == TRANSFER {
            self.transfers.set(self.transfers.get() + 1);
            let id = variables["issue"].as_str().unwrap().to_owned();
            self.transfer_ids.borrow_mut().push(id.clone());
            if self.failure == "before" {
                bail!("transport failed before response");
            }
            self.moved.borrow_mut().insert(id);
            if self.failure == "timeout" {
                bail!("timeout after GitHub applied");
            }
            return Ok(json!({"transferIssue":{"issue":{"id":variables["issue"],"number":99}}}));
        }
        assert_eq!(query, SNAPSHOT);
        let moved = self
            .moved
            .borrow()
            .contains(variables["id"].as_str().unwrap());
        if moved && self.failure == "readback" && !self.readback_failed.replace(true) {
            bail!("readback unavailable after transfer");
        }
        self.snapshots.set(self.snapshots.get() + 1);
        let number = variables["id"]
            .as_str()
            .unwrap()
            .strip_prefix("issue-")
            .unwrap()
            .parse()
            .unwrap();
        let mut node = node(number);
        node["body"] = json!(*self.body.borrow());
        if let Some(assignees) = &self.assignees {
            node["assignees"] = assignees.clone();
        }
        if self.assignees_during_preflight && self.snapshots.get() > 1 {
            node["assignees"]["nodes"] = json!([]);
        }
        if let Some(children) = &self.children {
            node["subIssues"] = children.clone();
        }
        if self.children_during_preflight && self.snapshots.get() > 1 {
            node["subIssues"] = json!({"totalCount":1});
        }
        if self.labeled {
            node["labels"]["nodes"] = json!([{"name":"migration: retained"}]);
        }
        if self.milestoned {
            node["milestone"] = json!({"title":"Next","dueOn":null});
        }
        if self.linked && (!self.references_after_transfer || moved) {
            node["closedByPullRequestsReferences"]["nodes"] =
                json!([{"state":"OPEN","number":7,"repository":{"nameWithOwner":"external/work"}}]);
            node["closedByPullRequestsReferences"]["totalCount"] = json!(1);
        }
        if self.cross_reference && (!self.references_after_transfer || moved) {
            node["timelineItems"]["nodes"] = json!([{"__typename":"CrossReferencedEvent","source":{"__typename":"PullRequest","state":"OPEN"}}]);
        }
        match if self.references_after_transfer && !moved {
            ""
        } else {
            self.incomplete_references
        } {
            "closing-page" => node["closedByPullRequestsReferences"]["pageInfo"] = Value::Null,
            "closing-count" => node["closedByPullRequestsReferences"]["totalCount"] = json!(1),
            "closing-state" => {
                node["closedByPullRequestsReferences"]["nodes"] = json!([{"number":7}]);
                node["closedByPullRequestsReferences"]["totalCount"] = json!(1);
            }
            "timeline-page" => node["timelineItems"]["pageInfo"] = Value::Null,
            "timeline-source" => {
                node["timelineItems"]["nodes"] =
                    json!([{"__typename":"CrossReferencedEvent","source":null}]);
            }
            "timeline-state" => {
                node["timelineItems"]["nodes"] = json!([{"__typename":"CrossReferencedEvent","source":{"__typename":"PullRequest"}}]);
            }
            "timeline-kind" => {
                node["timelineItems"]["nodes"] = json!([{"__typename":"CrossReferencedEvent","source":{"__typename":"Unknown"}}]);
            }
            "" => {}
            other => panic!("unrecorded reference failure {other}"),
        }
        if moved {
            node["repository"]["nameWithOwner"] = json!("cgwalters-forge/bootc");
            node["number"] = json!(98 + number);
            node["url"] = json!(format!(
                "https://github.com/cgwalters-forge/bootc/issues/{}",
                98 + number
            ));
            if self.failure == "priority" {
                node["projectItems"]["nodes"][0]["fieldValues"]["nodes"] = json!([]);
            }
            if self.failure == "parent" {
                node["parent"] = Value::Null;
            }
            if self.failure == "labels" {
                node["labels"]["nodes"] = json!([]);
            }
            if self.failure == "milestone" {
                node["milestone"] = Value::Null;
            }
            match self.failure {
                "assignees" => node["assignees"]["nodes"] = json!([]),
                "assignees-replaced" => node["assignees"]["nodes"] = json!([{"id":"user-3"}]),
                "assignees-unreadable" => node["assignees"] = Value::Null,
                _ => node["assignees"]["nodes"].as_array_mut().unwrap().reverse(),
            }
        }
        if self.incomplete {
            node["labels"]["pageInfo"] = Value::Null;
        }
        if self.stale && self.snapshots.get() > 1 {
            node["updatedAt"] = json!("2026-11-13T00:01:00Z");
        }
        let mut board = node["projectItems"]["nodes"][0].clone();
        board["__typename"] = json!("ProjectV2Item");
        board["content"] = json!({"id":node["id"]});
        Ok(json!({"node":node, "board":board}))
    }
}

#[test]
fn recorded_classification_table() {
    let config = config();
    let forks = discover_forks(&Forge::default(), &config).unwrap();
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../../fixtures/pass/midstream-migration.json"
    ))
    .unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let mut node = node(1);
        node["title"] = case["title"].clone();
        node["body"] = case["body"].clone();
        node["labels"]["nodes"] = json!(
            case["labels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| json!({"name":l}))
                .collect::<Vec<_>>()
        );
        match classify(&node, &[], &config, &forks) {
            Ok(index) => assert_eq!(forks[index].repository, case["destination"], "{case}"),
            Err(error) => assert!(
                format!("{error:#}").contains(case["hold"].as_str().unwrap()),
                "{case}: {error:#}"
            ),
        }
    }
}

fn execute(
    forge: &Forge,
    apply: bool,
    max: usize,
    journal: &mut Journal,
) -> (Result<()>, Vec<String>) {
    let mut lines = Vec::new();
    let result = run(
        forge,
        &config(),
        apply,
        max,
        journal,
        |_| Ok(()),
        |s| {
            lines.push(s);
            Ok(())
        },
    );
    (result, lines)
}

#[test]
fn recorded_preview_replays_without_writes() {
    let forge = Forge::default();
    let recorder = Recorder::new(&forge);
    let mut lines = Vec::new();
    run(
        &recorder,
        &config(),
        false,
        1,
        &mut Journal::default(),
        |_| panic!("preview save"),
        |s| {
            lines.push(s);
            Ok(())
        },
    )
    .unwrap();
    let fixture = recorder.fixture(OffsetDateTime::UNIX_EPOCH).unwrap();
    let replay = Replay::from_json(&serde_json::from_str(&fixture).unwrap()).unwrap();
    let mut replayed = Vec::new();
    run(
        &replay,
        &config(),
        false,
        1,
        &mut Journal::default(),
        |_| panic!("preview save"),
        |s| {
            replayed.push(s);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(lines, replayed);
    assert!(lines[0].starts_with("preview "));
    assert_eq!(forge.transfers.get(), 0);
}

#[test]
fn disabled_incomplete_active_and_exhausted_caps_hold_without_writes() {
    for forge in [
        Forge {
            disabled: true,
            ..Default::default()
        },
        Forge {
            incomplete: true,
            ..Default::default()
        },
        Forge {
            active: true,
            ..Default::default()
        },
        Forge {
            linked: true,
            ..Default::default()
        },
        Forge {
            cross_reference: true,
            ..Default::default()
        },
        Forge {
            plan: true,
            ..Default::default()
        },
        Forge {
            metadata: "inaccessible",
            ..Default::default()
        },
        Forge {
            metadata: "incomplete",
            ..Default::default()
        },
    ] {
        let (result, lines) = execute(&forge, true, 1, &mut Journal::default());
        result.unwrap();
        assert!(lines[0].starts_with("hold "), "{lines:?}");
        assert_eq!(forge.transfers.get(), 0);
        assert!(forge.body.borrow().is_empty());
    }
    let forge = Forge {
        count: 2,
        ..Default::default()
    };
    let (result, lines) = execute(&forge, true, 1, &mut Journal::default());
    result.unwrap();
    assert_eq!(forge.transfers.get(), 1);
    assert!(
        lines.iter().any(|s| s.contains("exhausted --max")),
        "{lines:?}"
    );
    assert!(
        execute(&Forge::default(), true, 0, &mut Journal::default())
            .0
            .is_err()
    );
    let forge = Forge {
        board_incomplete: true,
        ..Default::default()
    };
    assert!(execute(&forge, true, 1, &mut Journal::default()).0.is_err());
    assert_eq!(forge.transfers.get(), 0);
}

#[test]
fn structural_epics_and_unreadable_child_counts_never_transfer() {
    for (children, reason) in [
        (json!({"totalCount":1}), "parent with sub-issues"),
        (json!({"totalCount":1000}), "parent with sub-issues"),
        (Value::Null, "missing or invalid sub-issue count"),
        (json!({}), "missing or invalid sub-issue count"),
        (
            json!({"totalCount":null}),
            "missing or invalid sub-issue count",
        ),
        (
            json!({"totalCount":-1}),
            "missing or invalid sub-issue count",
        ),
        (
            json!({"totalCount":"0"}),
            "missing or invalid sub-issue count",
        ),
    ] {
        for apply in [false, true] {
            let forge = Forge {
                children: Some(children.clone()),
                ..Default::default()
            };
            let mut journal = Journal::default();
            let (result, lines) = execute(&forge, apply, 1, &mut journal);
            result.unwrap();
            assert!(
                lines[0].starts_with("hold ") && lines[0].contains(reason),
                "{lines:?}"
            );
            assert_eq!(forge.transfers.get(), 0);
            assert!(journal.receipts.is_empty());
        }
    }
    // The fixture has a parent of its own: leaf children remain transferable.
    let forge = Forge::default();
    execute(&forge, true, 1, &mut Journal::default()).0.unwrap();
    assert_eq!(forge.transfers.get(), 1);
}

#[test]
fn child_added_during_preflight_stops_before_transfer_intent() {
    let forge = Forge {
        children_during_preflight: true,
        ..Default::default()
    };
    let mut journal = Journal::default();
    let (result, _) = execute(&forge, true, 1, &mut journal);
    assert!(format!("{:#}", result.unwrap_err()).contains("parent with sub-issues"));
    assert_eq!(forge.transfers.get(), 0);
    assert!(journal.receipts.is_empty());
}

#[test]
fn transfer_failures_reconcile_before_any_retry_and_preservation_failure_stops() {
    for failure in [
        "",
        "before",
        "timeout",
        "readback",
        "priority",
        "parent",
        "labels",
        "milestone",
        "assignees",
        "assignees-replaced",
        "assignees-unreadable",
    ] {
        let forge = Forge {
            failure,
            labeled: true,
            milestoned: true,
            ..Default::default()
        };
        let mut journal = Journal::default();
        let (result, lines) = execute(&forge, true, 1, &mut journal);
        let success = failure.is_empty() || failure == "timeout";
        assert_eq!(result.is_ok(), success, "{failure}: {result:?}");
        let receipt = journal.receipts.get("issue-1").unwrap();
        assert_eq!(receipt.canonical.is_some(), success);
        if success {
            assert!(lines[0].contains("/bootc/issues/99"));
        }
        // Round-trip the saved intent just as a new process would do.
        let mut journal = serde_json::from_value(serde_json::to_value(&journal).unwrap()).unwrap();
        let (retry, _) = execute(&forge, true, 1, &mut journal);
        assert_eq!(retry.is_ok(), success || failure == "readback");
        assert_eq!(forge.transfers.get(), 1, "{failure} retried a mutation");
    }
}

#[test]
fn batch_recovery_restarts_from_durable_intent_before_advancing() {
    // Only successful save callbacks survive a process restart. In particular,
    // the in-memory canonical receipt must not hide a failed completion save.
    fn restart(
        forge: &Forge,
        apply: bool,
        max: usize,
        durable: &mut Vec<u8>,
        fail_completion: bool,
    ) -> (Result<()>, Vec<String>) {
        let mut journal = serde_json::from_slice(durable).unwrap();
        let mut lines = Vec::new();
        let result = run(
            forge,
            &config(),
            apply,
            max,
            &mut journal,
            |journal| {
                assert!(apply, "preview must not save reconciliation");
                ensure!(
                    !fail_completion || journal.receipts.values().all(|r| r.canonical.is_none()),
                    "disk full saving completion"
                );
                *durable = serde_json::to_vec(journal)?;
                Ok(())
            },
            |line| {
                lines.push(line);
                Ok(())
            },
        );
        (result, lines)
    }

    for failure in [
        "before",
        "readback",
        "priority",
        "parent",
        "labels",
        "milestone",
        "assignees",
        "assignees-replaced",
        "assignees-unreadable",
        "save-completion",
    ] {
        let mut forge = Forge {
            failure,
            count: 2,
            labeled: true,
            milestoned: true,
            ..Default::default()
        };
        let mut durable = serde_json::to_vec(&Journal::default()).unwrap();
        let (result, lines) = restart(&forge, true, 2, &mut durable, failure == "save-completion");
        assert!(result.is_err(), "{failure}: {lines:?}");
        assert!(lines.iter().all(|line| !line.starts_with("transferred ")));
        assert_eq!(*forge.transfer_ids.borrow(), ["issue-1"], "{failure}");
        let pending = durable.clone();
        let journal: Journal = serde_json::from_slice(&pending).unwrap();
        assert_eq!(journal.receipts.len(), 1, "{failure}");
        assert!(journal.receipts["issue-1"].canonical.is_none(), "{failure}");

        // Persistent loss (or an intent still at the source) blocks the next
        // candidate on restart too, without resending the first mutation.
        if !["readback", "save-completion"].contains(&failure) {
            let (result, _) = restart(&forge, true, 2, &mut durable, false);
            assert!(result.is_err(), "{failure}");
            assert_eq!(durable, pending, "{failure}");
            assert_eq!(*forge.transfer_ids.borrow(), ["issue-1"], "{failure}");
        }
        if failure == "before" {
            continue; // An unresolved source intent requires manual investigation.
        }

        // Model restored metadata/read availability, not an automatic repair.
        forge.failure = "";
        let (result, lines) = restart(&forge, false, 1, &mut durable, false);
        result.unwrap();
        assert!(lines[0].starts_with("reconciled "), "{failure}: {lines:?}");
        assert!(lines[1].starts_with("preview cgwalters-forge/tracker#2 "));
        assert_eq!(durable, pending, "preview persisted recovery: {failure}");
        assert_eq!(*forge.transfer_ids.borrow(), ["issue-1"]);

        // Reconciliation consumes no transfer allowance: the one permitted
        // new write belongs to issue-2, never a duplicate issue-1 transfer.
        let (result, lines) = restart(&forge, true, 1, &mut durable, false);
        result.unwrap();
        assert!(lines[0].starts_with("reconciled "), "{failure}: {lines:?}");
        assert!(lines[1].starts_with("transferred cgwalters-forge/tracker#2 "));
        assert_eq!(*forge.transfer_ids.borrow(), ["issue-1", "issue-2"]);
        let journal: Journal = serde_json::from_slice(&durable).unwrap();
        assert_eq!(journal.receipts.len(), 2);
        for (id, number) in [("issue-1", 99), ("issue-2", 100)] {
            assert_eq!(
                journal.receipts[id].canonical.as_deref(),
                Some(format!("https://github.com/cgwalters-forge/bootc/issues/{number}").as_str())
            );
        }
        restart(&forge, true, 2, &mut durable, false).0.unwrap();
        assert_eq!(*forge.transfer_ids.borrow(), ["issue-1", "issue-2"]);
    }
}

#[test]
fn incomplete_assignees_hold_without_transfer() {
    for assignees in [
        Value::Null,
        json!({"nodes":[]}),
        json!({"nodes":[],"pageInfo":{"hasNextPage":true}}),
        json!({"nodes":null,"pageInfo":{"hasNextPage":false}}),
        json!({"nodes":[null],"pageInfo":{"hasNextPage":false}}),
        json!({"nodes":[{"id":""}],"pageInfo":{"hasNextPage":false}}),
    ] {
        for apply in [false, true] {
            let forge = Forge {
                assignees: Some(assignees.clone()),
                ..Default::default()
            };
            let mut journal = Journal::default();
            let (result, lines) = execute(&forge, apply, 1, &mut journal);
            result.unwrap();
            assert!(lines[0].starts_with("hold "), "{lines:?}");
            assert_eq!(forge.transfers.get(), 0);
            assert!(journal.receipts.is_empty());
        }
    }
    // An explicitly empty connection, unlike missing evidence, is valid.
    let forge = Forge {
        assignees: Some(json!({"nodes":[],"pageInfo":{"hasNextPage":false}})),
        ..Default::default()
    };
    execute(&forge, true, 1, &mut Journal::default()).0.unwrap();
    assert_eq!(forge.transfers.get(), 1);
}

#[test]
fn assignee_change_during_preflight_stops_before_transfer_intent() {
    let forge = Forge {
        assignees_during_preflight: true,
        ..Default::default()
    };
    let mut journal = Journal::default();
    let (result, _) = execute(&forge, true, 1, &mut journal);
    assert!(format!("{:#}", result.unwrap_err()).contains("stale source evidence"));
    assert_eq!(forge.transfers.get(), 0);
    assert!(journal.receipts.is_empty());
}

#[test]
fn journal_without_assignee_evidence_is_not_treated_as_unassigned() {
    let forge = Forge::default();
    let mut journal = Journal::default();
    execute(&forge, true, 1, &mut journal).0.unwrap();
    let mut value = serde_json::to_value(journal).unwrap();
    value["receipts"]["issue-1"]["before"]
        .as_object_mut()
        .unwrap()
        .remove("assignees");
    assert!(
        serde_json::from_value::<Journal>(value)
            .err()
            .unwrap()
            .to_string()
            .contains("missing field `assignees`")
    );
}

#[test]
fn incompatible_destination_metadata_holds_without_transfer() {
    for (missing_metadata, reason) in [
        ("labels", "destination missing source labels"),
        ("milestone", "destination missing matching milestone"),
    ] {
        let forge = Forge {
            labeled: true,
            milestoned: true,
            missing_metadata,
            ..Default::default()
        };
        let mut journal = Journal::default();
        let (result, lines) = execute(&forge, true, 1, &mut journal);
        result.unwrap();
        assert!(
            lines[0].starts_with("hold ") && lines[0].contains(reason),
            "{lines:?}"
        );
        assert_eq!(forge.transfers.get(), 0);
        assert!(journal.receipts.is_empty());
    }
}

#[test]
fn incomplete_pr_reference_reads_hold_without_writes() {
    for (incomplete_references, reason) in [
        ("closing-page", "incomplete closedByPullRequestsReferences"),
        ("closing-count", "in-flight or unreadable linked PRs"),
        ("closing-state", "in-flight or unreadable linked PRs"),
        ("timeline-page", "incomplete timelineItems"),
        ("timeline-source", "missing __typename"),
        ("timeline-state", "missing state"),
        ("timeline-kind", "unknown cross-reference source type"),
    ] {
        let forge = Forge {
            incomplete_references,
            ..Default::default()
        };
        let mut journal = Journal::default();
        let (result, lines) = execute(&forge, true, 1, &mut journal);
        result.unwrap();
        assert!(
            lines[0].starts_with("hold ") && lines[0].contains(reason),
            "{lines:?}"
        );
        assert_eq!(forge.transfers.get(), 0);
        assert!(forge.body.borrow().is_empty());
        assert!(journal.receipts.is_empty());
    }
}

#[test]
fn body_plan_requires_handoff_even_when_body_plans_are_disabled() {
    let plan = json!({"schema":JOB_PLAN_SCHEMA,"repo":"cgwalters-forge/tracker","item":1,
        "target_repo":"cgwalters-forge/bootc","kind":"implement","runner":"hosted","brief":"Repair regression"});
    let forge = Forge {
        body: RefCell::new(format!("```{PLAN_FENCE}\n{plan}\n```")),
        ..Default::default()
    };
    for body_plans in [false, true] {
        let mut config = config();
        config.body_plans = body_plans;
        let mut journal = Journal::default();
        let mut lines = Vec::new();
        run(
            &forge,
            &config,
            true,
            1,
            &mut journal,
            |_| Ok(()),
            |line| {
                lines.push(line);
                Ok(())
            },
        )
        .unwrap();
        assert!(
            lines[0].starts_with("hold ") && lines[0].contains("replacement-plan handoff"),
            "{lines:?}"
        );
        assert_eq!(forge.transfers.get(), 0);
        assert!(journal.receipts.is_empty());
    }
}

#[test]
fn upstream_only_activity_holds_without_writes() {
    for forge in [
        Forge {
            active: true,
            active_repo: "bootc-dev/bootc",
            ..Default::default()
        },
        Forge {
            open_pr_repo: "bootc-dev/bootc",
            ..Default::default()
        },
    ] {
        let mut journal = Journal::default();
        let (result, lines) = execute(&forge, true, 1, &mut journal);
        result.unwrap();
        assert!(
            lines[0].starts_with("hold ")
                && lines[0].contains("in-flight")
                && lines[0].contains("bootc-dev/bootc"),
            "{lines:?}"
        );
        assert_eq!(forge.transfers.get(), 0);
        assert!(forge.body.borrow().is_empty());
        assert!(journal.receipts.is_empty());
    }
}

#[test]
fn completed_transfers_reconcile_despite_new_pr_references() {
    for (linked, cross_reference, incomplete_references) in [
        (true, false, ""),
        (false, true, ""),
        (false, false, "closing-page"),
        (false, false, "timeline-page"),
        (false, false, "timeline-source"),
    ] {
        for restart in [false, true] {
            let forge = Forge {
                linked,
                cross_reference,
                incomplete_references,
                references_after_transfer: true,
                failure: if restart { "readback" } else { "" },
                ..Default::default()
            };
            let mut journal = Journal::default();
            let mut durable = String::new();
            let result = run(
                &forge,
                &config(),
                true,
                1,
                &mut journal,
                |state| {
                    durable = serde_json::to_string(state)?;
                    Ok(())
                },
                |_| Ok(()),
            );
            assert_eq!(result.is_err(), restart, "{result:?}");
            if restart {
                journal = serde_json::from_str(&durable).unwrap();
                assert!(journal.receipts["issue-1"].canonical.is_none());
                // Preview can recognize the transfer without completing the journal.
                let (result, lines) = execute(&forge, false, 1, &mut journal);
                result.unwrap();
                assert!(lines[0].starts_with("reconciled "), "{lines:?}");
                assert!(journal.receipts["issue-1"].canonical.is_none());
                execute(&forge, true, 1, &mut journal).0.unwrap();
            }
            assert_eq!(
                journal.receipts["issue-1"].canonical.as_deref(),
                Some("https://github.com/cgwalters-forge/bootc/issues/99")
            );
            assert_eq!(forge.transfers.get(), 1);
        }
    }
}

#[test]
fn accepted_plan_targets_conflict_and_foreign_edits_hold() {
    let config = config();
    let forks = discover_forks(&Forge::default(), &config).unwrap();
    for (repository, title, foreign_edit, expected) in [
        ("cgwalters-forge/bootc", "repair regression", false, true),
        (
            "cgwalters-forge/ostree",
            "bootc: repair regression",
            false,
            false,
        ),
        ("cgwalters-forge/bootc", "repair regression", true, false),
    ] {
        let mut node = node(1);
        node["title"] = json!(title);
        let plan = json!({"schema":JOB_PLAN_SCHEMA,"repo":repository,"item":1,
            "kind":"implement","runner":"hosted","brief":"Repair regression"});
        let comment = plan::Comment {
            url: "https://github.com/cgwalters-forge/tracker/issues/1#issuecomment-1".into(),
            body: format!("```{PLAN_FENCE}\n{plan}\n```"),
            author: Some(config.bot.clone()),
            edited: foreign_edit,
            editor: foreign_edit.then(|| "visitor".into()),
            created: OffsetDateTime::UNIX_EPOCH,
            changed: OffsetDateTime::UNIX_EPOCH,
        };
        assert_eq!(
            classify(&node, &[comment], &config, &forks).is_ok(),
            expected
        );
    }
}

#[test]
fn stale_source_and_failed_intent_save_never_transfer() {
    let forge = Forge {
        stale: true,
        ..Default::default()
    };
    assert!(execute(&forge, true, 1, &mut Journal::default()).0.is_err());
    assert_eq!(forge.transfers.get(), 0);
    let forge = Forge {
        body: RefCell::new(
            "See https://github.com/bootc-dev/bootc/issues/42\n\nUpstream: #42\n".into(),
        ),
        ..Default::default()
    };
    let initial_body = forge.body.borrow().clone();
    let saves = Cell::new(0);
    let result = run(
        &forge,
        &config(),
        true,
        1,
        &mut Journal::default(),
        |journal| {
            saves.set(saves.get() + 1);
            ensure!(
                journal.receipts.is_empty(),
                "disk full saving pending intent"
            );
            Ok(())
        },
        |_| Ok(()),
    );
    assert!(result.is_err());
    assert_eq!(forge.transfers.get(), 0);
    assert_eq!(saves.get(), 2);
    assert_eq!(*forge.body.borrow(), initial_body);
}

#[test]
fn upstream_uses_parent_and_rejects_conflicts_and_qualified_syntax() {
    let config = config();
    let fork = discover_forks(&Forge::default(), &config)
        .unwrap()
        .remove(0);
    for (body, expected) in [
        ("Upstream: #42", Some(42)),
        ("https://github.com/bootc-dev/bootc/issues/42", Some(42)),
        ("", None),
    ] {
        assert_eq!(association(body, &fork).unwrap(), expected);
    }
    for body in [
        "Upstream: bootc-dev/bootc#42",
        "Upstream: #42\nUpstream: #43",
        "Upstream: #42\nhttps://github.com/bootc-dev/bootc/issues/43",
    ] {
        assert!(association(body, &fork).is_err());
    }
    for edit_during_preflight in [false, true] {
        let body = "See https://github.com/bootc-dev/bootc/issues/42";
        let forge = Forge {
            body: RefCell::new(body.into()),
            edit_during_preflight,
            ..Default::default()
        };
        let (result, lines) = execute(&forge, true, 1, &mut Journal::default());
        result.unwrap();
        assert!(
            lines[0].starts_with("hold ")
                && lines[0].contains("add Upstream: #42 before migration"),
            "{lines:?}"
        );
        assert_eq!(forge.transfers.get(), 0);
        assert_eq!(
            *forge.body.borrow(),
            if edit_during_preflight {
                format!("{body}\nHuman edit during preflight")
            } else {
                body.into()
            }
        );
    }
    let forge = Forge {
        body: RefCell::new("Upstream: #42\n".into()),
        ..Default::default()
    };
    execute(&forge, true, 1, &mut Journal::default()).0.unwrap();
    assert_eq!(forge.transfers.get(), 1);
    assert_eq!(*forge.body.borrow(), "Upstream: #42\n");
}
