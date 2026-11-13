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
        "labels":{"nodes":[],"pageInfo":{"hasNextPage":false}},
        "closedByPullRequestsReferences":{"nodes":[],"totalCount":0,"pageInfo":{"hasNextPage":false}},
        "timelineItems":{"nodes":[],"pageInfo":{"hasNextPage":false}},
        "projectItems":{"nodes":[{"id":format!("item-{number}"),"project":{"id":"board", "number":1,"owner":{"login":"cgwalters-forge"}},
            "fieldValues":{"nodes":[{"optionId":"priority-1","name":"P1","field":{"name":"Priority"}}],"pageInfo":{"hasNextPage":false}}}],
            "pageInfo":{"hasNextPage":false}}})
}

#[derive(Default)]
struct Forge {
    moved: Cell<bool>,
    transfers: Cell<usize>,
    snapshots: Cell<usize>,
    readback_failed: Cell<bool>,
    failure: &'static str,
    disabled: bool,
    incomplete: bool,
    board_incomplete: bool,
    metadata: &'static str,
    active: bool,
    linked: bool,
    cross_reference: bool,
    plan: bool,
    stale: bool,
    count: u64,
    body: RefCell<String>,
}

impl Rest for Forge {
    fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
        let path = url.strip_prefix(API_ROOT).unwrap();
        let body = if path == "user" {
            json!({"login":"cgwalters-bot"})
        } else if path.contains("/actions/runs?") {
            json!({"total_count":u64::from(self.active),"workflow_runs":if self.active {vec![json!({"id":1})]} else {vec![]}})
        } else if path.ends_with("/pulls?state=open&per_page=1") {
            json!([])
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
        assert_eq!(method, "PATCH");
        *self.body.borrow_mut() = body["body"].as_str().unwrap().into();
        Ok((200, json!({})))
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
        if query == TRANSFER {
            self.transfers.set(self.transfers.get() + 1);
            if self.failure == "before" {
                bail!("transport failed before response");
            }
            self.moved.set(true);
            if self.failure == "timeout" {
                bail!("timeout after GitHub applied");
            }
            return Ok(json!({"transferIssue":{"issue":{"id":variables["issue"],"number":99}}}));
        }
        assert_eq!(query, SNAPSHOT);
        if self.moved.get() && self.failure == "readback" && !self.readback_failed.replace(true) {
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
        if self.linked {
            node["closedByPullRequestsReferences"]["nodes"] =
                json!([{"state":"OPEN","number":7,"repository":{"nameWithOwner":"external/work"}}]);
            node["closedByPullRequestsReferences"]["totalCount"] = json!(1);
        }
        if self.cross_reference {
            node["timelineItems"]["nodes"] = json!([{"__typename":"CrossReferencedEvent","source":{"__typename":"PullRequest","state":"OPEN"}}]);
        }
        if self.moved.get() && number == 1 {
            node["repository"]["nameWithOwner"] = json!("cgwalters-forge/bootc");
            node["number"] = json!(99);
            node["url"] = json!("https://github.com/cgwalters-forge/bootc/issues/99");
            if self.failure == "priority" {
                node["projectItems"]["nodes"][0]["fieldValues"]["nodes"] = json!([]);
            }
            if self.failure == "parent" {
                node["parent"] = Value::Null;
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
fn transfer_failures_reconcile_before_any_retry_and_preservation_failure_stops() {
    for failure in ["", "before", "timeout", "readback", "priority", "parent"] {
        let forge = Forge {
            failure,
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
        body: RefCell::new("See https://github.com/bootc-dev/bootc/issues/42".into()),
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
    let forge = Forge {
        body: RefCell::new("See https://github.com/bootc-dev/bootc/issues/42".into()),
        ..Default::default()
    };
    execute(&forge, true, 1, &mut Journal::default()).0.unwrap();
    assert!(forge.body.borrow().ends_with("Upstream: #42\n"));
}
