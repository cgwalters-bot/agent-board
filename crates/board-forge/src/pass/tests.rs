use super::*;

const CONFIG: &str = include_str!("../../../../fixtures/pass/config.json");
/// The live board read on 23 October 2026 by `pass --record`, with a
/// synthetic usage snapshot (the live one is private): claude under its
/// pace, openai held over it. Stage 3's reads of the live epics tracker#596,
/// #766 and #890 were recorded that evening, seeded with this recording,
/// and appended to it ([`epics`]'s tests replay them at that time).
const BOARD: &str = include_str!("../../../../fixtures/pass/board-2026-10-23.json");

fn at(stamp: &str) -> OffsetDateTime {
    OffsetDateTime::parse(stamp, &Rfc3339).unwrap()
}

pub(super) fn replay() -> Replay {
    Replay::from_json(&serde_json::from_str(BOARD).unwrap()).unwrap()
}

/// `replay` with no open epic for stage 3 to read, for tests of the other
/// stages; the board's children still qualify through their epics.
pub(super) fn without_epics(mut replay: Replay) -> Replay {
    let variables = json!({"owner": "cgwalters-forge", "name": "tracker", "label": EPIC_LABEL,
        "field": "Priority", "cursor": null});
    let request = graphql_key(epics::EPICS_QUERY, &variables);
    let none = json!({"data": {"repository": {"issues": {"nodes": [],
        "pageInfo": {"hasNextPage": false, "endCursor": null}}}}});
    replay.requests.retain(|e| e["request"] != request);
    replay
        .requests
        .push(json!({"request": request, "answer": none}));
    replay
}

pub(super) fn config(edit: Value) -> Config {
    let mut value: Value = serde_json::from_str(CONFIG).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .extend(edit.as_object().unwrap().clone());
    Config::parse(&value.to_string()).unwrap()
}

pub(super) fn preview(config: &Config, replay: &Replay) -> Report {
    run(
        replay,
        replay,
        config,
        State::default(),
        (Mode::Preview, Limits::default()),
        replay.now,
    )
    .unwrap()
}

/// The recorded answer to the request whose JSON contains `needle`.
pub(super) fn answer<'a>(replay: &'a mut Replay, needle: &str) -> &'a mut Value {
    replay
        .requests
        .iter_mut()
        .find(|e| e["request"].to_string().contains(needle))
        .map(|e| &mut e["answer"])
        .unwrap_or_else(|| panic!("no recorded request has {needle}"))
}

pub(super) fn usage(replay: &mut Replay) -> Value {
    let body = &answer(replay, "bot-ops/issues/1/comments")["body"][0]["body"];
    usage_snapshot(body.as_str().unwrap()).unwrap()
}

pub(super) fn set_usage(replay: &mut Replay, snapshot: &Value) {
    let body = format!("{USAGE_MARKER}\n\n```json\n{snapshot:#}\n```\n");
    answer(replay, "bot-ops/issues/1/comments")["body"][0]["body"] = body.into();
}

const STARTED: &[&str] = &[
    "start triage cgwalters-forge/tracker#446 pool=claude",
    "start triage cgwalters-forge/tracker#459 pool=claude",
    "hold cgwalters-forge/agentic-job#188: plan 6229359427@2026-10-17T03:59:02Z was dispatched already, https://github.com/cgwalters-forge/agentic-job/issues/188#issuecomment-6229434409",
    "hold cgwalters-forge/tracker#468: plan 6332112108@2026-10-22T03:40:56Z was dispatched already, https://github.com/cgwalters-forge/tracker/issues/468#issuecomment-6332216967",
    "hold cgwalters-forge/tracker#527: Waiting: open pull request cgwalters-forge/tracker#832 changes `README.md`.",
    "start triage cgwalters-forge/tracker#533 pool=claude",
    "start triage cgwalters-forge/agent-board#40 pool=claude",
    "start triage cgwalters-forge/agent-board#44 pool=claude",
    "start triage cgwalters-forge/tracker#540 pool=claude",
    "wip: no slot left; 185 more candidate(s) wait",
];

/// The last line of scheduling, before the Status stage's.
fn last_scheduled(report: &Report) -> &str {
    let lines = report.lines.iter().rev();
    lines
        .map(String::as_str)
        .find(|l| !l.starts_with("status"))
        .unwrap()
}

/// Stage 3 on the recorded board: tracker#596, whose applied plan's
/// sub-issues have closed since, is planned; #890's changes wait three hours
/// after its last plan; #766 is held by its label.
pub(super) const EPIC_LINES: &[&str] = &[
    "epics: 3 live of 3 open in cgwalters-forge/tracker",
    "start planning cgwalters-forge/tracker#596 pool=claude: its sub-issues changed (cgwalters-forge/agentic-job#630 closed, cgwalters-forge/agentic-job#715 closed, cgwalters-forge/agentic-job#716 closed and 3 more)",
    "hold epic cgwalters-forge/tracker#890: its sub-issues changed (cgwalters-forge/agent-board#285 closed, cgwalters-forge/agent-board#286 closed, cgwalters-forge/agent-board#287 closed and 5 more); cgwalters-forge/agent-board#284 edited; cgwalters-forge/agent-board#289's Priority is P0, not P2; planned again from 2026-10-23T16:34:49Z, three hours after its last request",
    "hold epic cgwalters-forge/tracker#766: labelled needs-human; otherwise due to be planned (its sub-issues changed (cgwalters-forge/actions#83 closed, cgwalters-forge/agent-board#167 closed, cgwalters-forge/agent-board#168 closed and 2 more))",
];

/// The idle board of 23 October: two runs in flight of eight, a usable
/// pool, so six runs start: an epic's planning run, then five in Priority
/// order. Zero would be a regression. Recorded plans and an overlap behind
/// an open pull request hold.
#[test]
fn replay_idle_board_starts_work() {
    let report = preview(&config(json!({})), &replay());
    let mut expected = vec![
        "follow: no repositories",
        "usage: not refreshed by this build; pacing reads the published snapshot",
    ];
    expected.extend(EPIC_LINES);
    expected.push("candidates: 194 (0 opened by others, 107 outside a live epic)");
    // The planning run took the last slot stage 4 had.
    expected.extend(&STARTED[..STARTED.len() - 2]);
    expected.push("wip: no slot left; 186 more candidate(s) wait");
    // The recording's whole-board read is an empty board: the Status stage
    // has its own recording.
    expected.push("status: 0 change(s) of 0 item(s) read");
    assert_eq!(report.lines, expected);
    assert_eq!(report.failures, 0);
}

/// Keeps only `issues` (`OWNER/REPO#N`) and the epics on every recorded board page,
/// so a scenario that scans past what the recording read stops where it does,
/// and children keep their epics' Priority.
fn only(replay: &mut Replay, issues: &[&str]) {
    for entry in &mut replay.requests {
        let Some(nodes) = entry["answer"]
            .pointer_mut("/data/repositoryOwner/projectV2/items/nodes")
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        nodes.retain(|n| {
            let content = &n["content"];
            let name = format!(
                "{}#{}",
                text(content, "/repository/nameWithOwner"),
                content["number"]
            );
            let epic = content["labels"]["nodes"]
                .as_array()
                .is_some_and(|l| l.iter().any(|l| l["name"] == EPIC_LABEL));
            epic || issues.contains(&name.as_str())
        });
    }
}

/// The candidates of the recording, in the order the pass takes them, up to
/// where it stops with every slot taken.
const FIRST: &[&str] = &[
    "cgwalters-forge/tracker#446",
    "cgwalters-forge/tracker#459",
    "cgwalters-forge/agentic-job#188",
    "cgwalters-forge/tracker#468",
    "cgwalters-forge/tracker#527",
    "cgwalters-forge/tracker#533",
    "cgwalters-forge/agent-board#40",
    "cgwalters-forge/agent-board#44",
    "cgwalters-forge/tracker#540",
];

fn in_flight(replay: &mut Replay, runs: Vec<Value>) {
    let body = &mut answer(replay, "status=queued")["body"];
    body["total_count"] = runs.len().into();
    body["workflow_runs"] = runs.into();
}

fn runs(n: u64) -> Vec<Value> {
    (1..=n)
        .map(|i| json!({"id": i, "display_title": format!("implement o/r#{i}"), "html_url": format!("https://github.com/o/r/actions/runs/{i}")}))
        .collect()
}

/// Gives tracker#446 twelve open closing pull requests, eleven a visitor's
/// and the last `last`'s: ten on the board's page, all on the page read after.
fn closing_pulls(replay: &mut Replay, last: &str) {
    let pull = |n: u64, author: &str| {
        json!({"number": n, "state": "OPEN", "repository": {"nameWithOwner": "cgwalters-forge/tracker"},
            "author": {"login": author}})
    };
    let mut pulls: Vec<Value> = (1..=11).map(|n| pull(n, "visitor")).collect();
    for entry in &mut replay.requests {
        let nodes = entry["answer"]
            .pointer_mut("/data/repositoryOwner/projectV2/items/nodes")
            .and_then(Value::as_array_mut);
        for node in nodes.into_iter().flatten() {
            let content = &mut node["content"];
            if content["number"] == 446
                && text(content, "/repository/nameWithOwner").ends_with("/tracker")
            {
                content["closedByPullRequestsReferences"] =
                    json!({"nodes": pulls[..10], "totalCount": 12});
            }
        }
    }
    pulls.push(pull(12, last));
    let variables =
        json!({"owner": "cgwalters-forge", "name": "tracker", "number": 446, "cursor": null});
    replay.requests.push(json!({
        "request": graphql_key(PULLS_QUERY, &variables),
        "answer": {"data": {"repository": {"issue": {"closedByPullRequestsReferences": {
            "nodes": pulls, "totalCount": 12, "pageInfo": {"hasNextPage": false, "endCursor": null},
        }}}}},
    }));
}

type Edit = Box<dyn Fn(&mut Replay, &mut Value)>;

/// Each edit of the recording, the lines the pass must then print (each the
/// start of one), how many runs it starts and how many operations fail.
#[test]
fn replay_scenarios() {
    let cases: Vec<(&str, Edit, Vec<&str>, usize, usize)> = vec![
        (
            "full WIP: eight runs with the two in progress",
            Box::new(|r, _| in_flight(r, runs(6))),
            vec!["hold wip: 8 of 8 runs active"],
            0,
            0,
        ),
        (
            "an excess is not cancelled and leaves no slot",
            Box::new(|r, _| in_flight(r, runs(9))),
            vec!["hold wip: 11 of 8 runs active"],
            0,
            0,
        ),
        (
            "a run list that may be cut short is no capacity",
            Box::new(|r, _| {
                answer(r, "status=waiting")["body"]["total_count"] = 101.into();
            }),
            vec![
                "schedule error: more waiting runs than one page lists; capacity is unknown",
                "status: the runs in flight could not be listed, so none goes back to Todo",
            ],
            0,
            1,
        ),
        (
            "a run in flight holds its issue and its slot",
            Box::new(|r, _| {
                in_flight(
                    r,
                    vec![
                        json!({"id": 7, "display_title": "triage cgwalters-forge/tracker#446 (claude)", "html_url": "https://github.com/cgwalters-forge/tracker/actions/runs/7"}),
                    ],
                )
            }),
            vec![
                "hold cgwalters-forge/tracker#446: a run is in flight, https://github.com/cgwalters-forge/tracker/actions/runs/7",
                "start triage cgwalters-forge/tracker#459 pool=claude",
                "wip: no slot left; 185 more candidate(s) wait",
            ],
            5,
            0,
        ),
        (
            "missing usage holds every new run, a cold start too",
            Box::new(|r, _| {
                answer(r, "bot-ops/issues/1/comments")["body"] = json!([]);
            }),
            vec!["hold pools: there is no usage snapshot; new runs wait"],
            0,
            0,
        ),
        (
            "an unreadable usage snapshot is a failure, never room",
            Box::new(|r, _| {
                answer(r, "bot-ops/issues/1/comments")["status"] = 404.into();
            }),
            vec![
                "usage error: reading the usage snapshot: GitHub HTTP status 404",
                "hold pools: there is no usage snapshot; new runs wait",
            ],
            0,
            1,
        ),
        (
            "a snapshot older than 45 minutes is stale",
            Box::new(|r, _| r.now += time::Duration::minutes(45)),
            vec!["hold pools: the usage snapshot is stale"],
            0,
            0,
        ),
        (
            "both pools over pace: wait, no fall back to a held pool",
            Box::new(|r, _| {
                let mut snapshot = usage(r);
                snapshot["pools"]["claude"]["hold"] = true.into();
                set_usage(r, &snapshot);
            }),
            vec!["hold pools: claude is held; openai is held until"],
            0,
            0,
        ),
        (
            "the pool furthest under its pace takes the run",
            Box::new(|r, _| {
                let mut snapshot = usage(r);
                snapshot["pools"]["openai"]["hold"] = false.into();
                snapshot["pools"]["openai"]["used_percent"] = 0.into();
                set_usage(r, &snapshot);
            }),
            vec!["start triage cgwalters-forge/tracker#446 pool=openai"],
            6,
            0,
        ),
        (
            "more than ten visitors' pull requests do not hold an issue",
            Box::new(|r, _| {
                only(r, FIRST);
                closing_pulls(r, "visitor");
            }),
            vec!["start triage cgwalters-forge/tracker#446 pool=claude"],
            6,
            0,
        ),
        (
            "a trusted pull request past the first ten holds it",
            Box::new(|r, _| {
                only(r, FIRST);
                closing_pulls(r, "cgwalters-bot");
            }),
            vec![
                "hold cgwalters-forge/tracker#446: pull request cgwalters-forge/tracker#12 is open",
            ],
            5,
            0,
        ),
        (
            "triage off: an issue without a plan waits",
            Box::new(|r, c| {
                only(r, &["cgwalters-forge/tracker#446"]);
                c["triage"] = false.into();
            }),
            vec![
                "candidates: 1 (0 opened by others, 0 outside a live epic)",
                "hold cgwalters-forge/tracker#446: no plan, and triage is off",
                "idle: 6 slot(s) free and nothing started",
            ],
            0,
            0,
        ),
        (
            "an invalid newest plan is reported, never read as no plan",
            Box::new(|r, _| {
                only(r, &["cgwalters-forge/tracker#446"]);
                let comments = &mut answer(r, r#""name":"tracker","number":446"#)["data"]["repository"]
                    ["issue"]["comments"]["nodes"];
                comments.as_array_mut().unwrap().push(json!({
                    "url": "https://github.com/cgwalters-forge/tracker/issues/446#issuecomment-9",
                    "body": "```agent-board-plan\n{\"schema\": \"agent-board/job-plan/v1\"}\n```",
                    "createdAt": "2026-10-23T00:00:00Z", "lastEditedAt": null,
                    "author": {"login": "cgwalters"}, "editor": null,
                }));
            }),
            vec![
                "hold cgwalters-forge/tracker#446: its newest plan is invalid: newest plan https://github.com/cgwalters-forge/tracker/issues/446#issuecomment-9: plan has no repo",
            ],
            0,
            0,
        ),
        (
            "a visitor's issue outside any epic does not qualify",
            Box::new(|r, _| {
                only(r, &["cgwalters-forge/tracker#446"]);
                for entry in &mut r.requests {
                    let nodes = entry["answer"]
                        .pointer_mut("/data/repositoryOwner/projectV2/items/nodes")
                        .and_then(Value::as_array_mut);
                    for node in nodes.into_iter().flatten() {
                        if node["content"]["number"] == 446 {
                            node["content"]["author"]["login"] = "visitor".into();
                            node["content"]["parent"] = Value::Null;
                        }
                    }
                }
            }),
            vec!["candidates: 0 (1 opened by others, 0 outside a live epic)"],
            0,
            0,
        ),
        (
            "a board page that does not say whether more follow is no board",
            Box::new(|r, _| {
                let page = answer(r, r#""cursor":null"#);
                page["data"]["repositoryOwner"]["projectV2"]["items"]["pageInfo"] = json!({});
            }),
            vec!["schedule error: board items have no pageInfo"],
            0,
            1,
        ),
        (
            "a redacted board item makes the board incomplete",
            Box::new(|r, _| {
                let page = answer(r, r#""cursor":null"#);
                page["data"]["repositoryOwner"]["projectV2"]["items"]["nodes"][0]["content"] =
                    Value::Null;
            }),
            vec!["schedule error: a board item could not be read; the board is incomplete"],
            0,
            1,
        ),
        (
            "no work repositories: nothing is read or started",
            Box::new(|_, c| c["workRepositories"] = json!([])),
            vec!["schedule: off (wip 0 or no workRepositories)"],
            0,
            0,
        ),
    ];
    for (name, edit, lines, starts, failures) in cases {
        let mut replay = without_epics(replay());
        let mut settings = json!({});
        edit(&mut replay, &mut settings);
        let report = preview(&config(settings), &replay);
        for line in &lines {
            assert!(
                report.lines.iter().any(|l| l.starts_with(line)),
                "{name}: no line {line:?} in {:#?}",
                report.lines
            );
        }
        let started = report
            .lines
            .iter()
            .filter(|l| l.starts_with("start "))
            .count();
        assert_eq!(started, starts, "{name}: {:#?}", report.lines);
        assert_eq!(report.failures, failures, "{name}: {:#?}", report.lines);
    }
}

/// The recording with writes: comments posted and edited show in later
/// reads, and dispatches are kept. `fail` loses the response to the first
/// request whose method and URL, or query, contain it: a write is made and
/// its answer lost, a read fails. With `after_record` the first comments
/// read after a dispatch record is posted fails instead. An issue read again
/// before a start is its board item's facts. `late` edits each answer, by its
/// request, once a run is dispatched: what changed since the board was read.
struct Forge {
    replay: Replay,
    added: RefCell<BTreeMap<(String, u64), Vec<Value>>>,
    writes: RefCell<Vec<(String, String, Value)>>,
    fail: RefCell<Option<String>>,
    after_record: bool,
    ids: Cell<u64>,
    login: &'static str,
    late: Option<Late>,
    /// Other runs of the pass's own workflow in progress ([`own`]).
    passes: Vec<Value>,
}

/// The workflow run an applying pass runs in, a run of workflow 77.
fn own() -> PassRun {
    PassRun {
        repository: "cgwalters-forge/tracker".into(),
        id: 4242,
    }
}

fn listed(body: Value) -> RestResponse {
    RestResponse {
        status: 200,
        etag: None,
        last_modified: None,
        link: None,
        body,
    }
}

/// An edit of an answer by its request ([`Forge`]'s `late`).
type Late = Box<dyn Fn(&str, &mut Value)>;

impl Forge {
    fn new(fail: Option<&str>, after_record: bool) -> Self {
        let mut replay = without_epics(replay());
        only(&mut replay, FIRST);
        Self {
            replay,
            added: RefCell::new(BTreeMap::new()),
            writes: RefCell::new(vec![]),
            fail: RefCell::new(fail.map(str::to_owned)),
            after_record,
            ids: Cell::new(9_000_000_000),
            login: "cgwalters-bot",
            late: None,
            passes: vec![],
        }
    }

    fn late(&self, request: &str, answer: &mut Value) {
        if let Some(late) = self.late.as_ref().filter(|_| !self.dispatches().is_empty()) {
            late(request, answer);
        }
    }

    /// The board item's issue, on the board with its Status.
    fn fresh(&self, variables: &Value) -> Result<Value> {
        let name = format!("{}/{}", text(variables, "/owner"), text(variables, "/name"));
        let node = self
            .replay
            .requests
            .iter()
            .filter_map(|e| {
                e["answer"]
                    .pointer("/data/repositoryOwner/projectV2/items/nodes")
                    .and_then(Value::as_array)
            })
            .flatten()
            .find(|n| {
                text(n, "/content/repository/nameWithOwner") == name
                    && n["content"]["number"] == variables["number"]
            })
            .with_context(|| format!("{name}#{} is not on the board", variables["number"]))?;
        let mut issue = node["content"].clone();
        issue["projectItems"] = json!({"nodes": [{
            "project": {"number": 1, "owner": {"login": "cgwalters-forge"}},
            "status": node["status"],
        }], "totalCount": 1});
        Ok(json!({"repository": {"issue": issue}}))
    }

    fn failing(&self, what: &str) -> Result<()> {
        let mut fail = self.fail.borrow_mut();
        if fail.as_deref().is_some_and(|f| what.contains(f)) {
            *fail = None;
            bail!(
                "connection reset during {}",
                what.lines().next().unwrap_or("")
            );
        }
        Ok(())
    }

    fn dispatches(&self) -> Vec<String> {
        self.writes
            .borrow()
            .iter()
            .filter(|(_, url, _)| url.ends_with("/dispatches"))
            .map(|(_, _, body)| {
                let inputs = &body["inputs"];
                format!("{}#{}", text(inputs, "/repo"), text(inputs, "/item"))
            })
            .collect()
    }

    fn comments_on(&self, repo: &str, number: u64) -> Vec<String> {
        let added = self.added.borrow();
        let nodes = added
            .get(&(repo.into(), number))
            .cloned()
            .unwrap_or_default();
        nodes.iter().map(|n| text(n, "/body").to_owned()).collect()
    }

    fn write(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        self.writes
            .borrow_mut()
            .push((method.into(), url.into(), body.clone()));
        let path = url.trim_start_matches(API_ROOT);
        let id = self.ids.get() + 1;
        self.ids.set(id);
        if method == "POST" && path.ends_with("/comments") {
            let parts: Vec<&str> = path.split('/').collect();
            let (repo, number) = (format!("{}/{}", parts[1], parts[2]), parts[4].parse()?);
            let html_url = format!("https://github.com/{repo}/issues/{number}#issuecomment-{id}");
            let created =
                (self.replay.now + time::Duration::seconds((id % 1000) as i64)).format(&Rfc3339)?;
            let text = text(body, "/body");
            if self.after_record && text.starts_with("<!-- agent-board-dispatch") {
                *self.fail.borrow_mut() = Some("comments(first: 100".into());
            }
            self.added
                .borrow_mut()
                .entry((repo, number))
                .or_default()
                .push(json!({
                    "id": id, "url": html_url, "body": text, "createdAt": created,
                    "lastEditedAt": null, "author": {"login": "cgwalters-bot"}, "editor": null,
                }));
            return Ok((201, json!({"id": id, "html_url": html_url})));
        }
        if method == "PATCH" {
            let edited: u64 = path.rsplit('/').next().unwrap_or_default().parse()?;
            for nodes in self.added.borrow_mut().values_mut() {
                for node in nodes.iter_mut().filter(|n| n["id"] == edited) {
                    node["body"] = body["body"].clone();
                }
            }
            return Ok((200, json!({})));
        }
        if path.ends_with("/dispatches") {
            return Ok((200, json!({"workflow_run_id": id})));
        }
        bail!("unexpected write {method} {url}")
    }
}

impl Rest for Forge {
    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
        self.failing(&format!("GET {url}"))?;
        let runs = format!("{API_ROOT}repos/cgwalters-forge/tracker/actions/");
        if url == format!("{runs}runs/4242") {
            return Ok(listed(json!({"id": 4242, "workflow_id": 77})));
        }
        if url == format!("{runs}workflows/77/runs?status=in_progress&per_page=100") {
            let mut runs = vec![json!({"id": 4242})];
            runs.extend(self.passes.iter().cloned());
            let body = json!({"total_count": runs.len(), "workflow_runs": runs});
            return Ok(listed(body));
        }
        let mut response = self.replay.get(url, validator)?;
        self.late(url, &mut response.body);
        Ok(response)
    }

    fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        let written = self.write(method, url, body)?;
        self.failing(&format!("{method} {url}"))?;
        Ok(written)
    }
}

impl Transport for Forge {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.failing(&format!("graphql {query}"))?;
        if query.contains("viewer {") {
            return Ok(json!({"viewer": {"login": self.login}}));
        }
        if query.starts_with(FRESH_QUERY) {
            let mut data = self.fresh(&variables)?;
            self.late(&variables.to_string(), &mut data);
            return Ok(data);
        }
        let mut data = self.replay.graphql(query, variables.clone())?;
        if let Some(nodes) = data
            .pointer_mut("/repository/issue/comments/nodes")
            .and_then(Value::as_array_mut)
        {
            let repo = format!(
                "{}/{}",
                text(&variables, "/owner"),
                text(&variables, "/name")
            );
            let number = variables["number"].as_u64().unwrap_or_default();
            nodes.extend(
                self.added
                    .borrow()
                    .get(&(repo, number))
                    .cloned()
                    .unwrap_or_default(),
            );
        }
        Ok(data)
    }
}

fn apply(forge: &Forge, config: &Config) -> Report {
    run(
        forge,
        forge,
        config,
        State::default(),
        (Mode::Actions(&own()), Limits::default()),
        forge.replay.now,
    )
    .unwrap()
}

/// Two passes compete for the last slot, each with a different plan: the
/// one that sees the other in progress starts nothing, as the other may
/// take the slot it saw free; alone, it takes the slot.
#[test]
fn apply_starts_nothing_while_another_pass_is_in_progress() {
    let config = config(json!({"wip": 3}));
    let other = json!({"id": 4243, "html_url": "https://github.com/cgwalters-forge/tracker/actions/runs/4243"});
    let mut forge = Forge::new(None, false);
    forge.passes = vec![other];
    let report = apply(&forge, &config);
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    assert_eq!(
        last_scheduled(&report),
        "hold wip: another pass is in progress, https://github.com/cgwalters-forge/tracker/actions/runs/4243; it takes the free slots"
    );
    assert!(forge.writes.borrow().is_empty());
    let forge = Forge::new(None, false);
    let report = apply(&forge, &config);
    assert_eq!(forge.dispatches(), ["cgwalters-forge/tracker#446"]);
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
}

/// A pass past its time bound starts nothing more, and sends nothing; a
/// start under way when it passed may finish, for a while.
#[test]
fn apply_stops_at_its_time_bound() {
    let config = config(json!({}));
    let forge = Forge::new(None, false);
    let past = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let report = run_within(
        &forge,
        &forge,
        &config,
        (
            State::default(),
            Mode::Actions(&own()),
            Limits::default(),
            forge.replay.now,
        ),
        &Bounds::new(PASS_REQUEST_LIMIT, past),
    )
    .unwrap();
    assert_eq!(
        last_scheduled(&report),
        "schedule error: the pass's time bound (900 seconds) is spent"
    );
    // Nor does stage 3 read the epics.
    assert!(
        report
            .lines
            .contains(&"epics error: the pass's time bound (900 seconds) is spent".to_owned())
    );
    // Nor does the Status stage read the board.
    let last = report.lines.last().unwrap();
    assert!(
        last.starts_with("status error: reading the whole board: ")
            && last.ends_with("the pass's time bound (900 seconds) is spent"),
        "{last}"
    );
    assert_eq!(report.failures, 3);
    assert!(forge.writes.borrow().is_empty());
}

/// An applying pass starts what the preview names, each with the triage
/// plan and the dispatch record on its issue; a second pass over the same
/// forge starts none of them again.
#[test]
fn apply_starts_once_and_reruns_start_nothing_twice() {
    let config = config(json!({}));
    let forge = Forge::new(None, false);
    let first = apply(&forge, &config);
    assert_eq!(first.failures, 0, "{:#?}", first.lines);
    assert_eq!(
        forge.dispatches(),
        [
            "cgwalters-forge/tracker#446",
            "cgwalters-forge/tracker#459",
            "cgwalters-forge/tracker#533",
            "cgwalters-forge/agent-board#40",
            "cgwalters-forge/agent-board#44",
            "cgwalters-forge/tracker#540",
        ]
    );
    let line = first
        .lines
        .iter()
        .find(|l| l.contains("tracker#446"))
        .unwrap();
    assert!(
        line.starts_with("start triage cgwalters-forge/tracker#446 pool=claude record=https://github.com/cgwalters-forge/tracker/issues/446#issuecomment-")
            && line.contains(" run=https://github.com/cgwalters-forge/tracker/actions/runs/"),
        "{line}"
    );
    let comments = forge.comments_on("cgwalters-forge/tracker", 446);
    assert_eq!(comments.len(), 2, "{comments:#?}");
    assert!(
        comments[0].contains("```agent-board-plan") && comments[0].contains("\"kind\": \"triage\"")
    );
    assert!(comments[0].contains("\"pool\": \"claude\""));
    assert!(comments[1].starts_with("<!-- agent-board-dispatch: "));
    let second = apply(&forge, &config);
    assert_eq!(second.failures, 0, "{:#?}", second.lines);
    assert_eq!(
        forge.dispatches().len(),
        6,
        "a rerun dispatched again: {:#?}",
        second.lines
    );
    assert!(
        second
            .lines
            .iter()
            .any(|l| l.starts_with("hold cgwalters-forge/tracker#446: plan ")
                && l.ends_with(&format!(
                    "was dispatched already, {}",
                    first
                        .lines
                        .iter()
                        .find_map(|l| l.split(" record=").nth(1)?.split(' ').next())
                        .unwrap()
                ))),
        "{:#?}",
        second.lines
    );
    assert_eq!(
        last_scheduled(&second),
        "idle: 6 slot(s) free and nothing started"
    );
}

/// `--max-dispatch` starts the first runs the pass would and holds the rest,
/// a line each, writing nothing for them; a preview of the recorded board
/// plans what a local apply over it starts. A start is a dispatch, not a
/// write, so `--max-writes` leaves it.
#[test]
fn caps_hold_starts_past_max_dispatch() {
    let starts: Vec<&str> = STARTED
        .iter()
        .filter(|l| l.starts_with("start "))
        .copied()
        .collect();
    let config = config(json!({}));
    let limits = |dispatch, writes| Limits {
        dispatch,
        writes,
        ..Limits::default()
    };
    for (limits, started, summary) in [
        (
            limits(None, None),
            6,
            "summary: dispatched 6, writes 0, held 0",
        ),
        (
            limits(Some(1), Some(0)),
            1,
            "summary: dispatched 1/1, writes 0/0, held 5",
        ),
        (
            limits(Some(0), Some(3)),
            0,
            "summary: dispatched 0/0, writes 0/3, held 6",
        ),
        (
            limits(None, Some(0)),
            6,
            "summary: dispatched 6, writes 0/0, held 0",
        ),
    ] {
        let replay = without_epics(replay());
        let preview = (Mode::Preview, limits);
        let preview = run(
            &replay,
            &replay,
            &config,
            State::default(),
            preview,
            replay.now,
        );
        let forge = Forge::new(None, false);
        let local = (Mode::Local, limits);
        let applied = run(
            &forge,
            &forge,
            &config,
            State::default(),
            local,
            forge.replay.now,
        );
        for report in [preview.unwrap(), applied.unwrap()] {
            let held: Vec<&str> = report
                .lines
                .iter()
                .filter_map(|l| l.strip_prefix("held by --max-dispatch: "))
                .collect();
            assert_eq!(held, starts[started..], "{limits:?}: {:#?}", report.lines);
            let begun = report.lines.iter().filter(|l| l.starts_with("start "));
            assert_eq!(begun.count(), started, "{limits:?}");
            assert_eq!(report.summary, summary);
            assert_eq!(report.failures, 0, "{:#?}", report.lines);
        }
        let issue = |line: &str| line.split(' ').nth(2).unwrap().to_owned();
        let expected: Vec<String> = starts[..started].iter().map(|l| issue(l)).collect();
        assert_eq!(forge.dispatches(), expected, "{limits:?}");
        for held in &starts[started..] {
            let held = issue(held);
            let (repo, number) = held.split_once('#').unwrap();
            let comments = forge.comments_on(repo, number.parse().unwrap());
            assert!(comments.is_empty(), "{held} was written: {comments:#?}");
        }
    }
}

/// A pass interrupted at each write boundary: the failure shows, the slot
/// stays taken (only one is free, so nothing else starts), and the next pass
/// neither posts nor dispatches for that issue again.
#[test]
fn apply_interrupted_at_each_write_reruns_without_duplicates() {
    let config = config(json!({"wip": 3}));
    for (boundary, fail, after_record) in [
        (
            "the triage plan's post",
            Some("POST https://api.github.com/repos/cgwalters-forge/tracker/issues/446/comments"),
            false,
        ),
        ("the dispatch record's read-back", None, true),
        ("the dispatch response", Some("/dispatches"), false),
    ] {
        let forge = Forge::new(fail, after_record);
        let first = apply(&forge, &config);
        assert_eq!(first.failures, 1, "{boundary}: {:#?}", first.lines);
        assert!(
            first
                .lines
                .iter()
                .any(|l| l
                    .starts_with("start triage cgwalters-forge/tracker#446 pool=claude failed: ")),
            "{boundary}: {:#?}",
            first.lines
        );
        assert!(
            first
                .lines
                .iter()
                .any(|l| l == "wip: no slot left; 8 more candidate(s) wait"),
            "{boundary}: the failed start must keep its slot: {:#?}",
            first.lines
        );
        let posted = forge.comments_on("cgwalters-forge/tracker", 446);
        let dispatched = forge.dispatches().len();
        let second = apply(&forge, &config);
        assert_eq!(
            forge.comments_on("cgwalters-forge/tracker", 446).len(),
            posted.len(),
            "{boundary}: the rerun wrote on #446 again: {:#?}",
            second.lines
        );
        assert!(
            forge.dispatches()[dispatched..]
                .iter()
                .all(|d| d != "cgwalters-forge/tracker#446"),
            "{boundary}: #446 dispatched again"
        );
        assert!(
            second
                .lines
                .iter()
                .any(|l| l.starts_with("hold cgwalters-forge/tracker#446: ")),
            "{boundary}: {:#?}",
            second.lines
        );
    }
}

/// The account is checked before the pass's first write: a token that is
/// not a plan writer's, or unread, writes nothing, rather than a triage plan
/// that no pass would take as one.
#[test]
fn apply_writes_nothing_without_a_writer_token() {
    for (case, login, fail, error) in [
        (
            "another account",
            "someone",
            None,
            "schedule error: someone is not a plan writer",
        ),
        (
            "an unread account",
            "cgwalters-bot",
            Some("graphql query { viewer"),
            "schedule error: connection reset",
        ),
    ] {
        let mut forge = Forge::new(fail, false);
        forge.login = login;
        let report = apply(&forge, &config(json!({})));
        assert!(
            report.lines.iter().any(|l| l.starts_with(error)),
            "{case}: {:#?}",
            report.lines
        );
        assert_eq!(report.failures, 1, "{case}");
        assert!(forge.writes.borrow().is_empty(), "{case}");
    }
}

/// What changed after the board and the runs were read holds a later start:
/// each edit applies once tracker#446 is dispatched, before tracker#459 is.
#[test]
fn apply_rereads_holds_and_capacity_before_each_start() {
    let on_459 = |edit: fn(&mut Value)| -> Late {
        Box::new(move |request, answer| {
            if request.contains(r#""number":459"#) {
                edit(&mut answer["repository"]["issue"]);
            }
        })
    };
    let queued = |runs: Vec<Value>| -> Late {
        Box::new(move |request, answer| {
            if request.contains("status=queued") {
                answer["total_count"] = runs.len().into();
                answer["workflow_runs"] = runs.clone().into();
            }
        })
    };
    let cases: Vec<(&str, u64, Late, &str)> = vec![
        (
            "another caller's run takes the last slot",
            4,
            queued(runs(1)),
            "hold wip: 4 of 4 runs active; 8 more candidate(s) wait",
        ),
        (
            "another caller started a run for it",
            8,
            queued(vec![
                json!({"id": 7, "display_title": "triage cgwalters-forge/tracker#459 (claude)", "html_url": "https://github.com/cgwalters-forge/tracker/actions/runs/7"}),
            ]),
            "hold cgwalters-forge/tracker#459: a run is in flight, https://github.com/cgwalters-forge/tracker/actions/runs/7",
        ),
        (
            "labelled needs-human since",
            8,
            on_459(|issue| {
                let labels = &mut issue["labels"];
                labels["nodes"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"name": "needs-human"}));
                labels["totalCount"] = (labels["totalCount"].as_u64().unwrap() + 1).into();
            }),
            "hold cgwalters-forge/tracker#459: labelled needs-human",
        ),
        (
            "set Blocked on the board since",
            8,
            on_459(|issue| {
                issue["projectItems"]["nodes"][0]["status"] = json!({"name": "Blocked"});
            }),
            "hold cgwalters-forge/tracker#459: board Status is Blocked",
        ),
        (
            "given a dependency since",
            8,
            on_459(|issue| issue["issueDependenciesSummary"]["blockedBy"] = 1.into()),
            "hold cgwalters-forge/tracker#459: 1 open dependencies",
        ),
    ];
    for (case, wip, late, line) in cases {
        let mut forge = Forge::new(None, false);
        forge.late = Some(late);
        let report = apply(&forge, &config(json!({"wip": wip})));
        assert_eq!(report.failures, 0, "{case}: {:#?}", report.lines);
        assert!(
            report.lines.iter().any(|l| l == line),
            "{case}: {:#?}",
            report.lines
        );
        let dispatched = forge.dispatches();
        assert_eq!(dispatched[0], "cgwalters-forge/tracker#446", "{case}");
        assert!(
            !dispatched
                .iter()
                .any(|d| d == "cgwalters-forge/tracker#459"),
            "{case}: {dispatched:#?}"
        );
        assert!(
            forge.comments_on("cgwalters-forge/tracker", 459).is_empty(),
            "{case}"
        );
    }
}

/// However little of the budget is left, a start writes nothing or
/// completes: no triage plan without its record, no record without its run.
#[test]
fn apply_never_runs_out_between_a_claim_and_its_run() {
    let config = config(json!({"wip": 3}));
    let mut started = None;
    for limit in 1..=200 {
        let forge = Forge::new(None, false);
        let admit_by = Instant::now() + Duration::from_secs(PASS_SECONDS_LIMIT);
        run_within(
            &forge,
            &forge,
            &config,
            (
                State::default(),
                Mode::Actions(&own()),
                Limits::default(),
                forge.replay.now,
            ),
            &Bounds::new(limit, admit_by),
        )
        .unwrap();
        let comments = forge.comments_on("cgwalters-forge/tracker", 446);
        let planned = comments.iter().any(|c| c.contains("```agent-board-plan"));
        let recorded = comments
            .iter()
            .any(|c| c.starts_with("<!-- agent-board-dispatch"));
        let dispatched = !forge.dispatches().is_empty();
        assert_eq!(planned, recorded, "limit {limit}: {comments:#?}");
        assert_eq!(recorded, dispatched, "limit {limit}: {comments:#?}");
        if dispatched {
            started.get_or_insert(limit);
        }
    }
    assert!(started.is_some(), "no limit up to 200 started a run");
}

/// The operator's implement plan for tracker#446, without a pool.
fn implement_plan(replay: &mut Replay) {
    let comments = &mut answer(replay, r#""name":"tracker","number":446"#)["data"]["repository"]["issue"]
        ["comments"]["nodes"];
    let plan = json!({"schema": JOB_PLAN_SCHEMA, "repo": "cgwalters-forge/tracker", "item": 446,
        "kind": "implement", "runner": "hosted", "brief": "Write the staging design down."});
    comments.as_array_mut().unwrap().push(json!({
        "url": "https://github.com/cgwalters-forge/tracker/issues/446#issuecomment-9",
        "body": format!("```{PLAN_FENCE}\n{plan}\n```"),
        "createdAt": "2026-10-23T00:00:00Z", "lastEditedAt": null,
        "author": {"login": "cgwalters"}, "editor": null,
    }));
}

/// Unlike standalone dispatch, the pass waits when its overlap check cannot
/// read: the recording has no text of tracker#446 for it.
#[test]
fn implement_waits_when_its_overlap_check_cannot_read() {
    let mut replay = replay();
    only(&mut replay, &["cgwalters-forge/tracker#446"]);
    implement_plan(&mut replay);
    let report = preview(&config(json!({})), &replay);
    let line = "hold cgwalters-forge/tracker#446: Waiting: the overlap check could not read (the replay did not record";
    assert!(
        report.lines.iter().any(|l| l.starts_with(line)),
        "{:#?}",
        report.lines
    );
    assert_eq!(report.failures, 0);
}

/// A plan that names no pool is sent with the one pace chose; the dispatch
/// record and the run are the plan protocol's.
#[test]
fn implement_takes_the_paced_pool() {
    let forge = {
        let mut forge = Forge::new(None, false);
        implement_plan(&mut forge.replay);
        // The issue's text, which names no path: nothing to overlap.
        let mut text = forge
            .replay
            .requests
            .iter()
            .find(|e| {
                let request = e["request"].to_string();
                request.contains(r#""number":527"#) && !request.contains("after")
            })
            .unwrap()
            .clone();
        text["request"]["variables"]["number"] = 446.into();
        text["answer"]["data"]["repository"]["issue"] =
            json!({"title": "Staging", "body": "A staging organization."});
        forge.replay.requests.push(text);
        forge
    };
    let report = apply(&forge, &config(json!({"wip": 3})));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    let line = "start implement cgwalters-forge/tracker#446 plan=9@2026-10-23T00:00:00Z pool=claude record=";
    assert!(
        report.lines.iter().any(|l| l.starts_with(line)),
        "{:#?}",
        report.lines
    );
    let writes = forge.writes.borrow();
    let (_, _, sent) = writes
        .iter()
        .find(|(_, url, _)| url.ends_with("/dispatches"))
        .unwrap();
    assert_eq!(sent["inputs"]["kind"], "implement");
    assert_eq!(sent["inputs"]["pool"], "claude");
    assert_eq!(sent["ref"], "main");
}

// ---- Pure logic ----

#[test]
fn config_table() {
    let minimal = json!({"operator": "op", "bot": "bot", "dispatchRepo": "o/caller", "dispatchWorkflow": "dispatch.yml"});
    let parsed = Config::parse(&minimal.to_string()).unwrap();
    assert!(!parsed.work() && !parsed.triage && parsed.follow.is_empty());
    assert!(parsed.pools.0.is_empty() && parsed.dispatch_ref.is_none());
    // Pools keep the file's order, which breaks ties.
    let ordered = Config::parse(
        r#"{"operator": "op", "bot": "bot", "dispatchRepo": "o/caller", "dispatchWorkflow": "dispatch.yml",
            "pools": {"zeta": {}, "alpha": {"target": 0.5, "burst": 10}}, "usage": {"repository": "o/ops", "issue": 1}}"#,
    )
    .unwrap();
    let names: Vec<&str> = ordered.pools.0.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["zeta", "alpha"]);
    assert_eq!(ordered.pools.0[0].1.target, DEFAULT_TARGET);
    for (edit, error) in [
        (json!({"extra": 1}), "unknown field `extra`"),
        (json!({"wip": null}), "config.wip is null"),
        (
            json!({"pr": {"fixPools": [null]}}),
            "config.pr.fixPools[0] is null",
        ),
        (json!({"pr": {"other": 1}}), "unknown field `other`"),
        (json!({"wip": 101}), "wip is not 0 to 100"),
        (
            json!({"wip": 1, "workRepositories": ["o/r"]}),
            "board is required",
        ),
        (
            json!({"board": {"owner": "o", "project": 0}, "wip": 1, "workRepositories": ["o/r"]}),
            "board project is not a positive number",
        ),
        (
            json!({"pools": {"a": {"target": 0}}, "usage": {"repository": "o/ops", "issue": 1}}),
            "pools.a.target",
        ),
        (
            json!({"pools": {"a": {"burst": 101}}, "usage": {"repository": "o/ops", "issue": 1}}),
            "pools.a.burst",
        ),
        (
            json!({"pools": {"tier-1": {}}, "usage": {"repository": "o/ops", "issue": 1}}),
            "is not a pool name",
        ),
        (json!({"pools": {"a": {}}}), "pools need usage"),
        (
            json!({"planPools": ["a"]}),
            "planPools names a, which is not in pools",
        ),
        (
            json!({"triage": true}),
            "triage needs triagePools or planPools",
        ),
        (
            json!({"usage": {"repository": "o/ops", "issue": 1, "url": "http://broker"}}),
            "usage url is not https",
        ),
        (
            json!({"pr": {"rebaseRounds": 4}}),
            "rebaseRounds is not 0 to 3",
        ),
        (
            json!({"pr": {"mergeQueue": {"tries": 6}}}),
            "mergeQueue tries is not 1 to 5",
        ),
        (json!({"dispatchWorkflow": "../x.yml"}), "invalid workflow"),
        (json!({"follow": ["not a repo"]}), "invalid repository"),
        (json!({"bot": "-bad-"}), "invalid login"),
        (
            json!({"follow": ["o/r"], "usage": {"repository": "o/ops", "issue": 1}}),
            "pr fixPools is required to pace",
        ),
    ] {
        let mut value = minimal.clone();
        value
            .as_object_mut()
            .unwrap()
            .extend(edit.as_object().unwrap().clone());
        let got = format!("{:#}", Config::parse(&value.to_string()).unwrap_err());
        assert!(got.contains(error), "{edit}: {got}");
    }
    let duplicate = r#"{"operator": "op", "bot": "bot", "dispatchRepo": "o/caller", "dispatchWorkflow": "dispatch.yml",
        "pools": {"a": {}, "a": {}}, "usage": {"repository": "o/ops", "issue": 1}}"#;
    assert!(
        format!("{:#}", Config::parse(duplicate).unwrap_err()).contains("pool a is named twice")
    );
    // Pacing holds followed pull requests' fix runs only on the pools named.
    let paced = config(json!({"follow": ["o/r"], "pr": {"fixPools": ["claude"]}}));
    assert_eq!(paced.pr.fix_pools, ["claude"]);
    assert!(
        Config::parse(
            &json!({"operator": "op", "bot": "bot", "dispatchRepo": "o/caller",
            "dispatchWorkflow": "dispatch.yml", "follow": ["o/r"]})
            .to_string()
        )
        .is_ok()
    );
    // The PR stage keeps today's poll defaults but for its own keys.
    let poll = config(json!({"pr": {"mergeQueue": {}, "rebaseRounds": 2}})).poll();
    assert_eq!(poll.policy.merge_queue, Some(poll::QUEUE_TRIES));
    assert_eq!(poll.policy.rebase_rounds, 2);
    assert_eq!(poll.policy.continuation, poll::Continuation::default());
    assert!(poll.policy.operator_comments.is_empty() && poll.policy.notifications.is_none());
    let pace = poll.policy.pace.unwrap();
    assert_eq!(
        (pace.repository.as_str(), pace.issue, pace.bot.as_str()),
        ("cgwalters-forge/bot-ops", 1, "cgwalters-bot")
    );
}

#[test]
fn room_table() {
    let now = at("2026-10-23T12:00:00Z");
    let settings = PoolSettings {
        target: 0.95,
        burst: 0.0,
    };
    // A week's window half gone: pace allows 47.5 points.
    let entry = json!({"observed_at": "2026-10-23T11:00:00Z", "used_percent": 40.0, "window_minutes": 10080,
        "resets_at": "2026-10-27T00:00:00Z", "hold": false});
    let snapshot =
        |entry: Value| json!({"updated_at": "2026-10-23T11:50:00Z", "pools": {"p": entry}});
    let edited = |key: &str, value: Value| {
        let mut e = entry.clone();
        e[key] = value;
        snapshot(e)
    };
    for (name, snap, expected) in [
        ("under pace", Some(snapshot(entry.clone())), Ok(7.5)),
        (
            "over pace but not held: negative room",
            Some(edited("used_percent", json!(60.0))),
            Ok(-12.5),
        ),
        ("no snapshot", None, Err("there is no usage snapshot")),
        (
            "stale snapshot",
            Some(json!({"updated_at": "2026-10-23T11:14:59Z", "pools": {"p": entry.clone()}})),
            Err("the usage snapshot is stale, from 2026-10-23T11:14:59Z"),
        ),
        (
            "snapshot within the clock skew",
            Some(json!({"updated_at": "2026-10-23T12:05:00Z", "pools": {"p": entry.clone()}})),
            Ok(7.5),
        ),
        (
            "future snapshot",
            Some(json!({"updated_at": "2026-10-23T12:05:01Z", "pools": {"p": entry.clone()}})),
            Err("the usage snapshot is from the future, 2026-10-23T12:05:01Z"),
        ),
        (
            "future observation",
            Some(edited("observed_at", json!("2026-10-24T12:00:00Z"))),
            Err("p's reading is from the future"),
        ),
        (
            "no reading",
            Some(json!({"updated_at": "2026-10-23T11:50:00Z", "pools": {"p": null}})),
            Err("p has no reading"),
        ),
        (
            "observation over 30 hours old",
            Some(edited("observed_at", json!("2026-10-22T05:59:59Z"))),
            Err("p has no reading from the last 30 hours"),
        ),
        ("held", Some(edited("hold", json!(true))), Err("p is held")),
        (
            "hold missing is not room",
            Some({
                let mut e = entry.clone();
                e.as_object_mut().unwrap().remove("hold");
                snapshot(e)
            }),
            Err("p is held"),
        ),
        (
            "exhausted",
            Some(edited("used_percent", json!(100.0))),
            Err("p has used its window"),
        ),
        (
            "reset since its reading",
            Some(edited("resets_at", json!("2026-10-23T12:00:00Z"))),
            Err("p's window has reset since its reading"),
        ),
        (
            "no window",
            Some(edited("window_minutes", json!(0))),
            Err("p has no current window"),
        ),
    ] {
        let got = room(snap.as_ref(), "p", &settings, now).map(|r| (r * 1000.0).round() / 1000.0);
        assert_eq!(got, expected.map_err(str::to_owned), "{name}");
    }
    // Most room wins; a tie goes to the first named; held pools are skipped.
    let pools = Pools(vec![
        ("a".into(), settings.clone()),
        ("b".into(), settings.clone()),
        (
            "c".into(),
            PoolSettings {
                target: 1.0,
                burst: 0.0,
            },
        ),
    ]);
    let three = json!({"updated_at": "2026-10-23T11:50:00Z", "pools": {"a": entry.clone(), "b": entry.clone(), "c": entry.clone()}});
    let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        choose(Some(&three), &pools, &names(&["a", "b"]), now),
        Ok("a".into())
    );
    assert_eq!(
        choose(Some(&three), &pools, &names(&["b", "a"]), now),
        Ok("b".into())
    );
    assert_eq!(
        choose(Some(&three), &pools, &names(&["a", "c"]), now),
        Ok("c".into())
    );
    let mut held = three.clone();
    held["pools"]["c"]["hold"] = true.into();
    assert_eq!(
        choose(Some(&held), &pools, &names(&["c", "b"]), now),
        Ok("b".into())
    );
    assert_eq!(
        choose(Some(&held), &pools, &names(&["c"]), now),
        Err("c is held".into())
    );
    assert_eq!(
        choose(Some(&three), &pools, &names(&["x"]), now),
        Err("x is not in pools".into())
    );
    assert_eq!(
        choose(Some(&three), &pools, &[], now),
        Err("no automatic pool is configured".into())
    );
    assert_eq!(
        choose(None, &pools, &names(&["a", "b"]), now),
        Err("there is no usage snapshot".into())
    );
}

fn facts(repo: &str, number: u64, created: &str) -> Facts {
    Facts {
        issue: ItemRef {
            host: DEFAULT_HOST.into(),
            repository: repo.into(),
            number,
        },
        open: true,
        created: at(created),
        author: Some("op".into()),
        labels: vec![],
        labels_complete: true,
        blocked_by: 0,
        pulls: vec![],
        pulls_complete: true,
        parent: None,
        priority: None,
        status: None,
    }
}

/// A closing reference GitHub answers as null is unread, so the issue's
/// pull requests are incomplete, never "none open".
#[test]
fn null_closing_references_are_unread() {
    let open = json!({"number": 2, "state": "OPEN", "repository": {"nameWithOwner": "o/r"},
        "author": {"login": "cgwalters"}});
    let closed = json!({"number": 3, "state": "CLOSED", "repository": {"nameWithOwner": "o/r"},
        "author": null});
    for (nodes, total, read, open_count) in [
        (json!([open, closed]), 2, 2, 1),
        (json!([null, closed]), 2, 1, 0),
        (json!([null]), 1, 0, 0),
    ] {
        let refs = json!({"nodes": nodes, "totalCount": total});
        let (pulls, n, of) = pulls(&refs).unwrap();
        assert_eq!((pulls.len(), n, of), (open_count, read, total), "{refs}");
    }
    let mut node = json!({
        "number": 1, "state": "OPEN", "createdAt": "2026-10-01T00:00:00Z",
        "repository": {"nameWithOwner": "o/r"}, "author": {"login": "cgwalters"},
        "labels": {"nodes": [], "totalCount": 0},
        "issueDependenciesSummary": {"blockedBy": 0},
        "closedByPullRequestsReferences": {"nodes": [null], "totalCount": 1},
        "parent": null,
    });
    assert!(!Facts::from_node(&node).unwrap().pulls_complete);
    node["closedByPullRequestsReferences"] = json!({"nodes": [], "totalCount": 0});
    assert!(Facts::from_node(&node).unwrap().pulls_complete);
}

#[test]
fn hold_table() {
    let base = facts("o/r", 1, "2026-10-01T00:00:00Z");
    let busy: BTreeMap<String, String> = [("o/r#1".into(), "https://run".into())].into();
    let people = ["op", "bot"];
    type Change = fn(&mut Facts);
    for (name, change, expected) in [
        ("ready", (|_| {}) as Change, None),
        ("closed", |f| f.open = false, Some("closed")),
        (
            "needs-human",
            |f| f.labels = vec!["Needs-Human".into()],
            Some("labelled needs-human"),
        ),
        (
            "needs-design",
            |f| f.labels = vec!["needs-design".into()],
            Some("labelled needs-design"),
        ),
        (
            "unread labels",
            |f| f.labels_complete = false,
            Some("its labels could not all be read"),
        ),
        (
            "board Blocked",
            |f| f.status = Some("Blocked".into()),
            Some("board Status is Blocked"),
        ),
        (
            "dependency",
            |f| f.blocked_by = 2,
            Some("2 open dependencies"),
        ),
        (
            "bot pull request",
            |f| f.pulls = vec![(9, "o/r".into(), Some("bot".into()))],
            Some("pull request o/r#9 is open"),
        ),
        (
            "visitor pull request",
            |f| f.pulls = vec![(9, "o/r".into(), Some("visitor".into()))],
            None,
        ),
        (
            "unread pull requests",
            |f| f.pulls_complete = false,
            Some("its pull requests could not all be read"),
        ),
        (
            "a trusted pull request among those read",
            |f| {
                f.pulls = vec![(9, "o/r".into(), Some("op".into()))];
                f.pulls_complete = false;
            },
            Some("pull request o/r#9 is open"),
        ),
    ] {
        let mut f = base.clone();
        change(&mut f);
        assert_eq!(
            hold(&f, &BTreeMap::new(), &people).as_deref(),
            expected,
            "{name}"
        );
    }
    assert_eq!(
        hold(&base, &busy, &people).as_deref(),
        Some("a run is in flight, https://run")
    );
}

#[test]
fn rank_orders_by_epic_then_own_priority_then_age() {
    let options: Vec<String> = ["P0", "P1", "P2"].iter().map(|s| s.to_string()).collect();
    let parent = Parent {
        issue: ItemRef {
            host: DEFAULT_HOST.into(),
            repository: "o/t".into(),
            number: 1,
        },
        open: true,
        labels: vec!["epic".into()],
        labels_complete: true,
    };
    let child = |n, created, epic: Option<&str>, own: Option<&str>| {
        let mut f = facts("o/r", n, created);
        f.parent = Some(parent.clone());
        (rank(&options, epic, own, &f), n)
    };
    let alone =
        |n, created, own: Option<&str>| (rank(&options, None, own, &facts("o/r", n, created)), n);
    let mut ranked = [
        child(1, "2026-10-01T00:00:00Z", Some("P2"), Some("P0")),
        child(2, "2026-10-02T00:00:00Z", Some("P0"), Some("P2")),
        child(3, "2026-10-01T00:00:00Z", Some("P0"), Some("P2")),
        child(4, "2026-09-01T00:00:00Z", Some("P0"), Some("P1")),
        alone(5, "2026-10-05T00:00:00Z", Some("P0")),
        alone(6, "2026-01-01T00:00:00Z", None),
        alone(7, "2026-01-01T00:00:00Z", Some("P9")),
        child(8, "2026-01-01T00:00:00Z", None, Some("P0")),
    ];
    ranked.sort();
    let order: Vec<u64> = ranked.iter().map(|(_, n)| *n).collect();
    // An unparented P0 ranks P0 twice, ahead of a P0 epic's P1 child; an
    // unset or unknown Priority comes last, then the oldest first.
    assert_eq!(order, [5, 4, 3, 2, 1, 8, 6, 7]);
}

#[test]
fn budget_refuses_past_its_limit() {
    let replay = replay();
    let admit_by = Instant::now() + Duration::from_secs(PASS_SECONDS_LIMIT);
    let bounds = Bounds::new(WRITE_RESERVE + 1, admit_by);
    let budget = Budget {
        inner: &replay,
        bounds: &bounds,
    };
    let url = "https://api.github.com/repos/cgwalters-forge/tracker/actions/workflows/dispatch.yml/runs?status=queued&per_page=100";
    assert!(budget.get(url, None).is_ok());
    let error = budget.get(url, None).err().unwrap();
    assert_eq!(
        error.to_string(),
        "the pass's request budget (500) is spent"
    );
    // Writes spend the reserve reads leave them; the replay then refuses
    // each, but only after the budget let it through.
    for _ in 0..WRITE_RESERVE {
        let error = budget.send("PATCH", url, &json!({})).err().unwrap();
        assert!(error.to_string().starts_with("a replay is read-only"));
    }
    let error = budget.send("PATCH", url, &json!({})).err().unwrap();
    assert_eq!(
        error.to_string(),
        "the pass's request budget (500) is spent"
    );
}

/// Past its time bound a pass admits writes, and the reads of a start under
/// way, for [`FINISH_SECONDS`] more; then nothing.
#[test]
fn budget_refuses_past_its_time() {
    let replay = replay();
    let url = "https://api.github.com/repos/cgwalters-forge/tracker/actions/workflows/dispatch.yml/runs?status=queued&per_page=100";
    let spent = "the pass's time bound (900 seconds) is spent";
    for (case, late, finishing, read, write) in [
        ("in time", 0, false, true, true),
        ("late", 1, false, false, true),
        ("late, finishing a start", 1, true, true, true),
        ("past finishing", FINISH_SECONDS + 1, true, false, false),
    ] {
        let now = Instant::now();
        let admit_by = match late {
            0 => now + Duration::from_secs(PASS_SECONDS_LIMIT),
            late => now.checked_sub(Duration::from_secs(late)).unwrap(),
        };
        let bounds = Bounds::new(PASS_REQUEST_LIMIT, admit_by);
        bounds.finishing.set(finishing);
        let budget = Budget {
            inner: &replay,
            bounds: &bounds,
        };
        let got = budget.get(url, None).map(drop).map_err(|e| e.to_string());
        assert_eq!(got.is_ok(), read, "{case}: {got:?}");
        if !read {
            assert_eq!(got.unwrap_err(), spent, "{case}");
        }
        // The replay refuses a write the budget lets through.
        let error = budget.send("PATCH", url, &json!({})).unwrap_err();
        assert_eq!(error.to_string() != spent, write, "{case}: {error}");
    }
}

#[test]
fn replay_refuses_writes_and_unrecorded_reads() {
    let replay = replay();
    assert!(
        replay
            .send("POST", "https://api.github.com/x", &json!({}))
            .is_err()
    );
    assert!(
        replay
            .get("https://api.github.com/unrecorded", None)
            .is_err()
    );
    assert_eq!(fnv(""), "cbf29ce484222325");
    assert_eq!(fnv("a"), "af63dc4c8601ec8c");
}
