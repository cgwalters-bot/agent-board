//! Stage 3 on Forgejo, over a fake instance with two repositories: the
//! epics' `example/plans` and the work's `example/work`.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};

const PLANS: &str = "example/plans";
const WORK: &str = "example/work";
const AT: &str = "2026-10-23T15:15:10Z";
/// The epic, example/plans#3.
const EPIC: u64 = 3;
/// A start of the caller.
const DISPATCH: &str = "POST repos/example/control/actions/workflows/agent.yml/dispatches";
/// Label ids: the membership label, `needs-human`, `epic`, then `priority/P0`.
const LABELS: [(u64, &str); 7] = [
    (1, "agent-board"),
    (2, "needs-human"),
    (3, "epic"),
    (10, "priority/P0"),
    (11, "priority/P1"),
    (12, "priority/P2"),
    (13, "priority/P3"),
];

type Key = (String, u64);

fn at(repo: &str, n: u64) -> Key {
    (repo.into(), n)
}

/// A fake Forgejo instance that keeps what is written to it.
struct Instance {
    reads: Fake,
    issues: RefCell<BTreeMap<Key, Value>>,
    comments: RefCell<BTreeMap<Key, Vec<Value>>>,
    timelines: RefCell<BTreeMap<Key, Vec<Value>>>,
    slots: RefCell<Vec<(u64, Value)>>,
    runs: RefCell<Vec<Value>>,
    /// `METHOD PATH` of each write.
    writes: RefCell<Vec<String>>,
    next: Cell<u64>,
    fault: Cell<&'static str>,
    /// Repositories `GET /repos/{o}/{r}` says are private.
    private: RefCell<BTreeSet<String>>,
}

fn labels(names: &[&str]) -> Value {
    (names.iter())
        .map(|name| {
            let (id, _) = LABELS.iter().find(|(_, n)| n == name).unwrap();
            json!({"id": id, "name": name, "exclusive": name.contains('/')})
        })
        .collect()
}

impl Instance {
    /// The live epic example/plans#3, labelled by the operator, and its
    /// agreeing open child example/work#7.
    fn new() -> Self {
        let this = Self {
            reads: Fake::default(),
            issues: RefCell::default(),
            comments: RefCell::default(),
            timelines: RefCell::default(),
            slots: RefCell::new(vec![(0, json!({"free": "initial"})); 2]),
            runs: RefCell::default(),
            writes: RefCell::default(),
            next: Cell::new(1000),
            fault: Cell::new(""),
            private: RefCell::default(),
        };
        this.issue(PLANS, EPIC, OPERATOR, &["epic", "priority/P1"]);
        this.labelled(PLANS, EPIC, OPERATOR, "epic", true);
        this.issue(WORK, 7, BOT, &["agent-board"]);
        this.relate(7);
        this
    }

    fn issue(&self, repo: &str, n: u64, poster: u64, names: &[&str]) {
        let issue = json!({"id": 100 + n, "number": n, "title": format!("Issue {n}"),
            "body": "text", "state": "open", "user": {"id": poster},
            "labels": labels(names), "pull_request": null,
            "created_at": format!("2026-10-{:02}T10:00:00Z", 10 + n % 10),
            "updated_at": AT, "repository": {"full_name": repo}});
        self.issues.borrow_mut().insert(at(repo, n), issue);
    }

    fn labelled(&self, repo: &str, n: u64, user: u64, label: &str, added: bool) {
        let mut timelines = self.timelines.borrow_mut();
        let events = timelines.entry(at(repo, n)).or_default();
        let id = 1 + events.len() as u64;
        events.push(json!({"id": id, "type": "label", "user": {"id": user},
            "label": {"name": label}, "body": if added { "1" } else { "" }}));
    }

    fn comment(&self, repo: &str, n: u64, user: u64, body: &str) -> u64 {
        let id = self.next.get();
        self.next.set(id + 1);
        let comment = json!({"id": id, "user": {"id": user}, "body": body,
            "created_at": AT, "updated_at": AT});
        self.comments
            .borrow_mut()
            .entry(at(repo, n))
            .or_default()
            .push(comment);
        id
    }

    /// Both relationship comments for the epic and `children`: the epic's
    /// list, and each child's parent comment.
    fn relate(&self, child: u64) {
        let epic = crate::epic::IssueRef::parse("example/plans#3").unwrap();
        let mut listed = self.listed();
        listed.push(crate::epic::IssueRef::parse(&format!("{WORK}#{child}")).unwrap());
        let list = crate::forgejo::epics::children_comment(EPIC, &listed);
        self.comment(PLANS, EPIC, BOT, &list);
        let parent = crate::forgejo::epics::parent_comment(&epic);
        self.comment(WORK, child, BOT, &parent);
    }

    /// The children the epic's newest list names.
    fn listed(&self) -> Vec<crate::epic::IssueRef> {
        let comments = self.thread(PLANS, EPIC);
        crate::forgejo::epics::children(&comments, BOT, EPIC).unwrap()
    }

    fn thread(&self, repo: &str, n: u64) -> Vec<Value> {
        self.comments
            .borrow()
            .get(&at(repo, n))
            .cloned()
            .unwrap_or_default()
    }

    fn plan(&self, actions: Value) -> u64 {
        let plan = json!({"schema": "agent-board/epic-plan/v1", "epic": "example/plans#3",
            "actions": actions});
        let body = format!("Plan.\n\n```agent-board-epic-plan\n{plan}\n```\n");
        self.comment(PLANS, EPIC, OPERATOR, &body)
    }

    /// How many writes were `METHOD PATH`.
    fn writes(&self, write: &str) -> usize {
        (self.writes.borrow().iter())
            .filter(|w| *w == write)
            .count()
    }

    fn state(&self, repo: &str, n: u64) -> String {
        self.issues.borrow()[&at(repo, n)]["state"]
            .as_str()
            .unwrap()
            .into()
    }

    fn label_names(&self, repo: &str, n: u64) -> Vec<String> {
        (self.issues.borrow()[&at(repo, n)]["labels"]
            .as_array()
            .unwrap())
        .iter()
        .map(|l| l["name"].as_str().unwrap().to_owned())
        .collect()
    }

    /// The fenced epic-apply detail of the epic's newest receipt.
    fn receipt(&self) -> Value {
        let thread = self.thread(PLANS, EPIC);
        let receipt = (thread.iter().rev())
            .find(|c| (c["body"].as_str().unwrap()).starts_with("<!-- agent-board-receipt:"))
            .unwrap();
        crate::plan::fenced_json(receipt["body"].as_str().unwrap(), APPLY_FENCE)
            .unwrap()
            .unwrap()
    }
}

const APPLY_FENCE: &str = crate::epic_apply::APPLY_MARKER;

impl ForgejoTransport for Instance {
    fn get(&self, url: &url::Url) -> Result<Response> {
        let path = url.path().strip_prefix("/subpath/api/v1/").unwrap();
        let segments: Vec<&str> = path.split('/').collect();
        let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        let paged = |items: &[Value], total: bool| Response {
            body: page(url, items).into(),
            total: total.then_some(items.len() as u64),
        };
        let body = match segments[..] {
            ["repos", owner, name, "issues"] => {
                let repo = format!("{owner}/{name}");
                let listed: Vec<Value> = (self.issues.borrow().iter())
                    .filter(|((r, _), i)| {
                        *r == repo
                            && (query["state"] == "all" || i["state"] == query["state"].as_str())
                            && (query.get("created_by"))
                                .is_none_or(|by| by == "bot" && i["user"]["id"] == BOT)
                            && query.get("labels").is_none_or(|l| {
                                (i["labels"].as_array().unwrap())
                                    .iter()
                                    .any(|x| x["name"] == l.as_str())
                            })
                    })
                    .map(|(_, i)| i.clone())
                    .collect();
                // One the total says is there, but is not listed.
                if self.fault.get() == "sweep-listing" && query.contains_key("created_by") {
                    let mut response = paged(&listed, true);
                    response.total = Some(listed.len() as u64 + 1);
                    return Ok(response);
                }
                return Ok(paged(&listed, true));
            }
            ["repos", owner, name, "issues", n] if n != "comments" => {
                let key = at(&format!("{owner}/{name}"), n.parse()?);
                let mut issue = self.issues.borrow().get(&key).cloned().context("404")?;
                issue["comments"] = self.thread(&key.0, key.1).len().into();
                issue
            }
            ["repos", owner, name, "issues", n, "comments"] => {
                json!(self.thread(&format!("{owner}/{name}"), n.parse()?))
            }
            ["repos", _, _, "issues", "comments", id] => {
                let id: u64 = id.parse()?;
                (self.comments.borrow().values().flatten())
                    .find(|c| c["id"] == id)
                    .cloned()
                    .context("no comment")?
            }
            ["repos", owner, name, "issues", n, "timeline"] => {
                let key = at(&format!("{owner}/{name}"), n.parse()?);
                let events = self
                    .timelines
                    .borrow()
                    .get(&key)
                    .cloned()
                    .unwrap_or_default();
                // As v17.0.1 answers past a timeline's end.
                let events = page(url, &events);
                let body = if events.is_empty() {
                    Value::Null
                } else {
                    events.into()
                };
                return Ok(Response { body, total: None });
            }
            ["repos", _, _, "issues", _, "dependencies"] => return Ok(paged(&[], false)),
            ["repos", _, _, "pulls"] => return Ok(paged(&[], true)),
            ["repos", _, _, "labels"] => {
                let all = labels(&LABELS.map(|(_, n)| n));
                return Ok(paged(all.as_array().unwrap(), true));
            }
            ["orgs", _] => return Err(crate::forgejo::NotFound.into()),
            ["repos", "example", "control"] => json!({"private": true}),
            ["repos", owner, name] => {
                let repo = format!("{owner}/{name}");
                let private = self.private.borrow().contains(&repo);
                json!({"full_name": repo, "private": private, "internal": false,
                    "owner": {"visibility": "public"}})
            }
            ["repos", "example", "control", "contents", "leases", file] => {
                let n: usize = file.trim_end_matches(".json").parse()?;
                let (version, body) = &self.slots.borrow()[n];
                json!({"type": "file", "sha": format!("{version:040x}"), "encoding": "base64",
                    "content": STANDARD.encode(body.to_string())})
            }
            ["repos", "example", "control", "actions", "runs"] => {
                let found: Vec<Value> = (self.runs.borrow().iter())
                    .filter(|r| {
                        query
                            .get("status")
                            .is_none_or(|s| r["status"] == s.as_str())
                    })
                    .cloned()
                    .collect();
                json!({"total_count": found.len(), "workflow_runs": page(url, &found)})
            }
            ["repos", "example", "control", "actions", "runs", id] => {
                let id: u64 = id.parse()?;
                (self.runs.borrow().iter())
                    .find(|r| r["id"] == id)
                    .cloned()
                    .context("run missing")?
            }
            _ => return self.reads.get(url),
        };
        Ok(Response { body, total: None })
    }

    fn send(&self, method: &str, url: &url::Url, body: &Value) -> Result<(u16, Value)> {
        let path = url.path().strip_prefix("/subpath/api/v1/").unwrap();
        self.writes.borrow_mut().push(format!("{method} {path}"));
        let segments: Vec<&str> = path.split('/').collect();
        let fault = self.fault.get();
        match (method, &segments[..]) {
            ("POST", ["repos", owner, name, "issues"]) => {
                let repo = format!("{owner}/{name}");
                let n = 20 + self.issues.borrow().len() as u64;
                let names: Vec<&str> = (body["labels"].as_array().unwrap().iter())
                    .map(|id| LABELS.iter().find(|(i, _)| id == i).unwrap().1)
                    .collect();
                self.issue(&repo, n, BOT, &names);
                let mut issues = self.issues.borrow_mut();
                let issue = issues.get_mut(&at(&repo, n)).unwrap();
                issue["title"] = body["title"].clone();
                issue["body"] = body["body"].clone();
                ensure!(fault != "create-lost", "create reply lost");
                Ok((201, json!({"number": n})))
            }
            ("POST", ["repos", owner, name, "issues", n, "comments"]) => {
                let text = body["body"].as_str().unwrap();
                let repo = format!("{owner}/{name}");
                if fault == "final-receipt" && text.contains("\"outcome\": \"applied\"") {
                    bail!("receipt write failed");
                }
                let id = self.comment(&repo, n.parse()?, BOT, text);
                Ok((201, json!({"id": id})))
            }
            ("POST", ["repos", owner, name, "issues", n, "labels"]) => {
                let id = body["labels"][0].as_u64().unwrap();
                let (_, name_) = LABELS.iter().find(|(i, _)| *i == id).unwrap();
                let mut issues = self.issues.borrow_mut();
                let issue = issues
                    .get_mut(&at(&format!("{owner}/{name}"), n.parse()?))
                    .unwrap();
                let scope = name_.split_once('/').map(|(s, _)| format!("{s}/"));
                let kept: Vec<Value> = (issue["labels"].as_array().unwrap().iter())
                    .filter(|l| {
                        scope
                            .as_ref()
                            .is_none_or(|s| !l["name"].as_str().unwrap().starts_with(s.as_str()))
                    })
                    .cloned()
                    .chain(labels(&[name_]).as_array().unwrap().iter().cloned())
                    .collect();
                issue["labels"] = kept.into();
                Ok((200, json!([])))
            }
            ("PATCH", ["repos", owner, name, "issues", n]) => {
                ensure!(fault != "close", "close failed");
                let mut issues = self.issues.borrow_mut();
                let issue = issues
                    .get_mut(&at(&format!("{owner}/{name}"), n.parse()?))
                    .unwrap();
                issue["state"] = body["state"].clone();
                Ok((201, issue.clone()))
            }
            ("PUT", ["repos", "example", "control", "contents", "leases", file]) => {
                let n: usize = file.trim_end_matches(".json").parse()?;
                let mut slots = self.slots.borrow_mut();
                if body["sha"] != format!("{:040x}", slots[n].0) {
                    return Ok((409, Value::Null));
                }
                slots[n].0 += 1;
                slots[n].1 =
                    serde_json::from_slice(&STANDARD.decode(body["content"].as_str().unwrap())?)?;
                Ok((200, json!({})))
            }
            (
                "POST",
                [
                    "repos",
                    "example",
                    "control",
                    "actions",
                    "workflows",
                    _,
                    "dispatches",
                ],
            ) => {
                let inputs = &body["inputs"];
                let id = 41 + self.runs.borrow().len() as u64;
                let title = format!(
                    "{} {}#{} ({})",
                    inputs["kind"].as_str().unwrap(),
                    inputs["repo"].as_str().unwrap(),
                    inputs["item"].as_str().unwrap(),
                    inputs["pool"].as_str().unwrap()
                );
                let mut run = caller_run(id, &title);
                run["created"] = "2026-10-23T15:15:11Z".into();
                run["trigger_event"] = "workflow_dispatch".into();
                run["event_payload"] = json!({"inputs": inputs}).to_string().into();
                self.runs.borrow_mut().push(run);
                Ok((201, json!({"id": id})))
            }
            _ => panic!("unexpected write {method} {url}"),
        }
    }

    fn write(&self, method: &str, url: &url::Url, body: &Value) -> Result<Value> {
        let (status, answer) = self.send(method, url, body)?;
        ensure!((200..300).contains(&status), "write refused {status}");
        Ok(answer)
    }
}

/// A Forgejo pass with epics in example/plans: `wip` slots, leases.
fn epic_config(wip: u64) -> Config {
    let mut value = file();
    value["board"]["url"] = URL.into();
    value["wip"] = wip.into();
    value["workRepositories"] = json!([WORK]);
    value["pools"] = json!({"claude": {}});
    value["planPools"] = json!(["claude"]);
    value["leases"] = true.into();
    value["epics"] = json!({"repository": PLANS, "close": true});
    config(&value)
}

fn pass(instance: &Instance, config: &Config, mode: Mode<'_>) -> Report {
    run(
        instance,
        &BrokerFake::default(),
        config,
        (mode, Limits::default()),
        usage::tests::now(),
    )
    .unwrap()
}

fn has(report: &Report, text: &str) -> bool {
    report.lines.iter().any(|l| l.contains(text))
}

/// A create with an implement job plan, its Priority, and a close of #7.
fn create_and_close() -> Value {
    json!([
        {"type": "create_sub_issue", "repo": WORK, "title": "Slice", "body": "Do it.",
         "plan": {"schema": "agent-board/job-plan/v1", "kind": "implement",
                  "runner": "hosted", "brief": "Implement the slice."}},
        {"type": "set_priority", "new": 1, "value": "P1"},
        {"type": "close_sub_issue", "issue": "example/work#7", "reason": "Superseded."}
    ])
}

#[test]
fn live_epic_evidence_and_agreeing_children() {
    // #8 is listed without a parent comment; #9 names the epic unlisted.
    let base = || {
        let instance = Instance::new();
        instance.issue(WORK, 8, OPERATOR, &["agent-board"]);
        instance.issue(WORK, 9, BOT, &["agent-board"]);
        let mut listed = instance.listed();
        listed.push(crate::epic::IssueRef::parse("example/work#8").unwrap());
        let list = crate::forgejo::epics::children_comment(EPIC, &listed);
        instance.comment(PLANS, EPIC, BOT, &list);
        let epic = crate::epic::IssueRef::parse("example/plans#3").unwrap();
        let parent = crate::forgejo::epics::parent_comment(&epic);
        instance.comment(WORK, 9, BOT, &parent);
        instance
    };
    let held = |report: &Report, issue: &str, kind: HoldKind| {
        (report.holds.iter()).any(|e| {
            e.hold.kind == kind
                && matches!(&e.scope, Scope::Item(i) | Scope::Epic(i) if i.display() == issue)
        })
    };
    for case in ["live", "visitor", "removed", "edited"] {
        let instance = base();
        match case {
            "visitor" => instance.labelled(PLANS, EPIC, 999, "epic", true),
            "removed" => instance.labelled(PLANS, EPIC, OPERATOR, "epic", false),
            "edited" => {
                let mut comments = instance.comments.borrow_mut();
                let thread = comments.get_mut(&at(PLANS, EPIC)).unwrap();
                thread.last_mut().unwrap()["updated_at"] = "2026-10-23T15:16:00Z".into();
            }
            _ => {}
        }
        let report = pass(&instance, &epic_config(5), Mode::Preview);
        // An epic whose relationship comment was edited is held, not live.
        let live = case == "live";
        assert!(
            has(
                &report,
                &format!("epics: {} live of 1 open", u8::from(live))
            ),
            "{case}: {:?}",
            report.lines
        );
        // A live epic's agreeing child is a candidate under it; a parent
        // comment the epic does not answer holds; one-sided #8 is not its.
        let agreeing = case == "live";
        assert_eq!(
            has(&report, "would start example/work#7"),
            agreeing,
            "{case}: {:?}",
            report.lines
        );
        assert_eq!(
            held(&report, "example/work#7", HoldKind::EpicNotLive),
            !agreeing,
            "{case}"
        );
        assert!(
            held(&report, "example/work#9", HoldKind::EpicNotLive),
            "{case}"
        );
        assert!(has(&report, "would start example/work#8"), "{case}");
        assert_eq!(
            held(&report, "example/plans#3", HoldKind::Unreadable),
            case == "edited",
            "{case}: {:?}",
            report.lines
        );
        // Never planned, the live epic is due; preview starts nothing.
        assert_eq!(
            has(&report, "start planning example/plans#3"),
            agreeing,
            "{case}"
        );
        assert!(instance.writes.borrow().is_empty(), "{case}");
    }
}

#[test]
fn apply_creates_links_prioritizes_closes_and_creates_nothing_twice() {
    let instance = Instance::new();
    let id = instance.plan(create_and_close());
    let config = epic_config(0);
    let preview = pass(&instance, &config, Mode::Preview);
    assert!(
        has(
            &preview,
            &format!(
                "apply epic example/plans#3 plan={id}@2026-10-23T15:15:10Z: Create a sub-issue in example/work (P1); Close example/work#7"
            )
        ),
        "{:?}",
        preview.lines
    );
    assert!(instance.writes.borrow().is_empty());
    let report = pass(&instance, &config, Mode::Local);
    assert_eq!(report.failures, 0, "{:?}", report.lines);
    let new = instance.listed().last().unwrap().number;
    assert_ne!(new, 7);
    assert!(
        has(&report, &format!("Created example/work#{new} (P1)")),
        "{:?}",
        report.lines
    );
    assert_eq!(instance.writes("POST repos/example/work/issues"), 1);
    assert_eq!(
        instance.label_names(WORK, new),
        ["agent-board", "priority/P1"]
    );
    let names: Vec<String> = (instance.listed().iter())
        .map(ToString::to_string)
        .collect();
    assert_eq!(names, ["example/work#7", &format!("example/work#{new}")]);
    let child = instance.thread(WORK, new);
    assert!(
        child[0]["body"]
            .as_str()
            .unwrap()
            .starts_with("<!-- agent-board-epic-parent: example/plans#3 -->")
    );
    assert!(
        child[1]["body"]
            .as_str()
            .unwrap()
            .contains(&format!("\"item\": {new}"))
    );
    assert_eq!(instance.state(WORK, 7), "closed");
    assert_eq!(
        instance.thread(WORK, 7).last().unwrap()["body"],
        "Superseded."
    );
    let receipt = instance.receipt();
    assert_eq!(receipt["outcome"], "applied");
    assert_eq!(
        receipt["actions"][0]["writes"],
        json!(["create", "link", "priority", "comment"])
    );
    assert_eq!(receipt["actions"][1]["writes"], json!(["folded"]));
    assert_eq!(receipt["actions"][2]["writes"], json!(["comment", "patch"]));
    let writes = instance.writes.borrow().len();
    for _ in 0..2 {
        let again = pass(&instance, &config, Mode::Local);
        assert_eq!(again.failures, 0, "{:?}", again.lines);
        assert!(
            has(&again, "applied; nothing changed since"),
            "{:?}",
            again.lines
        );
    }
    assert_eq!(instance.writes.borrow().len(), writes);
}

#[test]
fn stale_foreign_edited_and_unsupported_plans_write_nothing() {
    for case in [
        "edited",
        "visitor",
        "unsupported",
        "foreign-issue",
        "closed-epic",
    ] {
        let instance = Instance::new();
        let id = match case {
            "unsupported" => instance.plan(json!([
                {"type": "set_priority", "issue": "example/work#7", "value": "P0"},
                {"type": "rescope_sub_issue", "issue": "example/work#7", "body": "New.",
                 "reason": "Narrower."}])),
            "foreign-issue" => instance.plan(json!([{"type": "close_sub_issue",
                "issue": "example/work#8", "reason": "Not ours."}])),
            _ => instance.plan(create_and_close()),
        };
        match case {
            "edited" => {
                let mut comments = instance.comments.borrow_mut();
                let plan = (comments.get_mut(&at(PLANS, EPIC)).unwrap().iter_mut())
                    .find(|c| c["id"] == id)
                    .unwrap();
                plan["updated_at"] = "2026-10-23T15:16:00Z".into();
            }
            "visitor" => {
                let mut comments = instance.comments.borrow_mut();
                let plan = (comments.get_mut(&at(PLANS, EPIC)).unwrap().iter_mut())
                    .find(|c| c["id"] == id)
                    .unwrap();
                plan["user"]["id"] = 999.into();
            }
            "closed-epic" => {
                instance
                    .issues
                    .borrow_mut()
                    .get_mut(&at(PLANS, EPIC))
                    .unwrap()["state"] = "closed".into();
            }
            _ => {}
        }
        let report = pass(&instance, &epic_config(0), Mode::Local);
        assert!(
            instance.writes.borrow().is_empty(),
            "{case}: {:?}",
            report.lines
        );
        let hold = |kind: HoldKind| {
            (report.holds.iter()).any(|e| {
                e.hold.kind == kind
                    && e.scope
                        == Scope::Epic(ItemRef {
                            host: URL.into(),
                            repository: PLANS.into(),
                            number: EPIC,
                        })
            })
        };
        match case {
            "edited" => assert!(hold(HoldKind::Unreadable), "{case}: {:?}", report.lines),
            // A visitor's plan is no plan: due, but wip 0 plans nothing.
            "visitor" => assert!(
                has(&report, "never been planned"),
                "{case}: {:?}",
                report.lines
            ),
            "unsupported" => {
                assert!(hold(HoldKind::NeedsOperator), "{case}: {:?}", report.lines);
                assert!(
                    (report.holds.iter())
                        .any(|e| e.hold.reason.contains("no native Forgejo capability")),
                    "{case}"
                );
            }
            "foreign-issue" => assert!(hold(HoldKind::NeedsOperator), "{case}"),
            // A closed epic is no live epic, and its child's parent
            // comment holds the child.
            "closed-epic" => {
                assert!(
                    has(&report, "epics: 0 live of 0 open"),
                    "{case}: {:?}",
                    report.lines
                );
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn closed_child_is_satisfied_and_finished_epic_closes() {
    let instance = Instance::new();
    instance.issues.borrow_mut().get_mut(&at(WORK, 7)).unwrap()["state"] = "closed".into();
    instance.plan(
        json!([{"type": "close_sub_issue", "issue": "example/work#7",
        "reason": "Done."}]),
    );
    let config = epic_config(0);
    let report = pass(&instance, &config, Mode::Local);
    assert_eq!(report.failures, 0, "{:?}", report.lines);
    assert!(
        has(&report, "example/work#7 was already closed"),
        "{:?}",
        report.lines
    );
    assert!(
        instance.thread(WORK, 7).len() == 1,
        "no reason comment on a closed child"
    );
    assert_eq!(
        instance.receipt()["actions"][0]["writes"],
        json!(["satisfied"])
    );
    // Every child closed and never planned: with `epics.close`, closed
    // after a comment; without, held for the operator.
    for close in [false, true] {
        let instance = Instance::new();
        instance.issues.borrow_mut().get_mut(&at(WORK, 7)).unwrap()["state"] = "closed".into();
        let mut config = epic_config(0);
        config.epics.as_mut().unwrap().close = close;
        let report = pass(&instance, &config, Mode::Local);
        assert_eq!(
            has(
                &report,
                "close epic example/plans#3: every sub-issue is closed"
            ),
            close,
            "{:?}",
            report.lines
        );
        let state = if close { "closed" } else { "open" };
        assert_eq!(instance.state(PLANS, EPIC), state);
        assert_eq!(instance.writes.borrow().len(), if close { 2 } else { 0 });
    }
}

#[test]
fn failure_part_way_then_recovery() {
    let instance = Instance::new();
    instance.plan(create_and_close());
    let config = epic_config(0);
    instance.fault.set("close");
    let report = pass(&instance, &config, Mode::Local);
    assert_eq!(report.failures, 1, "{:?}", report.lines);
    assert_eq!(instance.receipt()["outcome"], "stopped");
    instance.fault.set("");
    // Not proved while #7 is open: held, and nothing is written again.
    let writes = instance.writes.borrow().len();
    let report = pass(&instance, &config, Mode::Local);
    assert!(
        (report.holds.iter()).any(|e| e.hold.kind == HoldKind::NeedsOperator
            && e.hold.reason.contains("example/work#7 is open")),
        "{:?}",
        report.lines
    );
    assert_eq!(instance.writes.borrow().len(), writes);
    // A person closes it: the next pass proves the apply and says so once.
    instance.issues.borrow_mut().get_mut(&at(WORK, 7)).unwrap()["state"] = "closed".into();
    let report = pass(&instance, &config, Mode::Local);
    assert!(
        has(&report, "recover epic example/plans#3"),
        "{:?}",
        report.lines
    );
    assert_eq!(instance.receipt()["outcome"], "recovered");
    let writes = instance.writes.borrow().len();
    let report = pass(&instance, &config, Mode::Local);
    assert!(
        has(&report, "applied; nothing changed since"),
        "{:?}",
        report.lines
    );
    assert_eq!(instance.writes.borrow().len(), writes);
    assert_eq!(instance.writes("POST repos/example/work/issues"), 1);
}

#[test]
fn receipt_cut_short_is_proved_from_the_issues() {
    let instance = Instance::new();
    instance.plan(create_and_close());
    let config = epic_config(0);
    instance.fault.set("final-receipt");
    let report = pass(&instance, &config, Mode::Local);
    assert_eq!(report.failures, 1, "{:?}", report.lines);
    instance.fault.set("");
    // Its last receipt names the new issue's create alone.
    let receipt = instance.receipt();
    assert_eq!(receipt["outcome"], "claimed");
    assert_eq!(receipt["actions"][0]["writes"], json!(["create"]));
    // Linked, prioritized, its job plan posted and #7 closed: proved.
    let report = pass(&instance, &config, Mode::Local);
    assert!(
        has(&report, "recover epic example/plans#3"),
        "{:?}",
        report.lines
    );
    assert_eq!(instance.receipt()["outcome"], "recovered");
    assert_eq!(instance.writes("POST repos/example/work/issues"), 1);
}

#[test]
fn ambiguous_create_holds_and_is_never_retried() {
    let instance = Instance::new();
    instance.plan(create_and_close());
    let config = epic_config(0);
    instance.fault.set("create-lost");
    pass(&instance, &config, Mode::Local);
    instance.fault.set("");
    let receipt = instance.receipt();
    assert_eq!(receipt["outcome"], "stopped");
    assert!(receipt["actions"][0]["issue"].is_null());
    for _ in 0..2 {
        let report = pass(&instance, &config, Mode::Local);
        assert!(
            (report.holds.iter()).any(|e| e.hold.reason.contains("no confirmed issue")),
            "{:?}",
            report.lines
        );
    }
    assert_eq!(instance.writes("POST repos/example/work/issues"), 1);
    assert_eq!(instance.state(WORK, 7), "open");
}

#[test]
fn planning_run_starts_through_the_claim_path_once() {
    let instance = Instance::new();
    let config = epic_config(2);
    let report = pass(&instance, &config, Mode::Local);
    assert_eq!(report.failures, 0, "{:?}", report.lines);
    assert!(
        has(
            &report,
            "start planning example/plans#3 pool=claude: it has never been planned"
        ),
        "{:?}",
        report.lines
    );
    assert_eq!(instance.writes(DISPATCH), 1);
    let run = &instance.runs.borrow()[0];
    assert_eq!(run["title"], "triage example/plans#3 (claude)");
    let thread = instance.thread(PLANS, EPIC);
    let bodies: Vec<&str> = thread.iter().map(|c| c["body"].as_str().unwrap()).collect();
    assert!(bodies.iter().any(|b| b.contains("\"kind\": \"triage\"")));
    assert!(
        bodies
            .iter()
            .any(|b| b.starts_with("<!-- agent-board-dispatch: "))
    );
    assert!(bodies.iter().any(|b| b.contains("\"event\":\"started\"")));
    // In flight, the epic waits for its plan; nothing is posted again.
    let writes = instance.comments.borrow()[&at(PLANS, EPIC)].len();
    let report = pass(&instance, &config, Mode::Local);
    assert!(
        has(&report, "its planning run is in flight"),
        "{:?}",
        report.lines
    );
    assert_eq!(instance.writes(DISPATCH), 1);
    assert_eq!(instance.comments.borrow()[&at(PLANS, EPIC)].len(), writes);
}

mod sweep;
