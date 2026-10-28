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

/// The recording, its second run in progress started minutes before it
/// was read. That run, tracker's 39698121265, was the stuck run of
/// agent-board#376, which the pass now takes as ended ([`replay_scenarios`]);
/// the recording's other tests see it in flight.
pub(super) fn replay() -> Replay {
    let mut replay = Replay::from_json(&serde_json::from_str(BOARD).unwrap()).unwrap();
    started(&mut replay, "2026-10-23T15:03:00Z");
    replay
}

/// The recording's stuck run (see [`replay`]), started at `at`.
pub(super) fn started(replay: &mut Replay, at: &str) {
    let runs = &mut answer(replay, "status=in_progress")["body"]["workflow_runs"];
    let mut runs = runs.as_array_mut().unwrap().iter_mut();
    let stuck = runs.find(|r| r["id"] == STUCK_RUN).unwrap();
    stuck["run_started_at"] = at.into();
    stuck["created_at"] = at.into();
}

const STUCK_RUN: u64 = 39698121265;

/// When a job of the stuck run completed, well before the recording's clock.
const LONG_DONE: &str = "2026-10-23T13:30:00Z";

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

/// A pass run to its end on a current-thread runtime, as the CLI runs it.
pub(super) fn block<T>(pass: impl Future<Output = T>) -> T {
    let runtime = tokio::runtime::Builder::new_current_thread().build();
    runtime.unwrap().block_on(pass)
}

pub(super) fn preview(config: &Config, replay: &Replay) -> Report {
    block(run(
        replay,
        replay,
        config,
        State::default(),
        (Mode::Preview, Limits::default()),
        replay.now,
    ))
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

/// The broker answering `answer` ([`usage::tests::broker`]'s, recorded),
/// under a `usage.url` that names no real one.
fn with_broker(replay: &mut Replay, settings: &mut Value, answer: Value) {
    replay
        .requests
        .push(json!({"request": broker_key(USAGE_PATH), "answer": answer}));
    settings["usage"] = json!({"repository": "cgwalters-forge/bot-ops", "issue": 1,
        "url": "https://broker.invalid/usage"});
}

/// The broker just after a restart, `with_broker`'s with `probe` set: no
/// windows, and `requests` sent for each pool since, or no count at all.
fn restarted(replay: &mut Replay, settings: &mut Value, probe: bool, requests: Option<u64>) {
    let mut broker = usage::tests::broker();
    broker["started_at"] = 1792768000.into();
    let counts = requests.map(|n| json!({"requests": n, "unmetered": 0}));
    broker["anthropic"] = json!({"counts": counts});
    broker["codex"] = json!({"counts": counts});
    with_broker(replay, settings, json!({"body": broker}));
    settings["usage"]["probe"] = probe.into();
}

/// The recorded run in flight on claude, its pool unnamed.
fn unpooled(replay: &mut Replay) {
    let runs = &mut answer(replay, "status=in_progress")["body"]["workflow_runs"];
    for run in runs.as_array_mut().unwrap() {
        let title = run["display_title"]
            .as_str()
            .unwrap()
            .replace(" (claude)", "");
        run["display_title"] = title.into();
    }
}

/// The broker metering `spend` of the week, at list prices in dollars
/// (praxis-credential-broker#38), against a budget of $1000 a week.
pub(super) fn with_spend(replay: &mut Replay, settings: &mut Value, spend: Value) {
    let mut broker = usage::tests::broker();
    broker["spend"] = spend;
    with_broker(replay, settings, json!({"body": broker}));
    settings["budget"] = json!({"weeklyUsd": 1000});
}

/// The broker serving `grant` (`{"body": …}` or `{"error": …}`) as the
/// budget, `source: broker`, with claude on `anthropic` and openai on `codex`.
fn with_grant(replay: &mut Replay, settings: &mut Value, grant: Value) {
    with_broker(replay, settings, json!({"body": usage::tests::broker()}));
    replay
        .requests
        .push(json!({"request": broker_key(GRANT_PATH), "answer": grant}));
    settings["pools"] = json!({"claude": {}, "openai": {}});
    settings["budget"] = json!({"source": "broker", "url": "https://broker.invalid/usage/grant?consumer=public",
        "pools": {"claude": "anthropic", "openai": "codex"}});
}

/// [`usage::tests::grant`] with `edits` (JSON pointer, value), served.
fn granted(replay: &mut Replay, settings: &mut Value, edits: &[(&str, Value)]) {
    let mut grant = usage::tests::grant();
    for (pointer, value) in edits {
        *grant.pointer_mut(pointer).unwrap() = value.clone();
    }
    with_grant(replay, settings, json!({"body": grant}));
}

/// The broker's `/usage` that [`with_broker`] added, failing as when it is down.
fn broker_down(replay: &mut Replay) {
    let usage = broker_key(USAGE_PATH);
    let entry = replay.requests.iter_mut().find(|e| e["request"] == usage);
    entry.unwrap()["answer"] = json!({"error": "GET /usage: HTTP status 502"});
}

/// [`with_grant`] serving `grant`, the other pool held, with `usage.probe`
/// and a broker that has sent no request on anthropic since it started.
fn probe_grant(replay: &mut Replay, settings: &mut Value, mut grant: Value) {
    if let Some(pools) = grant.pointer_mut("/body/pools") {
        pools["anthropic"]["stale"] = true.into();
        pools["anthropic"]["hold"] = true.into();
        pools["codex"]["hold"] = true.into();
    }
    with_grant(replay, settings, grant);
    let broker = &mut answer(replay, "/usage\"")["body"];
    broker["anthropic"]["counts"] = json!({"requests": 0, "unmetered": 0});
    settings["usage"]["probe"] = true.into();
    unpooled(replay);
}

/// [`probe_grant`] of the sample grant with a third pool, fable, on
/// anthropic beside claude.
fn probe_alias(replay: &mut Replay, settings: &mut Value) {
    probe_grant(replay, settings, json!({"body": usage::tests::grant()}));
    settings["pools"]["fable"] = json!({});
    settings["budget"]["pools"]["fable"] = "anthropic".into();
}

/// A week's spend of `total` dollars by runs as the broker reports it, all of
/// it theirs: no operator's interactive use.
pub(super) fn spend(total: f64) -> Value {
    let runs = json!({"total": total, "unpriced_requests": 0, "unmetered_requests": 0});
    json!({"currency": "USD", "basis": "list", "prices": "2026-10-20", "window_days": 7,
        "since": 1_792_454_400, "total": total, "unpriced_requests": 0,
        "unmetered_requests": 0, "by_operator": {}, "runs": runs})
}

pub(super) fn set_usage(replay: &mut Replay, snapshot: &Value) {
    let body = format!("{USAGE_MARKER}\n\n```json\n{snapshot:#}\n```\n");
    answer(replay, "bot-ops/issues/1/comments")["body"][0]["body"] = body.into();
}

/// The line of a pass whose grant holds both pools.
const GRANT_HELD: &str =
    "hold pools: claude is held by its grant; openai is held by its grant; new runs wait";

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
    "hold epic cgwalters-forge/tracker#890: its sub-issues changed (cgwalters-forge/agent-board#285 closed, cgwalters-forge/agent-board#286 closed, cgwalters-forge/agent-board#287 closed and 5 more); cgwalters-forge/agent-board#284 edited; cgwalters-forge/agent-board#289's Priority is P0, not P2; planned again from 2026-10-23T16:34:49Z, 3 hours after its last request",
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
        "usage: no usage.url; pacing reads the published snapshot",
        "follow: no repositories",
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

/// Spend at the weekly budget holds stage 3's planning run as it holds
/// stage 4's: a planning run is a new run too.
#[test]
fn budget_holds_epic_planning() {
    let mut replay = replay();
    let mut settings = json!({});
    with_spend(&mut replay, &mut settings, spend(1000.0));
    let report = preview(&config(settings), &replay);
    let planned = "hold epic cgwalters-forge/tracker#596: due (its sub-issues changed";
    let line = report.lines.iter().find(|l| l.starts_with(planned));
    let line = line.unwrap_or_else(|| panic!("{:#?}", report.lines));
    assert!(line.ends_with("; budget: weekly budget reached"), "{line}");
    let held = report.holds.iter().find(|e| e.line() == *line);
    let held = held.unwrap_or_else(|| panic!("{line} is not a recorded hold"));
    assert_eq!(held.hold.kind, HoldKind::Budget);
    assert!(!report.lines.iter().any(|l| l.starts_with("start ")));
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

/// Leases on claude in the dispatch repository of runs `ids`, queued, and
/// after them an expired one of run 7, with [`Config::leases`] on.
fn leased(replay: &mut Replay, settings: &mut Value, ids: &[u64]) {
    settings["leases"] = true.into();
    let get = |path: String, body: Value| {
        let url = format!("{API_ROOT}repos/cgwalters-forge/tracker/{path}");
        json!({"request": {"get": url}, "answer": {"status": 200, "body": body}})
    };
    let mut refs = Vec::new();
    for (slot, &id) in ids.iter().chain(&[7]).enumerate() {
        let expires = match id {
            7 => "2026-10-23T00:00:00Z",
            _ => "2026-10-24T15:00:00Z",
        };
        let lease = json!({"run": id, "item": "o/r#1", "pool": "claude",
            "taken_at": "2026-10-23T15:00:00Z", "expires_at": expires});
        refs.push(json!({"ref": format!("refs/agent-board/leases/{slot}"),
            "object": {"sha": format!("c{slot}")}}));
        let message = json!({"message": lease.to_string()});
        replay
            .requests
            .push(get(format!("git/commits/c{slot}"), message));
        let run = json!({"id": id, "status": "queued"});
        replay.requests.push(get(format!("actions/runs/{id}"), run));
    }
    let list = get("git/matching-refs/agent-board/leases/".into(), refs.into());
    replay.requests.push(list);
}

/// The recording's stuck run's jobs on one page: each job's status and when
/// it completed.
fn jobs(replay: &mut Replay, jobs: &[(&str, Option<&str>)]) {
    let url = format!(
        "{API_ROOT}repos/cgwalters-forge/tracker/actions/runs/{STUCK_RUN}/jobs?per_page={JOBS_PER_PAGE}"
    );
    let jobs: Vec<Value> = (jobs.iter())
        .map(|(status, at)| json!({"status": status, "completed_at": at}))
        .collect();
    let body = json!({"total_count": jobs.len(), "jobs": jobs});
    replay
        .requests
        .push(json!({"request": {"get": url}, "answer": {"status": 200, "body": body}}));
}

/// Puts `run` on a second page of the in-progress runs, after the two
/// recorded; that page links to `next`, where there is one.
fn second_page(replay: &mut Replay, run: Value, next: Option<&str>) {
    let first = answer(replay, "status=in_progress");
    let url = format!(
        "{API_ROOT}repos/cgwalters-forge/tracker/actions/workflows/dispatch.yml/runs?status=in_progress&per_page=100&page=2"
    );
    first["link"] = format!("<{url}>; rel=\"next\"").into();
    first["body"]["total_count"] = 3.into();
    let link = next.map(|n| format!("<{n}>; rel=\"next\""));
    let body = json!({"total_count": 3, "workflow_runs": [run]});
    replay.requests.push(json!({"request": {"get": url},
        "answer": {"status": 200, "link": link, "body": body}}));
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

/// Keeps tracker#446 alone, made a visitor's sub-issue of `epic`'s #135, an
/// open issue labelled `epic`, last by `by` as tracker#160's labelling was
/// recorded.
fn sub_issue(replay: &mut Replay, epic: &str, by: &str) {
    only(replay, &["cgwalters-forge/tracker#446"]);
    let parent = json!({"number": 135, "state": "OPEN", "repository": {"nameWithOwner": epic},
        "labels": {"nodes": [{"name": EPIC_LABEL}], "totalCount": 1}});
    for entry in &mut replay.requests {
        let nodes = entry["answer"]
            .pointer_mut("/data/repositoryOwner/projectV2/items/nodes")
            .and_then(Value::as_array_mut);
        for node in nodes.into_iter().flatten() {
            if node["content"]["number"] == 446 {
                node["content"]["author"]["login"] = "visitor".into();
                node["content"]["parent"] = parent.clone();
            }
        }
    }
    let tracker_160 = json!({"owner": "cgwalters-forge", "name": "tracker", "number": 160});
    let mut labelled = (replay.requests.iter())
        .find(|e| {
            e["request"]["variables"] == tracker_160
                && e["answer"]
                    .pointer("/data/repository/issue/timelineItems")
                    .is_some()
        })
        .unwrap()
        .clone();
    let (owner, name) = epic.split_once('/').unwrap();
    labelled["request"]["variables"] = json!({"owner": owner, "name": name, "number": 135});
    let events = labelled
        .pointer_mut("/answer/data/repository/issue/timelineItems/nodes")
        .and_then(Value::as_array_mut)
        .unwrap();
    for event in events
        .iter_mut()
        .filter(|e| e["label"]["name"] == EPIC_LABEL)
    {
        event["actor"]["login"] = by.into();
    }
    replay.requests.push(labelled);
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
            "full WIP: six leased runs, two of them listed, with the two in progress",
            Box::new(|r, settings| {
                in_flight(r, runs(2));
                leased(r, settings, &[1, 2, 3, 4, 5, 6]);
            }),
            vec![
                "lease: reap agent-board/leases/6: expired 2026-10-23T00:00:00Z; a preview does not free it",
                "hold wip: 8 of 8 runs active",
            ],
            0,
            0,
        ),
        (
            "a young in_progress run stays in flight, its jobs unread",
            Box::new(|r, _| {
                in_flight(r, runs(6));
                jobs(
                    r,
                    &[
                        ("completed", Some(LONG_DONE)),
                        ("completed", Some(LONG_DONE)),
                    ],
                );
            }),
            vec!["hold wip: 8 of 8 runs active"],
            0,
            0,
        ),
        (
            "an in_progress run over an hour old with a job running stays in flight",
            Box::new(|r, _| {
                in_flight(r, runs(6));
                started(r, "2026-10-23T13:00:00Z");
                jobs(r, &[("completed", Some(LONG_DONE)), ("in_progress", None)]);
            }),
            vec!["hold wip: 8 of 8 runs active"],
            0,
            0,
        ),
        (
            "an in_progress run whose last job completed minutes ago stays in flight",
            Box::new(|r, _| {
                in_flight(r, runs(6));
                started(r, "2026-10-23T13:00:00Z");
                let just = Some("2026-10-23T15:10:00Z");
                jobs(r, &[("completed", Some(LONG_DONE)), ("completed", just)]);
            }),
            vec!["hold wip: 8 of 8 runs active"],
            0,
            0,
        ),
        (
            "an in_progress run whose jobs have all completed has ended",
            Box::new(|r, _| {
                in_flight(r, runs(6));
                started(r, "2026-10-23T13:00:00Z");
                jobs(
                    r,
                    &[
                        ("completed", Some(LONG_DONE)),
                        ("completed", Some(LONG_DONE)),
                    ],
                );
            }),
            vec!["start triage cgwalters-forge/tracker#446 pool=claude"],
            1,
            0,
        ),
        (
            "a 25h-old in_progress run with a job running stays in flight",
            Box::new(|r, _| {
                in_flight(r, runs(6));
                started(r, "2026-10-22T14:00:00Z");
                jobs(r, &[("completed", Some(LONG_DONE)), ("in_progress", None)]);
            }),
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
            "a run that moved between pages and was missed is no capacity",
            Box::new(|r, _| {
                second_page(r, json!({"id": 7}), None);
                answer(r, "page=2")["body"]["total_count"] = 4.into();
            }),
            vec![
                "schedule error: in_progress runs: 3 read of 4; capacity is unknown",
                "status: the runs in flight could not be listed, so none goes back to Todo",
            ],
            0,
            1,
        ),
        (
            "a stuck run GitHub counts but never lists does not hide the rest",
            Box::new(|r, _| {
                answer(r, "status=in_progress")["body"]["total_count"] = 3.into();
            }),
            vec!["start triage cgwalters-forge/tracker#446 pool=claude"],
            6,
            0,
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
            "runs past one page are all read: a run on the second holds its issue",
            Box::new(|r, _| {
                let run = json!({"id": 7, "display_title": "triage cgwalters-forge/tracker#446 (claude)",
                    "html_url": "https://github.com/cgwalters-forge/tracker/actions/runs/7"});
                second_page(r, run, None);
            }),
            vec![
                "hold cgwalters-forge/tracker#446: a run is in flight, https://github.com/cgwalters-forge/tracker/actions/runs/7",
                "wip: no slot left; 185 more candidate(s) wait",
            ],
            5,
            0,
        ),
        (
            "more runs than GitHub lists for a status are no capacity, without paging",
            Box::new(|r, _| {
                second_page(r, json!({"id": 7}), None);
                answer(r, "status=in_progress")["body"]["total_count"] = 1001.into();
            }),
            vec![
                "schedule error: in_progress runs: 2 read of 1001; capacity is unknown, more than GitHub lists",
            ],
            0,
            1,
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
            "a fresh broker reading paces a pass whose published snapshot is stale",
            Box::new(|r, c| {
                r.now += time::Duration::minutes(45);
                with_broker(r, c, json!({"body": usage::tests::broker()}));
            }),
            vec![
                "usage: claude: 18% used, pace allows 50%, resets 2026-10-26T23:00:00Z, read 2026-10-23T15:06:40Z; openai: 3% used, pace allows 2%",
                "usage: publish to cgwalters-forge/bot-ops#1",
                "start triage cgwalters-forge/tracker#446 pool=claude",
            ],
            6,
            0,
        ),
        (
            "a preview paces pull requests by its fresh reading, unpublished",
            Box::new(|r, c| {
                r.now += time::Duration::minutes(45);
                with_broker(r, c, json!({"body": usage::tests::broker()}));
                followed_draft(r, c);
            }),
            vec![
                "usage: publish to cgwalters-forge/bot-ops#1; a preview paces by it unpublished",
                "cgwalters-forge/agentic-job#9 abc continue: continuing (round 1)",
                "pools: claude normal 18%/145%, openai normal 3%/5%",
            ],
            6,
            0,
        ),
        (
            "an unreadable broker is one line, and the stale snapshot holds",
            Box::new(|r, c| {
                r.now += time::Duration::minutes(45);
                with_broker(r, c, json!({"error": "GET /usage: HTTP status 502"}));
            }),
            vec![
                "usage error: reading the broker: GET /usage: HTTP status 502; pacing reads the published snapshot",
                "hold pools: the usage snapshot is stale",
            ],
            0,
            1,
        ),
        (
            "a broker reading without claude's week holds claude",
            Box::new(|r, c| {
                let mut broker = usage::tests::broker();
                broker["anthropic"]["unified_7d"] = Value::Null;
                broker["codex"]["primary"]["used_percent"] = 10.into();
                with_broker(r, c, json!({"body": broker}));
            }),
            vec![
                "usage: claude: no reading (held); openai: 10% used",
                "hold pools: claude has no reading; openai is held until 2026-10-24T00:22:44.21Z",
            ],
            0,
            0,
        ),
        (
            "after a broker restart, without probe, every pool waits for a reading",
            Box::new(|r, c| restarted(r, c, false, Some(0))),
            vec![
                "usage: claude: no reading (held); openai: no reading (held)",
                "hold pools: claude has no reading; openai has no reading; new runs wait",
            ],
            0,
            0,
        ),
        (
            "probe: one run on each pool the broker has sent nothing for",
            Box::new(|r, c| {
                restarted(r, c, true, Some(0));
                unpooled(r);
            }),
            vec![
                "probe: claude has no reading; one run to take one",
                "start triage cgwalters-forge/tracker#446 pool=claude",
                "probe: openai has no reading; one run to take one",
                "start triage cgwalters-forge/tracker#459 pool=openai",
                "hold pools: claude has no reading; openai has no reading; 192 more candidate(s) wait",
            ],
            2,
            0,
        ),
        (
            "probe: none on a pool with a run in flight",
            Box::new(|r, c| restarted(r, c, true, Some(0))),
            vec![
                "probe: openai has no reading; one run to take one",
                "start triage cgwalters-forge/tracker#446 pool=openai",
            ],
            1,
            0,
        ),
        (
            "probe: requests since the restart and still no reading hold",
            Box::new(|r, c| {
                restarted(r, c, true, Some(3));
                unpooled(r);
            }),
            vec!["hold pools: claude has no reading; openai has no reading; new runs wait"],
            0,
            0,
        ),
        (
            "probe: a broker that counts no requests holds",
            Box::new(|r, c| {
                restarted(r, c, true, None);
                unpooled(r);
            }),
            vec!["hold pools: claude has no reading; openai has no reading; new runs wait"],
            0,
            0,
        ),
        (
            "spend under the weekly budget is shown and starts runs",
            Box::new(|r, c| with_spend(r, c, spend(412.37))),
            vec![
                "usage: claude: 18% used, pace allows 50%, resets 2026-10-26T23:00:00Z, read 2026-10-23T15:06:40Z; openai: 3% used, pace allows 2%, resets 2026-10-30T12:00:00Z, read 2026-10-23T15:06:40Z; budget: $412 of $1000 this week at list prices",
                "start triage cgwalters-forge/tracker#446 pool=claude",
            ],
            6,
            0,
        ),
        (
            "spend at the weekly budget holds every new run",
            Box::new(|r, c| with_spend(r, c, spend(1003.0))),
            vec!["hold budget: weekly budget reached; new runs wait"],
            0,
            0,
        ),
        (
            "a broker that does not meter spend holds, never reads as $0",
            Box::new(|r, c| with_spend(r, c, Value::Null)),
            vec![
                "usage: claude: 18% used",
                "hold budget: the usage snapshot has no spend reading; new runs wait",
            ],
            0,
            0,
        ),
        (
            "spend in anything but list-price dollars over a week is no reading",
            Box::new(|r, c| {
                let mut spend = spend(10.0);
                spend["window_days"] = 30.into();
                with_spend(r, c, spend);
            }),
            vec!["hold budget: the usage snapshot has no spend reading; new runs wait"],
            0,
            0,
        ),
        (
            "a total that leaves requests unpriced is no reading, never their $0",
            Box::new(|r, c| {
                let mut spend = spend(0.0);
                spend["runs"]["unpriced_requests"] = 1000.into();
                with_spend(r, c, spend);
            }),
            vec![
                "usage: claude: 18% used, pace allows 50%, resets 2026-10-26T23:00:00Z, read 2026-10-23T15:06:40Z; openai: 3% used, pace allows 2%, resets 2026-10-30T12:00:00Z, read 2026-10-23T15:06:40Z; budget: $1000 a week, 1000 request(s) unpriced (held)",
                "hold budget: the usage snapshot has no spend reading; new runs wait",
            ],
            0,
            0,
        ),
        (
            "a total that does not say it prices every request is no reading",
            Box::new(|r, c| {
                let mut spend = spend(0.0);
                spend["runs"]
                    .as_object_mut()
                    .unwrap()
                    .remove("unpriced_requests");
                with_spend(r, c, spend);
            }),
            vec!["hold budget: the usage snapshot has no spend reading; new runs wait"],
            0,
            0,
        ),
        (
            "a broker that does not split out runs' spend is no reading",
            Box::new(|r, c| {
                let mut spend = spend(0.0);
                spend.as_object_mut().unwrap().remove("runs");
                with_spend(r, c, spend);
            }),
            vec!["hold budget: the usage snapshot has no spend reading; new runs wait"],
            0,
            0,
        ),
        (
            "an operator's unpriced interactive use neither counts nor holds",
            Box::new(|r, c| {
                let mut spend = spend(412.37);
                let operator = json!({"total": 900.0, "unpriced_requests": 7,
                    "unmetered_requests": 0});
                spend["by_operator"] = json!({"alice": operator});
                spend["total"] = 1312.37.into();
                spend["unpriced_requests"] = 7.into();
                with_spend(r, c, spend);
            }),
            vec![
                "usage: claude: 18% used, pace allows 50%, resets 2026-10-26T23:00:00Z, read 2026-10-23T15:06:40Z; openai: 3% used, pace allows 2%, resets 2026-10-30T12:00:00Z, read 2026-10-23T15:06:40Z; budget: $412 of $1000 this week at list prices",
                "start triage cgwalters-forge/tracker#446 pool=claude",
            ],
            6,
            0,
        ),
        (
            "with requests unpriced, pull requests' fix rounds wait too",
            Box::new(|r, c| {
                let mut spend = spend(0.0);
                spend["runs"]["unpriced_requests"] = 1.into();
                with_spend(r, c, spend);
                followed_draft(r, c);
            }),
            vec![
                "budget: the usage snapshot has no spend reading; pull requests' fix rounds wait",
                "pools: claude held, openai held",
                "hold budget: the usage snapshot has no spend reading; new runs wait",
            ],
            0,
            0,
        ),
        (
            "with no spend reading, pull requests' fix rounds wait too",
            Box::new(|r, c| {
                with_spend(r, c, Value::Null);
                followed_draft(r, c);
            }),
            vec![
                "budget: the usage snapshot has no spend reading; pull requests' fix rounds wait",
                "pools: claude held, openai held",
            ],
            0,
            0,
        ),
        (
            "an unreadable broker leaves the published snapshot, with no spend, to hold",
            Box::new(|r, c| {
                with_spend(r, c, spend(10.0));
                broker_down(r);
            }),
            vec![
                "usage error: reading the broker",
                "hold budget: the usage snapshot has no spend reading; new runs wait",
            ],
            0,
            1,
        ),
        (
            "a published snapshot's spend under the budget starts runs",
            Box::new(|r, c| {
                with_spend(r, c, spend(10.0));
                broker_down(r);
                let mut snapshot = usage(r);
                snapshot["budget"] = json!({"weekly_usd": 1000, "spent_usd": 10.0});
                set_usage(r, &snapshot);
            }),
            vec!["start triage cgwalters-forge/tracker#446 pool=claude"],
            6,
            1,
        ),
        (
            "spend just under the budget lets pull requests' rounds and new runs go",
            Box::new(|r, c| {
                with_spend(r, c, spend(999.0));
                followed_draft(r, c);
            }),
            vec![
                "cgwalters-forge/agentic-job#9 abc continue: continuing (round 1)",
                "start triage cgwalters-forge/tracker#446 pool=claude",
            ],
            6,
            0,
        ),
        (
            "within the margin past the budget, pull requests' rounds go on",
            Box::new(|r, c| {
                with_spend(r, c, spend(1050.0));
                followed_draft(r, c);
            }),
            vec![
                "cgwalters-forge/agentic-job#9 abc continue: continuing (round 1)",
                "hold budget: weekly budget reached; new runs wait",
            ],
            0,
            0,
        ),
        (
            "past the margin, pull requests' fix rounds wait too",
            Box::new(|r, c| {
                with_spend(r, c, spend(1150.0));
                followed_draft(r, c);
            }),
            vec![
                "budget: weekly budget reached; pull requests' fix rounds wait",
                "pools: claude held, openai held",
                "hold budget: weekly budget reached; new runs wait",
            ],
            0,
            0,
        ),
        (
            "a grant paces each pool in percentages; a sole configured pool takes the run",
            Box::new(|r, c| {
                granted(r, c, &[]);
                c["triagePools"] = json!(["openai"]);
            }),
            vec![
                "usage: claude: 18% used of 32% granted on anthropic unified_7d, read 2026-10-23T15:06:40Z; openai: 5% used of 32% granted on codex secondary, read 2026-10-23T15:06:40Z",
                "start triage cgwalters-forge/tracker#446 pool=openai",
            ],
            6,
            0,
        ),
        (
            "a small grant holds its pool, and the other runs",
            Box::new(|r, c| {
                let small = json!(0.5);
                granted(
                    r,
                    c,
                    &[("/pools/codex/windows/secondary/allowed_percent", small)],
                );
            }),
            vec![
                "usage: claude: 18% used of 32% granted on anthropic unified_7d, read 2026-10-23T15:06:40Z; openai: 5% used of 1% granted on codex secondary, read 2026-10-23T15:06:40Z; holds new work (grant)",
                "start triage cgwalters-forge/tracker#446 pool=claude",
            ],
            6,
            0,
        ),
        (
            "a grant that holds holds the pass",
            Box::new(|r, c| granted(r, c, &[("/hold", json!(true))])),
            vec![
                "usage: claude: held by the grant (grant); openai: held by the grant (grant)",
                GRANT_HELD,
            ],
            0,
            0,
        ),
        (
            "a pool missing from the grant has no share, and holds",
            Box::new(|r, c| granted(r, c, &[("/pools/codex", Value::Null)])),
            vec![
                "usage: claude: 18% used of 32% granted on anthropic unified_7d, read 2026-10-23T15:06:40Z; openai: held by the grant (grant)",
                "start triage cgwalters-forge/tracker#446 pool=claude",
            ],
            6,
            0,
        ),
        (
            "a missing grant holds the pass, and is a failure",
            Box::new(|r, c| {
                with_grant(r, c, json!({"error": "GET /usage/grant: HTTP status 404"}));
            }),
            vec![
                "usage error: reading the grant: GET /usage/grant: HTTP status 404; every pool holds",
                "usage: claude: held by the grant (no-grant); openai: held by the grant (no-grant)",
                GRANT_HELD,
            ],
            0,
            1,
        ),
        (
            "a grant of another schema holds the pass",
            Box::new(|r, c| granted(r, c, &[("/schema", json!("praxis-broker-grant/v2"))])),
            vec![
                "usage error: reading the grant: the grant is not praxis-broker-grant/v1; every pool holds",
                GRANT_HELD,
            ],
            0,
            1,
        ),
        (
            "an unreadable broker leaves the grant, never the published snapshot, to hold",
            Box::new(|r, c| {
                granted(r, c, &[("/hold", json!(true))]);
                broker_down(r);
            }),
            vec![
                "usage error: reading the broker: GET /usage: HTTP status 502; pacing reads the grant",
                GRANT_HELD,
            ],
            0,
            1,
        ),
        (
            "an unreadable broker leaves the grant to start runs",
            Box::new(|r, c| {
                granted(r, c, &[]);
                c["triagePools"] = json!(["openai"]);
                broker_down(r);
            }),
            vec![
                "usage: claude: 18% used of 32% granted on anthropic unified_7d, read 2026-10-23T15:06:40Z; openai: 5% used",
                "start triage cgwalters-forge/tracker#446 pool=openai",
            ],
            6,
            1,
        ),
        (
            "with neither the grant nor the broker read, the pass holds",
            Box::new(|r, c| {
                with_grant(r, c, json!({"error": "GET /usage/grant: HTTP status 502"}));
                broker_down(r);
            }),
            vec![
                "usage error: reading the grant: GET /usage/grant: HTTP status 502; every pool holds",
                "usage error: reading the broker: GET /usage: HTTP status 502; pacing reads the grant",
                GRANT_HELD,
            ],
            0,
            2,
        ),
        (
            "probe: a stale grant pool with no request since takes one run",
            Box::new(|r, c| probe_grant(r, c, json!({"body": usage::tests::grant()}))),
            vec![
                "probe: claude has no reading; one run to take one",
                "start triage cgwalters-forge/tracker#446 pool=claude",
                "hold pools: claude has no reading; openai is held by its grant; 193 more candidate(s) wait",
            ],
            1,
            0,
        ),
        (
            "probe: one run for two pools on one cold grant pool",
            Box::new(probe_alias),
            vec![
                "probe: claude has no reading; one run to take one",
                "start triage cgwalters-forge/tracker#446 pool=claude",
                "hold pools: claude has no reading; fable has no reading; openai is held by its grant; 193 more candidate(s) wait",
            ],
            1,
            0,
        ),
        (
            "probe: none on a pool whose grant pool has a run in flight",
            Box::new(|r, c| {
                probe_alias(r, c);
                let runs = &mut answer(r, "status=in_progress")["body"]["workflow_runs"];
                for run in runs.as_array_mut().unwrap() {
                    let title = run["display_title"].as_str().unwrap();
                    run["display_title"] = format!("{title} (claude)").into();
                }
            }),
            vec![
                "hold pools: claude has no reading; fable has no reading; openai is held by its grant; new runs wait",
            ],
            0,
            0,
        ),
        (
            "probe: none past a grant that holds",
            Box::new(|r, c| {
                let mut grant = usage::tests::grant();
                grant["hold"] = true.into();
                probe_grant(r, c, json!({"body": grant}));
            }),
            vec![GRANT_HELD],
            0,
            0,
        ),
        (
            "probe: none on an unreadable grant",
            Box::new(|r, c| {
                probe_grant(r, c, json!({"error": "GET /usage/grant: HTTP status 403"}));
            }),
            vec![GRANT_HELD],
            0,
            1,
        ),
        (
            "with a grant that holds, pull requests' fix rounds wait too",
            Box::new(|r, c| {
                granted(r, c, &[("/hold", json!(true))]);
                followed_draft(r, c);
            }),
            vec!["pools: claude held, openai held", GRANT_HELD],
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
            "a sole configured pool takes the run regardless of another pool's headroom",
            Box::new(|r, c| {
                c["triagePools"] = json!(["openai"]);
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
            "an epic outside the epics repository is not live",
            Box::new(|r, _| sub_issue(r, "cgwalters-forge/review", "cgwalters")),
            vec!["candidates: 0 (0 opened by others, 1 outside a live epic)"],
            0,
            0,
        ),
        (
            "inWorkRepositories: the operator's epic in a work repository is live",
            Box::new(|r, c| {
                sub_issue(r, "cgwalters-forge/review", "cgwalters");
                c["epics"] =
                    json!({"repository": "cgwalters-forge/tracker", "inWorkRepositories": true});
            }),
            vec![
                "candidates: 1 (0 opened by others, 0 outside a live epic)",
                "start triage cgwalters-forge/tracker#446 pool=claude",
            ],
            1,
            0,
        ),
        (
            "inWorkRepositories: an epic someone else labelled is not",
            Box::new(|r, c| {
                sub_issue(r, "cgwalters-forge/review", "visitor");
                c["epics"] =
                    json!({"repository": "cgwalters-forge/tracker", "inWorkRepositories": true});
            }),
            vec!["candidates: 0 (0 opened by others, 1 outside a live epic)"],
            0,
            0,
        ),
        (
            "inWorkRepositories: nor is one outside the work repositories",
            Box::new(|r, c| {
                sub_issue(r, "elsewhere/review", "cgwalters");
                c["epics"] =
                    json!({"repository": "cgwalters-forge/tracker", "inWorkRepositories": true});
            }),
            vec!["candidates: 0 (0 opened by others, 1 outside a live epic)"],
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
        let report = crate::pool_choice::with_index(0, || preview(&config(settings), &replay));
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
/// `racing` edits each GraphQL answer, by its query, before then: what
/// changes while the pass reads.
struct Forge {
    replay: Replay,
    added: RefCell<BTreeMap<(String, u64), Vec<Value>>>,
    writes: RefCell<Vec<(String, String, Value)>>,
    fail: RefCell<Option<String>>,
    after_record: bool,
    ids: Cell<u64>,
    login: &'static str,
    late: Option<Late>,
    racing: Option<Late>,
    /// Other runs of the pass's own workflow in progress ([`own`]).
    passes: Vec<Value>,
    /// A confirmed response to workflow dispatch, instead of a successful start.
    dispatch_status: Option<u16>,
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
            racing: None,
            passes: vec![],
            dispatch_status: None,
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
            return Ok((
                201,
                json!({"id": id, "html_url": html_url, "created_at": created}),
            ));
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
            if let Some(status) = self.dispatch_status {
                return Ok((status, json!({"message": "dispatch refused"})));
            }
            return Ok((200, json!({"workflow_run_id": id})));
        }
        if method == "POST" && path.ends_with("/labels") {
            return Ok((200, json!([])));
        }
        if method == "POST" && path.ends_with("/rerun-failed-jobs") {
            return Ok((201, json!({})));
        }
        bail!("unexpected write {method} {url}")
    }
}

impl Broker for Forge {
    fn usage(&self, url: &str) -> Result<Value> {
        self.replay.usage(url)
    }

    fn grant(&self, url: &str) -> Result<Value> {
        self.replay.grant(url)
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

impl crate::github::GraphQl for Forge {
    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        Transport::graphql(self, query, variables)
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
        let mut data = match self.replay.graphql(query, variables.clone()) {
            // An issue whose body was not recorded: the operator's, unedited.
            Err(_) if query == BODY_QUERY => {
                let (repo, number) = (text(&variables, "/name"), &variables["number"]);
                json!({"repository": {"issue": {
                    "url": format!("https://github.com/cgwalters-forge/{repo}/issues/{number}"),
                    "title": "Work", "body": "Work.", "createdAt": "2026-10-01T00:00:00Z", "lastEditedAt": null,
                    "author": {"login": "cgwalters"}, "editor": null}}})
            }
            data => data?,
        };
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
        if let Some(racing) = self
            .racing
            .as_ref()
            .filter(|_| self.dispatches().is_empty())
        {
            racing(query, &mut data);
        }
        Ok(data)
    }
}

fn apply(forge: &Forge, config: &Config) -> Report {
    block(run(
        forge,
        forge,
        config,
        State::default(),
        (Mode::Actions(&own()), Limits::default()),
        forge.replay.now,
    ))
    .unwrap()
}

/// Replay the pass against the atomic ref forge, with a foreign workflow
/// taking a lease on a chosen listing. Its run is never in the caller list.
struct LeasedForge {
    pass: Forge,
    leases: lease::tests::Forge,
    reads: Cell<usize>,
    appear_on: usize,
    item: &'static str,
}

impl Rest for LeasedForge {
    fn get(&self, url: &str, condition: Option<(&str, &str)>) -> Result<RestResponse> {
        if url.contains("/git/")
            || url.ends_with("/actions/runs/123")
            || url == format!("{API_ROOT}repos/cgwalters-forge/tracker")
        {
            if url.contains("/matching-refs/") {
                let n = self.reads.get() + 1;
                self.reads.set(n);
                if n == self.appear_on {
                    self.leases.raw(
                        "agent-board/leases/0",
                        &json!({
                            "run": 123, "item": self.item, "pool": "claude",
                            "taken_at": "2026-10-23T15:00:00Z",
                            "expires_at": "2026-10-24T15:00:00Z"
                        })
                        .to_string(),
                    );
                }
            }
            return self.leases.get(url, condition);
        }
        self.pass.get(url, condition)
    }
    fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        if url.contains("/git/") {
            self.leases.send(method, url, body)
        } else {
            self.pass.send(method, url, body)
        }
    }
}

impl Transport for LeasedForge {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        Transport::graphql(&self.pass, query, variables)
    }
}

impl crate::github::GraphQl for LeasedForge {
    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        Transport::graphql(self, query, variables)
    }
}

impl Broker for LeasedForge {
    fn usage(&self, url: &str) -> Result<Value> {
        self.pass.usage(url)
    }
    fn grant(&self, url: &str) -> Result<Value> {
        self.pass.grant(url)
    }
}

#[test]
fn foreign_leases_hold_capacity_and_normalized_candidate_items() {
    for (appear_on, wip, item, expected) in [
        (2, 1, "o/r#1", "1 of 1 runs active"),
        (1, 3, "CGWALTERS-FORGE/TRACKER#446", "leased by run 123"),
        (2, 3, "CGWALTERS-FORGE/TRACKER#446", "leased by run 123"),
    ] {
        let mut pass = Forge::new(None, false);
        only(&mut pass.replay, &["cgwalters-forge/tracker#446"]);
        for status in ACTIVE_RUNS {
            let body = &mut answer(&mut pass.replay, &format!("status={status}"))["body"];
            body["total_count"] = json!(0);
            body["workflow_runs"] = json!([]);
        }
        let forge = LeasedForge {
            pass,
            leases: lease::tests::forge("cgwalters-bot", &[]),
            reads: Cell::new(0),
            appear_on,
            item,
        };
        let config = config(json!({"leases": true, "wip": wip}));
        let report = block(run(
            &forge,
            &forge,
            &config,
            State::default(),
            (Mode::Actions(&own()), Limits::default()),
            forge.pass.replay.now,
        ))
        .unwrap();
        assert!(forge.pass.dispatches().is_empty(), "{:#?}", report.lines);
        assert_eq!(report.failures, 0, "{:#?}", report.lines);
        assert!(
            report.lines.iter().any(|l| l.contains(expected)),
            "{:#?}",
            report.lines
        );
    }
}

/// A grant used up on both pools holds the pass, and the same grant restored
/// after its windows reset, nothing used yet, releases it.
#[test]
fn a_grant_restored_after_reset_releases_the_pass() {
    for (used, expected) in [
        (40.0, GRANT_HELD),
        (0.0, "start triage cgwalters-forge/tracker#446 pool=claude"),
    ] {
        let mut grant = usage::tests::grant();
        for week in ["anthropic/windows/unified_7d", "codex/windows/secondary"] {
            grant["pools"].pointer_mut(&format!("/{week}")).unwrap()["used_percent"] = used.into();
        }
        let mut replay = without_epics(replay());
        let mut settings = json!({});
        with_grant(&mut replay, &mut settings, json!({"body": grant}));
        settings["triagePools"] = json!(["claude"]);
        let report = preview(&config(settings), &replay);
        assert!(
            report.lines.iter().any(|l| l == expected),
            "{expected}: {:#?}",
            report.lines
        );
    }
}

/// An applying pass publishes the refreshed snapshot to the bot's comment,
/// one write that `--max-writes` holds, and only to a private repository.
#[test]
fn apply_publishes_the_refreshed_usage() {
    let bot_ops = format!("{API_ROOT}repos/cgwalters-forge/bot-ops");
    let edit = format!("PATCH {bot_ops}/issues/comments/1");
    let add = format!("POST {bot_ops}/issues/1/comments");
    let publish = "usage: publish to cgwalters-forge/bot-ops#1";
    let not_private = format!("usage error: {publish}: cgwalters-forge/bot-ops is not private");
    let not_bot = format!("usage error: {publish}: the token is \"someone\"'s");
    // The --max-writes cap, whether the repository is private, the account
    // of a pass that finds no usage comment, and what the pass says and writes.
    type Case<'a> = (
        Option<usize>,
        bool,
        Option<&'a str>,
        String,
        Option<&'a String>,
    );
    let cases: Vec<Case> = vec![
        (Some(1), true, None, "usage: published ".into(), Some(&edit)),
        (
            Some(0),
            true,
            None,
            format!("held by --max-writes: {publish}"),
            None,
        ),
        (None, false, None, not_private, None),
        (
            None,
            true,
            Some("cgwalters-bot"),
            "usage: published ".into(),
            Some(&add),
        ),
        (None, true, Some("someone"), not_bot, None),
    ];
    for (writes, private, account, line, wrote) in cases {
        let mut forge = Forge::new(None, false);
        let mut settings = json!({"wip": 2});
        let broker = json!({"body": usage::tests::broker()});
        with_broker(&mut forge.replay, &mut settings, broker);
        let reply = |body| json!({"status": 200, "link": null, "body": body});
        let mut recorded = vec![(bot_ops.clone(), reply(json!({"private": private})))];
        if let Some(login) = account {
            answer(&mut forge.replay, "bot-ops/issues/1/comments")["body"] = json!([]);
            recorded.push((format!("{API_ROOT}user"), reply(json!({"login": login}))));
        }
        for (url, answer) in recorded {
            let request = json!({"request": {"get": url}, "answer": answer});
            forge.replay.requests.push(request);
        }
        let limits = Limits {
            writes,
            ..Limits::default()
        };
        let now = forge.replay.now;
        let mode = (Mode::Actions(&own()), limits);
        let report = block(run(
            &forge,
            &forge,
            &config(settings),
            State::default(),
            mode,
            now,
        ))
        .unwrap();
        assert!(
            report.lines.iter().any(|l| l.starts_with(&line)),
            "{line}: {:#?}",
            report.lines
        );
        let writes = forge.writes.borrow();
        let usage = writes
            .iter()
            .find(|(m, url, _)| url.starts_with(&bot_ops) && m != "GET")
            .map(|(m, url, body)| (format!("{m} {url}"), body));
        assert_eq!(usage.as_ref().map(|(w, _)| w), wrote, "{line}");
        if let Some((_, body)) = usage {
            let published = usage_snapshot(text(body, "/body")).unwrap();
            let pools = &config(json!({})).pools;
            let expected = usage::snapshot(&usage::tests::broker(), pools, now).unwrap();
            assert_eq!(published, expected);
        }
    }
}

/// With `holds`, an applying pass publishes stages 3 and 4's holds by kind,
/// sorted by item, to the bot's own comment on that issue: one write that
/// `--max-writes` holds, only to a private repository, and none where the
/// comment already has the same holds. tracker#446 is board Blocked and asks
/// a question in its Why, #527 has no Priority; at WIP 2 the two runs in
/// flight hold every new run, and stage 3 still holds its epics. Spend at
/// the weekly budget (WIP 0 below, which reads 8) holds every new run too,
/// without its dollar amounts, counted as held in the summary.
#[test]
fn apply_publishes_the_holds() {
    let bot_ops = format!("{API_ROOT}repos/cgwalters-forge/bot-ops");
    let add = format!("POST {bot_ops}/issues/1/comments");
    let edit = format!("PATCH {bot_ops}/issues/comments/7");
    let publish = "holds: publish to cgwalters-forge/bot-ops#1";
    let not_private = format!("holds error: {publish}: cgwalters-forge/bot-ops is not private");
    let published = "holds: published ".to_owned();
    // The WIP limit, the --max-writes cap, whether the repository is
    // private, the holds of the bot's hold comment if it has one ("same" as
    // the first case published), and what the pass says and writes.
    type Case<'a> = (
        u64,
        Option<usize>,
        bool,
        Option<&'a str>,
        String,
        Option<&'a String>,
    );
    let cases: Vec<Case> = vec![
        (8, None, true, None, published.clone(), Some(&add)),
        (8, None, true, Some("same"), "holds: unchanged".into(), None),
        (8, None, true, Some("old"), published.clone(), Some(&edit)),
        (
            8,
            Some(0),
            true,
            None,
            format!("held by --max-writes: {publish}"),
            None,
        ),
        (8, None, false, None, not_private, None),
        (2, None, true, None, published.clone(), Some(&add)),
        (0, None, true, None, published, Some(&add)),
    ];
    let at = json!({"repository": "cgwalters-forge/bot-ops", "issue": 1});
    let items = "/data/repositoryOwner/projectV2/items/nodes";
    let mut first: Option<String> = None;
    for (wip, writes, private, found, line, wrote) in cases {
        let budget = wip == 0;
        let wip = if budget { 8 } else { wip };
        let mut forge = Forge::new(None, false);
        for entry in &mut forge.replay.requests {
            let nodes = entry["answer"].pointer_mut(items);
            for node in nodes.and_then(Value::as_array_mut).into_iter().flatten() {
                match node["content"]["number"].as_u64() {
                    Some(446) => {
                        node["status"] = json!({"name": "Blocked"});
                        node["why"] = json!({"text": "Q: Ship it?"});
                    }
                    Some(527) => node["priority"] = Value::Null,
                    _ => {}
                }
            }
        }
        let comment = match found {
            Some("same") => first.clone(),
            Some(_) => {
                let old = holds::snapshot(&[], "2026-10-22T00:00:00Z".into(), Default::default());
                Some(holds::render(&old).unwrap())
            }
            None => None,
        };
        if let Some(body) = comment {
            let comments = &mut answer(&mut forge.replay, "bot-ops/issues/1/comments")["body"];
            let comment = json!({"id": 7, "user": {"login": "cgwalters-bot"}, "body": body});
            comments.as_array_mut().unwrap().push(comment);
        }
        let reply = |body| json!({"status": 200, "link": null, "body": body});
        for (url, body) in [
            (bot_ops.clone(), json!({"private": private})),
            (format!("{API_ROOT}user"), json!({"login": "cgwalters-bot"})),
        ] {
            let request = json!({"request": {"get": url}, "answer": reply(body)});
            forge.replay.requests.push(request);
        }
        let mut settings = json!({"wip": wip, "holds": at});
        if budget {
            with_spend(&mut forge.replay, &mut settings, spend(1003.0));
        }
        let limits = Limits {
            writes,
            ..Limits::default()
        };
        let now = forge.replay.now;
        let mode = (Mode::Actions(&own()), limits);
        let config = config(settings);
        let report = block(run(&forge, &forge, &config, State::default(), mode, now)).unwrap();
        assert!(
            report.lines.iter().any(|l| l.starts_with(&line)),
            "{line}: {:#?}",
            report.lines
        );
        assert_eq!(report.failures, usize::from(!private), "{line}");
        let writes = forge.writes.borrow();
        let holds = writes
            .iter()
            // Not the usage snapshot's, which a budget reads from a broker.
            .find(|(m, url, body)| {
                url.starts_with(&bot_ops)
                    && m != "GET"
                    && text(body, "/body").starts_with(holds::HOLDS_MARKER)
            })
            .map(|(m, url, body)| (format!("{m} {url}"), text(body, "/body").to_owned()));
        assert_eq!(holds.as_ref().map(|(w, _)| w), wrote, "{line}");
        let Some((_, body)) = holds else {
            continue;
        };
        assert!(body.starts_with(holds::HOLDS_MARKER), "{body}");
        let snapshot = holds::parse(&body).unwrap();
        assert_eq!(snapshot["at"], now.format(&Rfc3339).unwrap());
        // The WIP limit and count and the pass's summary, as its lines say.
        // Both stop before the board is read.
        let (in_flight, candidates, started) = match (budget, wip) {
            (true, _) | (_, 2) => (2, Value::Null, 0),
            _ => (7, json!(9), 5),
        };
        let held = usize::from(budget);
        assert!(report.summary.ends_with(&format!("held {held}")), "{line}");
        assert!(!body.contains('$'), "{body}");
        if let Some(n) = candidates.as_u64() {
            let said = format!("candidates: {n} (");
            assert!(report.lines.iter().any(|l| l.starts_with(&said)), "{said}");
        }
        let tally = json!({
            "wip": {"limit": wip, "in_flight": in_flight},
            "pass": {"mode": "actions", "candidates": candidates, "started": started,
                     "live_epics": 0},
        });
        assert_eq!(
            json!({"wip": snapshot["wip"], "pass": snapshot["pass"]}),
            tally,
            "{line}"
        );
        // Each entry's reason is what its line says.
        let mut kinds = vec![];
        for hold in snapshot["holds"].as_array().unwrap() {
            let on = match (hold["item"].as_str(), hold["epic"].as_bool()) {
                (None, _) => text(hold, "/kind").to_owned(),
                (Some(item), Some(true)) => format!("epic {item}"),
                (Some(item), _) => item.to_owned(),
            };
            let said = format!("hold {on}: {}", text(hold, "/reason"));
            assert!(report.lines.contains(&said), "{said}: {:#?}", report.lines);
            if let Some(item) = hold["item"].as_str() {
                let (repo, number) = item.split_once('#').unwrap();
                let url = format!("https://github.com/{repo}/issues/{number}");
                assert_eq!(hold["url"], url);
            }
            kinds.push(json!([
                hold["item"],
                hold["kind"],
                hold["operator"],
                hold["priority"],
                hold["question"]
            ]));
        }
        let expected = match wip {
            _ if budget => json!([[null, "budget", false, null, null]]),
            2 => json!([[null, "wip", false, null, null]]),
            _ => json!([
                [
                    "cgwalters-forge/agentic-job#188",
                    "dispatched",
                    false,
                    "P0",
                    null
                ],
                [
                    "cgwalters-forge/tracker#446",
                    "blocked",
                    false,
                    "P0",
                    "Ship it?"
                ],
                [
                    "cgwalters-forge/tracker#468",
                    "dispatched",
                    false,
                    "P0",
                    null
                ],
                ["cgwalters-forge/tracker#527", "overlap", false, null, null],
            ]),
        };
        assert_eq!(Value::Array(kinds), expected, "{line}");
        first.get_or_insert(body);
    }
    // A pass that waits for another in progress leaves that one's holds.
    let url = "https://github.com/cgwalters-forge/tracker/actions/runs/4243";
    let mut forge = Forge::new(None, false);
    forge.passes = vec![json!({"id": 4243, "html_url": url})];
    let report = apply(&forge, &config(json!({"holds": at})));
    let line = format!("holds: not published; another pass is in progress, {url}");
    assert!(report.lines.contains(&line), "{:#?}", report.lines);
    assert!(forge.writes.borrow().is_empty());
    // A preview says what it would write. Stage 3's epics are held too:
    // #766 waits on the operator, and #890 was planned too recently.
    let report = preview(&config(json!({"holds": at})), &replay());
    let line = format!("{publish}; a preview does not write it");
    assert!(report.lines.contains(&line), "{:#?}", report.lines);
    let snapshot = holds::snapshot(&report.holds, String::new(), report.tally.clone());
    assert_eq!(snapshot.pass.mode, "preview");
    assert_eq!(snapshot.pass.live_epics, Some(3));
    let said = "epics: 3 live of ";
    assert!(
        report.lines.iter().any(|l| l.starts_with(said)),
        "{:#?}",
        report.lines
    );
    let epics: Vec<Value> = (snapshot.holds.iter())
        .filter(|h| h.epic)
        .map(|h| json!([h.item, h.kind, h.operator, h.priority]))
        .collect();
    let expected = json!([
        ["cgwalters-forge/tracker#766", "needs-human", true, null],
        [
            "cgwalters-forge/tracker#890",
            "triage-cooldown",
            false,
            "P0"
        ],
    ]);
    assert_eq!(Value::Array(epics), expected);
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
    let report = block(run_within(
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
    ))
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

/// A triage run's answer on tracker#446, `days` from the pass's clock, by
/// `author` and edited by `editor`, for [`Forge`] to read after its record.
fn answered(forge: &Forge, (author, editor): (&str, Option<&str>), body: &str, days: i64) {
    let id = forge.ids.get() + 1;
    forge.ids.set(id);
    let created = (forge.replay.now + time::Duration::days(days))
        .format(&Rfc3339)
        .unwrap();
    let edited = editor.map(|_| created.clone());
    let key = ("cgwalters-forge/tracker".to_owned(), 446);
    forge.added.borrow_mut().entry(key).or_default().push(json!({
        "id": id, "url": format!("https://github.com/cgwalters-forge/tracker/issues/446#issuecomment-{id}"),
        "body": body, "createdAt": created, "lastEditedAt": edited,
        "author": {"login": author}, "editor": editor.map(|e| json!({"login": e})),
    }));
}

/// With `triageFollowUpDays`, a triage that ended without a plan is acted on
/// by its verdict, read only from the bot's answer after the run's record:
/// one for the operator labels the issue, any other waits a day and then
/// triages it again only if something but the bot's comments changed, and
/// the operator's comment triages it again at once. Each case: its answers
/// (author, editor, verdict or body, days from now), `--max-writes`, the
/// line for tracker#446 (`{record}`, `{answer}` and `{due}` filled in),
/// whether it was triaged again and whether it was labelled.
#[test]
fn apply_follows_up_a_triage_by_its_verdict() {
    type Answer = (&'static str, Option<&'static str>, &'static str, i64);
    type Case = (
        &'static str,
        Vec<Answer>,
        Option<usize>,
        &'static str,
        bool,
        bool,
    );
    let verdict = |v: &str| format!("Found.\n\n```{TRIAGE_FENCE}\n{{\"verdict\": \"{v}\"}}\n```\n");
    let bot = "cgwalters-bot";
    let cases: Vec<Case> = vec![
        (
            "done asks the operator to close it, which the pass does not",
            vec![(bot, None, "done", 0)],
            None,
            "hold cgwalters-forge/tracker#446: triage verdict done, {answer}; label needs-human",
            false,
            true,
        ),
        (
            "needs-design asks the operator",
            vec![(bot, None, "needs-design", 0)],
            None,
            "hold cgwalters-forge/tracker#446: triage verdict needs-design, {answer}; label needs-human",
            false,
            true,
        ),
        (
            "--max-writes holds the label",
            vec![(bot, None, "done", 0)],
            Some(0),
            "hold cgwalters-forge/tracker#446: triage verdict done, {answer}; held by --max-writes: label needs-human",
            false,
            false,
        ),
        (
            "partly waits a day",
            vec![(bot, None, "partly", 0)],
            None,
            "hold cgwalters-forge/tracker#446: triage verdict partly, {answer}; triaged again from {due} if anything changed, or once cgwalters comments",
            false,
            false,
        ),
        (
            "partly, its day past and nothing changed, is not triaged again",
            vec![(bot, None, "partly", -4)],
            None,
            "hold cgwalters-forge/tracker#446: triage verdict partly, {answer}; nothing changed since its triage",
            false,
            false,
        ),
        (
            "a plan posted before fingerprints is triaged again after the days, changed or not",
            vec![("", None, "unmarked", 0), (bot, None, "partly", -4)],
            None,
            "start triage cgwalters-forge/tracker#446 pool=claude record=",
            true,
            false,
        ),
        (
            "nor after the bot's own comment",
            vec![(bot, None, "partly", -4), (bot, None, "Still partly.", -3)],
            None,
            "hold cgwalters-forge/tracker#446: triage verdict partly, {answer}; nothing changed since its triage",
            false,
            false,
        ),
        (
            "a comment by anyone else since makes it due",
            vec![
                (bot, None, "partly", -4),
                ("visitor", None, "Fixed by #9?", -3),
            ],
            None,
            "start triage cgwalters-forge/tracker#446 pool=claude record=",
            true,
            false,
        ),
        (
            "the operator's comment after the verdict triages it again, unlabelled",
            vec![
                (bot, None, "done", 0),
                ("cgwalters", None, "Not done: see #2.", 0),
            ],
            None,
            "start triage cgwalters-forge/tracker#446 pool=claude record=",
            true,
            false,
        ),
        (
            "a visitor's comment after it does not",
            vec![
                (bot, None, "partly", 0),
                ("visitor", None, "Please retry.", 0),
            ],
            None,
            "hold cgwalters-forge/tracker#446: triage verdict partly, {answer}",
            false,
            false,
        ),
        (
            "a visitor's verdict is none",
            vec![("visitor", None, "done", 0)],
            None,
            "hold cgwalters-forge/tracker#446: its triage, {record}, gave no verdict; triaged again from",
            false,
            false,
        ),
        (
            "a verdict a visitor edited is none",
            vec![(bot, Some("visitor"), "done", 0)],
            None,
            "hold cgwalters-forge/tracker#446: its triage, {record}, gave no verdict",
            false,
            false,
        ),
        (
            "two verdicts in one answer are none",
            vec![(bot, None, "two", 0)],
            None,
            "hold cgwalters-forge/tracker#446: its triage, {record}, gave no verdict",
            false,
            false,
        ),
    ];
    let config = config(json!({"triageFollowUpDays": 3}));
    for (name, answers, writes, expected, again, labelled) in cases {
        let forge = Forge::new(None, false);
        let first = apply(&forge, &config);
        assert_eq!(first.failures, 0, "{name}: {:#?}", first.lines);
        let comments = forge.comments_on("cgwalters-forge/tracker", 446);
        assert!(
            comments[0].contains("schedule-triage fenced JSON block"),
            "{name}"
        );
        assert!(
            comments[0].contains("\n\n<!-- agent-board-inputs: "),
            "{name}"
        );
        let record = (first.lines.iter())
            .find_map(|l| l.strip_prefix("start triage cgwalters-forge/tracker#446 "))
            .and_then(|l| l.split(" record=").nth(1)?.split(' ').next())
            .unwrap()
            .to_owned();
        for (author, editor, said, days) in answers {
            // A plan posted before fingerprints, without its marker line.
            if said == "unmarked" {
                let key = ("cgwalters-forge/tracker".to_owned(), 446);
                let mut added = forge.added.borrow_mut();
                let plan = &mut added.get_mut(&key).unwrap()[0]["body"];
                let body: String = (plan.as_str().unwrap().split_inclusive('\n'))
                    .filter(|l| !l.starts_with("<!-- agent-board-inputs: "))
                    .collect();
                *plan = body.into();
                continue;
            }
            let body = match said {
                "two" => verdict("done").repeat(2),
                "done" | "needs-design" | "partly" => verdict(said),
                text => text.to_owned(),
            };
            answered(&forge, (author, editor), &body, days);
        }
        let added = forge.added.borrow()[&("cgwalters-forge/tracker".to_owned(), 446)].clone();
        let answer = text(&added[2], "/url").to_owned();
        let due = (forge.replay.now + time::Duration::days(1))
            .format(&Rfc3339)
            .unwrap();
        let expected = expected
            .replace("{record}", &record)
            .replace("{answer}", &answer)
            .replace("{due}", &due);
        let limits = Limits {
            writes,
            ..Limits::default()
        };
        let second = block(run(
            &forge,
            &forge,
            &config,
            State::default(),
            (Mode::Actions(&own()), limits),
            forge.replay.now,
        ))
        .unwrap();
        assert_eq!(second.failures, 0, "{name}: {:#?}", second.lines);
        let line = second.lines.iter().find(|l| l.contains("tracker#446"));
        assert!(
            line.is_some_and(|l| l.starts_with(&expected)),
            "{name}: no line {expected:?} in {:#?}",
            second.lines
        );
        let triaged = forge
            .dispatches()
            .iter()
            .filter(|d| d.ends_with("tracker#446"))
            .count();
        assert_eq!(triaged, 1 + usize::from(again), "{name}");
        let labels: Vec<(String, Value)> = (forge.writes.borrow().iter())
            .filter(|(_, url, _)| url.ends_with("/labels"))
            .map(|(_, url, body)| (url.clone(), body.clone()))
            .collect();
        let label = (
            format!("{API_ROOT}repos/cgwalters-forge/tracker/issues/446/labels"),
            json!({"labels": ["needs-human"]}),
        );
        assert_eq!(
            labels,
            if labelled { vec![label] } else { vec![] },
            "{name}"
        );
    }
}

/// A follow-up triage that had nothing to do either waits twice as long,
/// two days, before the next, even with a new comment; then a third
/// triage starts. Each case: days from now of the second verdict, and
/// whether the third pass triages again.
#[test]
fn apply_doubles_the_wait_after_each_follow_up() {
    let partly = format!("```{TRIAGE_FENCE}\n{{\"verdict\": \"partly\"}}\n```\n");
    let config = config(json!({"triageFollowUpDays": 3}));
    for (days, again) in [(-1, false), (-2, true)] {
        let forge = Forge::new(None, false);
        apply(&forge, &config);
        answered(&forge, ("cgwalters-bot", None), &partly, -4);
        answered(&forge, ("visitor", None), "Fixed by #9?", -4);
        let second = apply(&forge, &config);
        let start = "start triage cgwalters-forge/tracker#446 ";
        let started = |report: &Report| report.lines.iter().any(|l| l.starts_with(start));
        assert!(started(&second), "{:#?}", second.lines);
        answered(&forge, ("cgwalters-bot", None), &partly, days);
        answered(&forge, ("visitor", None), "Or by #10?", days);
        let third = apply(&forge, &config);
        assert_eq!(third.failures, 0, "{:#?}", third.lines);
        assert_eq!(started(&third), again, "{days}: {:#?}", third.lines);
        if !again {
            let due = forge.replay.now + time::Duration::days(1);
            let due = format!("triaged again from {} ", due.format(&Rfc3339).unwrap());
            let line = third.lines.iter().find(|l| l.contains(&due));
            assert!(line.is_some(), "{due}: {:#?}", third.lines);
        }
    }
    let day = FOLLOW_UP_SECONDS;
    assert_eq!(backoff(day, 0, 3 * day), day);
    assert_eq!(backoff(day, 5, 3 * day), 3 * day);
    assert_eq!(backoff(day, 99, i64::MAX), day << 30);
}

/// Gives `repo#number` on the board one open pull request by `author`.
fn closing_pull(replay: &mut Replay, (repo, number): (&str, u64), author: &str) {
    for entry in &mut replay.requests {
        let nodes = entry["answer"]
            .pointer_mut("/data/repositoryOwner/projectV2/items/nodes")
            .and_then(Value::as_array_mut);
        for node in nodes.into_iter().flatten() {
            let content = &mut node["content"];
            if content["number"] == number && text(content, "/repository/nameWithOwner") == repo {
                content["closedByPullRequestsReferences"] = json!({"nodes": [{"number": 900,
                    "state": "OPEN", "repository": {"nameWithOwner": repo},
                    "author": {"login": author}}], "totalCount": 1});
            }
        }
    }
}

/// With `replanAfterDays`, an issue whose implement plan's run ended and
/// that is still open is triaged once, that many days after the plan's
/// dispatch record; a triage plan's record is left to `triageFollowUpDays`.
/// tracker#468's record last changed 2026-10-22T03:49:24Z, a day and a half
/// before the recording. Each case: its settings, its edit of the
/// recording, the issue, the start of its line in a first and in a second
/// applying pass, and the intro of the one triage plan posted, if any.
#[test]
fn apply_replans_an_open_issue_after_its_run() {
    type Case = (
        &'static str,
        Value,
        fn(&mut Replay),
        (&'static str, u64),
        [&'static str; 2],
        Option<&'static str>,
    );
    let held = "hold cgwalters-forge/tracker#468: plan 6332112108@2026-10-22T03:40:56Z was dispatched already, https://github.com/cgwalters-forge/tracker/issues/468#issuecomment-6332216967";
    let tracker = |n| ("cgwalters-forge/tracker", n);
    let cases: Vec<Case> = vec![
        (
            "off, it is dispatched already",
            json!({}),
            |_| {},
            tracker(468),
            [held, held],
            None,
        ),
        (
            "before its days, it waits for them",
            json!({"replanAfterDays": 2}),
            |_| {},
            tracker(468),
            [
                "hold cgwalters-forge/tracker#468: plan 6332112108@2026-10-22T03:40:56Z was dispatched already, https://github.com/cgwalters-forge/tracker/issues/468#issuecomment-6332216967; re-planned from 2026-10-24T03:49:24Z",
                "hold cgwalters-forge/tracker#468: plan 6332112108@2026-10-22T03:40:56Z was dispatched already, https://github.com/cgwalters-forge/tracker/issues/468#issuecomment-6332216967; re-planned from 2026-10-24T03:49:24Z",
            ],
            None,
        ),
        (
            "past them, a triage plans the rest once",
            json!({"replanAfterDays": 1}),
            |_| {},
            tracker(468),
            [
                "start triage cgwalters-forge/tracker#468 pool=claude record=",
                "hold cgwalters-forge/tracker#468: plan ",
            ],
            Some(REPLAN),
        ),
        (
            "a run in flight holds it",
            json!({"replanAfterDays": 1}),
            |r| {
                in_flight(
                    r,
                    vec![
                        json!({"id": 7, "display_title": "implement cgwalters-forge/tracker#468 (claude)", "html_url": "https://github.com/cgwalters-forge/tracker/actions/runs/7"}),
                    ],
                )
            },
            tracker(468),
            [
                "hold cgwalters-forge/tracker#468: a run is in flight, https://github.com/cgwalters-forge/tracker/actions/runs/7",
                "hold cgwalters-forge/tracker#468: a run is in flight, https://github.com/cgwalters-forge/tracker/actions/runs/7",
            ],
            None,
        ),
        (
            "an open bot pull request holds it",
            json!({"replanAfterDays": 1}),
            |r| closing_pull(r, ("cgwalters-forge/tracker", 468), "cgwalters-bot"),
            tracker(468),
            [
                "hold cgwalters-forge/tracker#468: pull request cgwalters-forge/tracker#900 is open",
                "hold cgwalters-forge/tracker#468: pull request cgwalters-forge/tracker#900 is open",
            ],
            None,
        ),
        (
            "a triage plan's record is followed up as before",
            json!({"replanAfterDays": 1, "triageFollowUpDays": 3}),
            |_| {},
            tracker(446),
            [
                "start triage cgwalters-forge/tracker#446 pool=claude record=",
                "hold cgwalters-forge/tracker#446: its triage, ",
            ],
            Some("The board pass asks a triage run to plan this issue."),
        ),
    ];
    for (name, settings, edit, (repo, number), expected, intro) in cases {
        let config = config(settings);
        let mut forge = Forge::new(None, false);
        edit(&mut forge.replay);
        let item = format!("{repo}#{number}");
        for (pass, expected) in expected.into_iter().enumerate() {
            let report = apply(&forge, &config);
            assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
            let line = report
                .lines
                .iter()
                .find(|l| l.contains(&format!("{item}:")) || l.contains(&format!("{item} ")))
                .map_or("", String::as_str);
            let replanned = line.contains("re-planned") && !expected.contains("re-planned");
            assert!(
                line.starts_with(expected) && !replanned,
                "{name}, pass {pass}: no line {expected:?} in {:#?}",
                report.lines
            );
        }
        let started = forge.dispatches().iter().filter(|d| **d == item).count();
        assert_eq!(started, usize::from(intro.is_some()), "{name}");
        let comments = forge.comments_on(repo, number);
        let posted = comments.first().and_then(|c| c.lines().next());
        assert_eq!(posted, intro, "{name}");
    }
}

/// The caller's runs that failed in the day before the recording, as
/// [`Scheduler::failed_runs`] lists them.
fn failed(replay: &mut Replay, runs: Vec<Value>) {
    let since = (replay.now - time::Duration::seconds(RERUN_WINDOW_SECONDS))
        .format(&Rfc3339)
        .unwrap();
    let url = format!(
        "{API_ROOT}repos/cgwalters-forge/tracker/actions/workflows/dispatch.yml/runs?status=failure&created=%3E%3D{since}&per_page=100"
    );
    let body = json!({"total_count": runs.len(), "workflow_runs": runs});
    replay
        .requests
        .push(json!({"request": {"get": url}, "answer": {"status": 200, "body": body}}));
}

/// tracker#468's plan with `from` replaced by `to`, as a writer edited it
/// after its dispatch.
fn replan_468(replay: &mut Replay, from: &str, to: &str) {
    for entry in &mut replay.requests {
        let nodes = (entry["answer"].pointer_mut("/data/repository/issue/comments/nodes"))
            .and_then(Value::as_array_mut);
        for node in nodes.into_iter().flatten() {
            if text(node, "/url").ends_with("#issuecomment-6332112108") {
                node["body"] = text(node, "/body").replace(from, to).into();
            }
        }
    }
}

/// tracker#468's title and body as the overlap check reads them: `body`.
fn text_468(replay: &mut Replay, body: &str) {
    let variables = json!({"owner": "cgwalters-forge", "name": "tracker", "number": 468});
    replay.requests.push(json!({
        "request": graphql_key(overlap::TEXT_QUERY, &variables),
        "answer": {"data": {"repository": {"issue": {"title": "Wake", "body": body}}}},
    }));
}

#[test]
fn apply_reruns_a_failed_run_once() {
    // tracker#468's implement plan was dispatched as run 39718949900.
    let url = "https://github.com/cgwalters-forge/tracker/actions/runs/39718949900";
    let run = |attempt: u64| {
        json!({"id": 39718949900_u64, "display_title": "implement cgwalters-forge/tracker#468 (claude)",
            "conclusion": "failure", "run_attempt": attempt, "html_url": url})
    };
    let held = "hold cgwalters-forge/tracker#468: plan 6332112108@2026-10-22T03:40:56Z was dispatched already";
    let rerun = format!(
        "run rerun cgwalters-forge/tracker#468: its implement run {url} ended (failure), so its failed jobs run once more pool=claude run={url}"
    );
    let again = format!(
        "hold cgwalters-forge/tracker#468: its implement run {url} ended (failure) again after a rerun, so it waits for a person"
    );
    let other = json!({"id": 1, "display_title": "implement cgwalters-forge/tracker#468 (claude)",
        "conclusion": "failure", "run_attempt": 1});
    // Since the run failed: its plan moved to #469, which a person held.
    let held_469: Edit = |r| {
        replan_468(r, r#""item":468,"#, r#""item":469,"#);
        let pages = (r.requests.iter())
            .filter_map(|e| e["answer"].pointer("/data/repositoryOwner/projectV2/items/nodes"))
            .filter_map(Value::as_array);
        // As #468 is on the board, which this test reads alone.
        let node = pages.flatten().find(|n| n["content"]["number"] == 468);
        let mut issue = node.unwrap()["content"].clone();
        issue["number"] = 469.into();
        let labels = &mut issue["labels"];
        labels["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name": "needs-human"}));
        labels["totalCount"] = (labels["totalCount"].as_u64().unwrap() + 1).into();
        let variables = json!({"owner": "cgwalters-forge", "name": "tracker", "number": 469});
        r.requests.push(json!({
            "request": graphql_key(&format!("{ISSUE_QUERY}{FACTS}"), &variables),
            "answer": {"data": {"repository": {"issue": issue}}},
        }));
    };
    let elsewhere: Edit = |r| {
        replan_468(
            r,
            r#""cgwalters-forge/tracker","item""#,
            r#""cgwalters-forge/elsewhere","item""#,
        )
    };
    let stacked: Edit = |r| replan_468(r, r#""item":468,"#, r#""item":468,"stacks_on":467,"#);
    // The recording's open tracker#889 changes paths its plan names: the
    // rerun waits for it, as a start would. Without it, it is re-run.
    let independent: Edit = |r| {
        let pulls = (r.requests.iter_mut())
            .filter_map(|e| e["answer"].pointer_mut("/data/repository/pullRequests/nodes"))
            .filter_map(Value::as_array_mut);
        for nodes in pulls {
            nodes.retain(|n| n["number"] != 889);
        }
    };
    type Edit = fn(&mut Replay);
    let none: Edit = |_| {};
    // Name, settings, the failed runs listed, an edit of the recording, the
    // line and the reruns.
    type Case<'a> = (&'a str, Value, Vec<Value>, Edit, &'a str, usize);
    let cases: Vec<Case> = vec![
        (
            "off, it is held as dispatched",
            json!({}),
            vec![run(1)],
            none,
            held,
            0,
        ),
        (
            "on, its failed run is re-run",
            json!({"reruns": true}),
            vec![run(1)],
            independent,
            &rerun,
            1,
        ),
        (
            "one that failed again holds it",
            json!({"reruns": true}),
            vec![run(2)],
            none,
            &again,
            0,
        ),
        (
            "another run of it is not its plan's",
            json!({"reruns": true}),
            vec![other],
            none,
            held,
            0,
        ),
        (
            "its plan's item held since holds it",
            json!({"reruns": true}),
            vec![run(1)],
            held_469,
            "hold cgwalters-forge/tracker#468: its plan's cgwalters-forge/tracker#469: labelled needs-human",
            0,
        ),
        (
            "so does one outside the work repositories",
            json!({"reruns": true}),
            vec![run(1)],
            elsewhere,
            "hold cgwalters-forge/tracker#468: its plan's cgwalters-forge/elsewhere#468 is not in a work repository",
            0,
        ),
        (
            "and an overlap since",
            json!({"reruns": true}),
            vec![run(1)],
            none,
            "hold cgwalters-forge/tracker#468: Waiting: open pull request cgwalters-forge/tracker#889 changes",
            0,
        ),
        (
            "a stacked plan's run is a person's",
            json!({"reruns": true}),
            vec![run(1)],
            stacked,
            "hold cgwalters-forge/tracker#468: its implement run",
            0,
        ),
    ];
    for (name, settings, runs, edit, expected, reruns) in cases {
        let config = config(settings);
        let mut forge = Forge::new(None, false);
        failed(&mut forge.replay, runs);
        edit(&mut forge.replay);
        text_468(&mut forge.replay, "Work.");
        let report = apply(&forge, &config);
        assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
        let line = (report.lines.iter())
            .find(|l| l.contains("cgwalters-forge/tracker#468:"))
            .map_or("", String::as_str);
        assert!(line.starts_with(expected), "{name}: {:#?}", report.lines);
        let sent = (forge.writes.borrow().iter())
            .filter(|(_, u, _)| u.ends_with("/runs/39718949900/rerun-failed-jobs"))
            .count();
        assert_eq!(sent, reruns, "{name}");
        assert!(
            !forge
                .dispatches()
                .contains(&"cgwalters-forge/tracker#468".to_owned())
        );
    }
}

/// The board as a pass with `revisit` reads it: each recorded page again
/// under [`board_query`]'s query with `updatedAt`, an item `days` (by
/// `OWNER/REPO#N`) before the recording and the rest a day before, and
/// the items of `held` labelled `needs-human` on every page.
fn silent(replay: &mut Replay, days: &[(&str, i64)], held: &[&str]) {
    let plain = fnv(&board_query(false));
    let now = replay.now;
    let mut pages = Vec::new();
    for entry in &mut replay.requests {
        let nodes = entry["answer"]
            .pointer_mut("/data/repositoryOwner/projectV2/items/nodes")
            .and_then(Value::as_array_mut);
        for node in nodes.into_iter().flatten() {
            let content = &mut node["content"];
            let name = format!(
                "{}#{}",
                text(content, "/repository/nameWithOwner"),
                content["number"]
            );
            if held.contains(&name.as_str()) {
                let labels = content["labels"]["nodes"].as_array_mut().unwrap();
                labels.push(json!({"name": NEEDS_HUMAN}));
                content["labels"]["totalCount"] = labels.len().into();
            }
            let ago = days.iter().find(|(n, _)| *n == name).map_or(1, |(_, d)| *d);
            let at = now - time::Duration::days(ago);
            content["updatedAt"] = at.format(&Rfc3339).unwrap().into();
        }
        if entry["request"]["graphql"] == plain.as_str() {
            let mut page = entry.clone();
            page["request"]["graphql"] = fnv(&board_query(true)).into();
            pages.push(page);
        }
    }
    replay.requests.extend(pages);
}

/// Revisits with a slot spare after stage 4's starts: of the recorded
/// candidates left held, agentic-job#188 untouched for 20 days and
/// tracker#468 labelled needs-human for 4 are due, by `afterDays` and
/// `heldAfterDays`; tracker#527, untouched for 4 days and not held, is not.
/// Each gets one revisit, recorded by its plan comment, which the next
/// pass reads and skips. Without a free slot or a pool with room, nothing
/// is revisited.
#[test]
fn apply_revisits_a_sample_with_spare_capacity() {
    let section = json!({"perPass": 3, "afterDays": 14, "heldAfterDays": 3});
    let mut forge = Forge::new(None, false);
    let days = [
        ("cgwalters-forge/agentic-job#188", 20),
        ("cgwalters-forge/tracker#468", 4),
        ("cgwalters-forge/tracker#527", 4),
    ];
    silent(&mut forge.replay, &days, &["cgwalters-forge/tracker#468"]);
    let spare = config(json!({"wip": 12, "revisit": section}));
    let first = apply(&forge, &spare);
    assert_eq!(first.failures, 0, "{:#?}", first.lines);
    let revisits: Vec<&str> = (first.lines.iter())
        .filter(|l| l.contains("revisit"))
        .map(String::as_str)
        .collect();
    assert_eq!(revisits.len(), 3, "{revisits:#?}");
    assert_eq!(
        revisits[0],
        "revisit: 2 due, seed 4242, drawn: cgwalters-forge/agentic-job#188 (weight 4), cgwalters-forge/tracker#468 (weight 4)"
    );
    for (line, item) in revisits[1..].iter().zip(["agentic-job#188", "tracker#468"]) {
        let start = format!("start revisit cgwalters-forge/{item} pool=claude record=");
        assert!(line.starts_with(&start), "{line}");
    }
    assert_eq!(first.tally.1.revisited, Some(Some(2)));
    assert_eq!(first.tally.1.started, 8);
    for (repo, number) in [("agentic-job", 188), ("tracker", 468)] {
        let repo = format!("cgwalters-forge/{repo}");
        let posted = forge.comments_on(&repo, number);
        assert!(posted[0].starts_with(revisit::REVISIT), "{posted:#?}");
        let item = format!("{repo}#{number}");
        assert_eq!(forge.dispatches().iter().filter(|d| **d == item).count(), 1);
    }
    let brief = &forge.comments_on("cgwalters-forge/tracker", 468)[0];
    let held = "nobody has touched it for 4 days, and it is held (labelled needs-human)";
    assert!(brief.contains(held), "{brief}");
    // The next pass reads each revisit's record and leaves the issue alone.
    let second = apply(&forge, &spare);
    assert_eq!(second.failures, 0, "{:#?}", second.lines);
    // Again three days after a held issue's revisit, 14 after another's.
    for (item, again) in [("tracker#468", "10-26"), ("agentic-job#188", "11-06")] {
        let line = (second.lines.iter())
            .find(|l| l.starts_with(&format!("revisit cgwalters-forge/{item}: revisited ")));
        let line = line.unwrap_or_else(|| panic!("{item}: {:#?}", second.lines));
        assert!(
            line.contains(&format!("; again from 2026-{again}T15:")),
            "{line}"
        );
    }
    assert_eq!(second.tally.1.revisited, Some(Some(0)));
    assert_eq!(forge.dispatches().len(), 8);
    // Its days past (all of tracker#468's comments that many days old), a
    // revisit that gave a verdict on an issue nothing but the bot changed
    // since is left alone for up to 90 days. One with no verdict (its run
    // may have failed), or since anyone else's comment or a new title, is
    // due. Each step: days old, (author, comment) added, title, held.
    let key = ("cgwalters-forge/tracker".to_owned(), 468);
    let partly = format!("```{TRIAGE_FENCE}\n{{\"verdict\": \"partly\"}}\n```\n");
    let (bot, visitor) = (
        Some(("cgwalters-bot", partly.as_str())),
        Some(("visitor", "Done in #9?")),
    );
    let steps = [
        (4, None, None, false),
        (4, bot, None, true),
        (91, None, None, false),
        (4, bot, None, true),
        (4, visitor, None, false),
        (4, bot, None, true),
        (4, None, Some("Work, renamed"), false),
    ];
    let item = "revisit cgwalters-forge/tracker#468";
    for (step, (days, said, title, held)) in steps.into_iter().enumerate() {
        let earlier = forge.replay.now - time::Duration::days(days);
        for node in forge.added.borrow_mut().get_mut(&key).unwrap() {
            node["createdAt"] = earlier.format(&Rfc3339).unwrap().into();
        }
        if let Some((author, body)) = said {
            let id = forge.ids.get() + 1;
            forge.ids.set(id);
            let comment = json!({"id": id, "url": format!("https://github.com/cgwalters-forge/tracker/issues/468#issuecomment-{id}"),
                "body": body, "createdAt": earlier.format(&Rfc3339).unwrap(), "lastEditedAt": null,
                "author": {"login": author}, "editor": null});
            forge
                .added
                .borrow_mut()
                .get_mut(&key)
                .unwrap()
                .push(comment);
        }
        if let Some(title) = title {
            let variables = json!({"owner": "cgwalters-forge", "name": "tracker", "number": 468});
            let issue = json!({"url": "https://github.com/cgwalters-forge/tracker/issues/468",
                "title": title, "body": "Work.", "createdAt": "2026-10-01T00:00:00Z",
                "lastEditedAt": null, "author": {"login": "cgwalters"}, "editor": null});
            forge
                .replay
                .requests
                .push(json!({"request": graphql_key(BODY_QUERY, &variables),
                "answer": {"data": {"repository": {"issue": issue}}}}));
        }
        let report = apply(&forge, &spare);
        assert_eq!(report.failures, 0, "{step}: {:#?}", report.lines);
        let expected = match held {
            true => format!("{item}: nothing changed since its revisit, "),
            false => format!("start {item} pool=claude record="),
        };
        let line = report.lines.iter().find(|l| l.starts_with(&expected));
        assert!(line.is_some(), "{step}: {expected}: {:#?}", report.lines);
    }
    // No slot left after stage 4's starts, or no room on the revisit pool.
    for (settings, expected) in [
        (json!({"revisit": section}), "revisit: no slot free"),
        (
            json!({"wip": 12, "revisit": {"perPass": 3, "afterDays": 14, "pools": ["openai"]}}),
            "revisit: no pool: ",
        ),
    ] {
        let mut forge = Forge::new(None, false);
        silent(&mut forge.replay, &days, &["cgwalters-forge/tracker#468"]);
        let report = apply(&forge, &config(settings));
        let line = report.lines.iter().find(|l| l.starts_with(expected));
        assert!(line.is_some(), "{expected}: {:#?}", report.lines);
        let writes = forge.writes.borrow();
        let revisited = writes
            .iter()
            .any(|(_, _, b)| text(b, "/body").starts_with(revisit::REVISIT));
        assert!(!revisited);
        assert_eq!(report.tally.1.revisited, Some(Some(0)));
    }
}

/// Focus first: tracker#446, a child of the live P0 epic tracker#528,
/// weighs 5⁴ × 4 (P0 of three options) × 1, and agentic-job#188, outside
/// any epic, 1 × 4 × 4, though both are P0 and #188 has been silent four
/// times its days; #446 is drawn first and takes the one revisit.
#[test]
fn revisit_draws_by_focus_first() {
    let mut forge = Forge::new(None, false);
    let days = [
        ("cgwalters-forge/agentic-job#188", 60),
        ("cgwalters-forge/tracker#446", 4),
    ];
    silent(&mut forge.replay, &days, &["cgwalters-forge/tracker#446"]);
    let section = json!({"perPass": 1, "afterDays": 14, "heldAfterDays": 3});
    let report = apply(&forge, &config(json!({"wip": 12, "revisit": section})));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    let revisits: Vec<&str> = (report.lines.iter())
        .filter(|l| l.contains("revisit"))
        .map(String::as_str)
        .collect();
    assert_eq!(
        revisits[0],
        "revisit: 2 due, seed 4242, drawn: cgwalters-forge/tracker#446 (weight 2500), cgwalters-forge/agentic-job#188 (weight 16)"
    );
    let start = "start revisit cgwalters-forge/tracker#446 pool=claude record=";
    assert!(revisits[1].starts_with(start), "{revisits:#?}");
    assert_eq!(revisits.len(), 2, "{revisits:#?}");
}

/// Without `revisit` the pass reads the board as it always did and its
/// lines are the same; with it, its stages but the revisits are too.
#[test]
fn revisit_off_changes_nothing() {
    let mut plain = without_epics(replay());
    only(&mut plain, FIRST);
    let mut edited = without_epics(replay());
    only(&mut edited, FIRST);
    silent(&mut edited, &[("cgwalters-forge/agentic-job#188", 20)], &[]);
    let off = config(json!({"wip": 12}));
    let before = preview(&off, &plain);
    assert_eq!(before.lines, preview(&off, &edited).lines);
    assert!(!before.lines.iter().any(|l| l.contains("revisit")));
    assert_eq!(before.tally.1.revisited, None);
    let on = config(json!({"wip": 12, "revisit": {"perPass": 1, "afterDays": 14}}));
    let report = preview(&on, &edited);
    let rest: Vec<&String> = (report.lines.iter())
        .filter(|l| !l.contains("revisit"))
        .collect();
    assert_eq!(rest, before.lines.iter().collect::<Vec<_>>());
    let started = "start revisit cgwalters-forge/agentic-job#188 pool=claude";
    assert!(
        report.lines.iter().any(|l| l == started),
        "{:#?}",
        report.lines
    );
}

/// The digest issue of [`maintained`]'s configuration.
const DIGEST: &str = "https://github.com/cgwalters-forge/bot-ops/issues/9";

/// A head commit of forty hex characters, starting with `start`.
fn head(start: &str) -> String {
    format!("{start:0<40}")
}

/// The bot's comment `id` on the digest issue: the pass's recorded plan
/// for the day from `from`, recording `heads`, where `from` is some, or else
/// a digest whose block records `heads`, as a run could write one.
fn on_digest(id: u64, from: Option<&str>, heads: Value) -> Value {
    let body = match from {
        Some(from) => {
            let block = json!({"schema": maintain::PLAN_SCHEMA, "from": from, "heads": heads});
            let fence = maintain::PLAN_BLOCK;
            format!(
                "{}\nPlan.\n\n```{fence}\n{block}\n```\n",
                maintain::PLAN_MARKER
            )
        }
        None => {
            let approve: Vec<Value> = (heads.as_object().into_iter().flatten())
                .map(|(pr, head)| json!({"pr": pr, "head": head, "reason": "Small; CI green."}))
                .collect();
            let block = json!({"schema": maintain::DIGEST_SCHEMA, "date": "2026-10-22",
                "heads": heads, "approve": approve, "closer": [], "waiting": [], "stale": []});
            format!("Digest.\n\n```{}\n{block}\n```\n", maintain::DIGEST_FENCE)
        }
    };
    json!({"url": format!("{DIGEST}#issuecomment-{id}"), "body": body,
        "createdAt": "2026-10-01T00:00:00Z", "lastEditedAt": null,
        "author": {"login": "cgwalters-bot"}, "editor": null})
}

/// The pass's plan `comment` as the first schema recorded it.
fn v1(mut comment: Value) -> Value {
    let body = text(&comment, "/body").replace(maintain::PLAN_SCHEMA, maintain::OLD_PLAN_SCHEMA);
    comment["body"] = body.into();
    comment
}

/// `comment` edited by `editor`, or by an account GitHub does not name.
fn edited_by(mut comment: Value, editor: Option<&str>) -> Value {
    comment["lastEditedAt"] = json!("2026-10-23T09:30:00Z");
    comment["editor"] = editor.map_or(Value::Null, |login| json!({"login": login}));
    comment
}

/// The address of the search for the operator's pull requests by
/// `qualifier`, updated in the 90 days before the recording's clock.
fn operator_search(qualifier: &str) -> String {
    let query = format!("is:pr is:public {qualifier}:cgwalters updated:>=2026-07-25");
    let query: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    format!("{API_ROOT}search/issues?q={query}&sort=updated&order=desc&per_page=100&page=1")
}

/// The repositories the searches find, with how many of the operator's pull
/// requests each: up/a 3 (#2 found by both), up/c 3, up/b 2, low/d 1, and
/// 2 each in the operator's own account and the forge organization.
const SEARCHED: &[(&str, &[(&str, u64)])] = &[
    (
        "author",
        &[
            ("up/a", 1),
            ("up/a", 2),
            ("up/b", 1),
            ("cgwalters-forge/x", 1),
            ("cgwalters-forge/x", 2),
            ("cgwalters/y", 1),
            ("cgwalters/y", 2),
        ],
    ),
    (
        "reviewed-by",
        &[
            ("up/a", 2),
            ("up/a", 3),
            ("up/b", 2),
            ("up/c", 1),
            ("up/c", 2),
            ("up/c", 3),
            ("low/d", 1),
        ],
    ),
];

/// The repositories the open pull requests are listed of: `up/a`'s are #1
/// and #2 at heads `aa…` and `bb…` and the operator's #3; the others have none.
const LISTED: &[&str] = &["up/a", "up/b", "up/c", "inc/z"];

/// The recording with `maintain` on (repositories derived as `repos` says
/// from [`SEARCHED`] and [`LISTED`], due from `hour` UTC, a share of 0.2),
/// its digest issue holding `comments`, and claude having used `used`
/// percent of its 54.3% allowance.
fn maintained_with(hour: u8, comments: Vec<Value>, used: f64, repos: Value) -> (Config, Replay) {
    let mut replay = without_epics(replay());
    only(&mut replay, FIRST);
    let variables =
        json!({"owner": "cgwalters-forge", "name": "bot-ops", "number": 9, "after": null});
    replay.requests.push(json!({
        "request": graphql_key(plan::COMMENTS_QUERY, &variables),
        "answer": {"data": {"repository": {"issue": {"comments": {"nodes": comments,
            "pageInfo": {"hasNextPage": false, "endCursor": null}}}}}},
    }));
    let mut get = |url: String, body: Value| {
        replay.requests.push(json!({
            "request": {"get": url},
            "answer": {"status": 200, "link": null, "body": body},
        }));
    };
    for (qualifier, found) in SEARCHED {
        let items: Vec<Value> = (found.iter())
            .map(|(repo, number)| json!({"number": number, "repository_url": format!("{API_ROOT}repos/{repo}")}))
            .collect();
        get(operator_search(qualifier), json!({"items": items}));
    }
    let pull = |number: u64, login: &str, sha: &str| json!({"number": number, "user": {"login": login}, "head": {"sha": head(sha)}});
    for repo in LISTED {
        get(format!("{API_ROOT}repos/{repo}"), visible(true));
        let pulls = match *repo {
            "up/a" => json!([
                pull(1, "visitor", "aa"),
                pull(2, "contributor", "bb"),
                pull(3, "cgwalters", "cc"),
            ]),
            _ => json!([]),
        };
        get(
            format!("{API_ROOT}repos/{repo}/pulls?state=open&per_page=100"),
            pulls,
        );
    }
    let mut snapshot = usage(&mut replay);
    snapshot["pools"]["claude"]["used_percent"] = used.into();
    set_usage(&mut replay, &snapshot);
    let section = json!({"repos": repos, "share": 0.2, "hourUtc": hour,
        "digest": {"repository": "cgwalters-forge/bot-ops", "issue": 9}});
    (config(json!({"maintain": section})), replay)
}

/// [`maintained_with`] up/a alone: at least 3 pull requests, up/c left out.
fn maintained(hour: u8, comments: Vec<Value>, used: f64) -> (Config, Replay) {
    let repos = json!({"sinceDays": 90, "min": 3, "max": 2, "exclude": ["up/c"]});
    maintained_with(hour, comments, used, repos)
}

/// The repositories are those with at least `min` of the operator's pull
/// requests, never in the operator's, the bot's or the forge's own
/// organizations, `include` first, `exclude` left out, the `max` most
/// active kept, ties by name.
#[test]
fn maintain_derives_its_repositories() {
    let cases = [
        (json!({}), "up/a, up/c, up/b"),
        (json!({"min": 3}), "up/a, up/c"),
        (json!({"max": 1}), "up/a"),
        (json!({"min": 1, "max": 20}), "up/a, up/c, up/b, low/d"),
        (json!({"include": ["inc/z"], "max": 2}), "inc/z, up/a"),
        (json!({"exclude": ["UP/A"]}), "up/c, up/b"),
    ];
    for (edit, expected) in cases {
        let mut repos = json!({"sinceDays": 90, "min": 2, "max": 3});
        repos
            .as_object_mut()
            .unwrap()
            .extend(edit.as_object().unwrap().clone());
        let (config, mut replay) = maintained_with(9, vec![], 40.0, repos);
        let low = format!("{API_ROOT}repos/low/d/pulls?state=open&per_page=100");
        replay
            .requests
            .push(json!({"request": {"get": format!("{API_ROOT}repos/low/d")},
            "answer": {"status": 200, "link": null, "body": visible(true)}}));
        replay.requests.push(json!({"request": {"get": low},
            "answer": {"status": 200, "link": null, "body": []}}));
        let report = preview(&config, &replay);
        assert_eq!(report.failures, 0, "{edit}: {:#?}", report.lines);
        let lines = maintain_lines(&report);
        assert_eq!(
            lines[0],
            format!("maintain: repositories: {expected}"),
            "{edit}"
        );
        let day = report.maintain.unwrap();
        assert_eq!(day.repos.join(", "), expected, "{edit}");
    }
}

/// A search GitHub says is incomplete, or one that reaches its 1,000
/// results, is an error, not a ranking the day settles on.
#[test]
fn maintain_refuses_a_partial_search() {
    let cases = [
        (
            "incomplete",
            1,
            "maintain error: searching the operator's pull requests (author): GitHub's results are incomplete",
        ),
        (
            "capped",
            10,
            "maintain error: searching the operator's pull requests (author): reaches GitHub's 1000 results; \
             lower maintain repos sinceDays",
        ),
    ];
    for (name, pages, expected) in cases {
        let (config, mut replay) = maintained(9, vec![], 40.0);
        let first = operator_search("author");
        let item = json!({"number": 1, "repository_url": format!("{API_ROOT}repos/up/a")});
        let body = match pages {
            1 => json!({"incomplete_results": true, "items": []}),
            _ => json!({"incomplete_results": false, "items": vec![item; 100]}),
        };
        answer(&mut replay, &first)["body"] = body.clone();
        for page in 2..=pages {
            let url = first.replace("&page=1", &format!("&page={page}"));
            replay.requests.push(json!({"request": {"get": url},
                "answer": {"status": 200, "link": null, "body": body}}));
        }
        let report = preview(&config, &replay);
        assert_eq!(report.failures, 1, "{name}: {:#?}", report.lines);
        assert_eq!(maintain_lines(&report), [expected], "{name}");
        assert_eq!(report.maintain, None, "{name}");
    }
}

/// Its lines that start with `maintain`.
fn maintain_lines(report: &Report) -> Vec<&str> {
    let lines = report.lines.iter().map(String::as_str);
    lines.filter(|l| l.starts_with("maintain")).collect()
}

/// The start of the recording's day's due time, and of the next day's.
const TODAY: &str = "2026-10-23T09:00:00Z";
const NEXT: &str = "2026-10-24T09:00:00Z";

/// A locally retained creation receipt, not a marker discovered on GitHub.
fn maintain_receipt() -> State {
    State {
        maintain: Some(maintain::Day {
            from: "2026-10-22T09:00:00Z".into(),
            repos: vec![],
            done: false,
            configuration: None,
            comment: Some(format!("{DIGEST}#issuecomment-5")),
        }),
        ..State::default()
    }
}

/// A day's digest is due once, from `hourUtc`, for the pull requests whose
/// head changed since the pass last recorded a plan, the operator's and the
/// bot's left out. Only the pass's own plan comment, with its locally kept
/// creation receipt, is read, and only while nobody else edited it: heads a run
/// or a collaborator wrote, in a digest, a later comment like the pass's or
/// an edit of it, are not.
#[test]
fn maintain_is_due_once_a_day_for_changed_heads() {
    let both = json!({"up/a#1": "aa0000000000", "up/a#2": "bb0000000000"});
    let one = json!({"up/a#1": "aa0000000000", "up/a#2": "b10000000000"});
    let yesterday = Some("2026-10-22T09:00:00Z");
    let all_due = "maintain: digest due: 2 of 2 incoming pull request(s) changed since no plan; reviews up/a#1, up/a#2";
    let one_due = format!(
        "maintain: digest due: 1 of 2 incoming pull request(s) changed since {DIGEST}#issuecomment-5; reviews up/a#2"
    );
    let cases = [
        (
            "before the hour",
            16,
            vec![],
            "maintain: not due until 2026-10-23T16:00:00Z".to_owned(),
        ),
        ("no plan yet", 9, vec![], all_due.to_owned()),
        (
            "recorded today",
            9,
            vec![on_digest(5, Some(TODAY), both.clone())],
            format!(
                "maintain: today's plan is {DIGEST}#issuecomment-5; the next is due from {NEXT}"
            ),
        ),
        (
            "unchanged since yesterday's",
            9,
            vec![on_digest(5, yesterday, both.clone())],
            format!(
                "maintain: none of 2 incoming pull request(s) changed since {DIGEST}#issuecomment-5"
            ),
        ),
        (
            "one head changed",
            9,
            vec![on_digest(5, yesterday, one.clone())],
            one_due.clone(),
        ),
        (
            "a run's digest records heads",
            9,
            vec![on_digest(5, None, both.clone())],
            all_due.to_owned(),
        ),
        (
            "a run's later comment like the pass's",
            9,
            vec![
                on_digest(5, yesterday, one),
                on_digest(6, Some(TODAY), both.clone()),
            ],
            one_due,
        ),
        (
            "another account's comment like the pass's",
            9,
            vec![{
                let mut comment = on_digest(5, Some(TODAY), both.clone());
                comment["author"] = json!({"login": "collaborator"});
                comment
            }],
            all_due.to_owned(),
        ),
        (
            "the bot's edit",
            9,
            vec![edited_by(
                on_digest(5, yesterday, both.clone()),
                Some("cgwalters-bot"),
            )],
            format!(
                "maintain: none of 2 incoming pull request(s) changed since {DIGEST}#issuecomment-5"
            ),
        ),
        (
            "another account's edit",
            9,
            vec![edited_by(
                on_digest(5, Some(TODAY), both.clone()),
                Some("collaborator"),
            )],
            format!(
                "maintain: digest due: 2 of 2 incoming pull request(s) changed since {DIGEST}#issuecomment-5, whose heads are unreadable: it was last edited by collaborator, not the bot; reviews up/a#1, up/a#2"
            ),
        ),
        (
            "an unnamed account's edit",
            9,
            vec![edited_by(on_digest(5, yesterday, both.clone()), None)],
            format!(
                "maintain: digest due: 2 of 2 incoming pull request(s) changed since {DIGEST}#issuecomment-5, whose heads are unreadable: it was last edited by an unknown account, not the bot; reviews up/a#1, up/a#2"
            ),
        ),
        (
            "a v1 record's heads, never assessed",
            9,
            vec![v1(on_digest(5, yesterday, both.clone()))],
            format!(
                "maintain: digest due: 2 of 2 incoming pull request(s) changed since {DIGEST}#issuecomment-5; reviews up/a#1, up/a#2"
            ),
        ),
        (
            "a v1 record today",
            9,
            vec![v1(on_digest(5, Some(TODAY), both.clone()))],
            format!(
                "maintain: today's plan is {DIGEST}#issuecomment-5; the next is due from {NEXT}"
            ),
        ),
        (
            "unreadable",
            9,
            vec![on_digest(5, yesterday, json!(["aa"]))],
            format!(
                "maintain: digest due: 2 of 2 incoming pull request(s) changed since {DIGEST}#issuecomment-5, whose heads are unreadable: it has no heads; reviews up/a#1, up/a#2"
            ),
        ),
    ];
    for (name, hour, comments, expected) in cases {
        let (config, replay) = maintained(hour, comments, 40.0);
        let state = if hour == 9 {
            maintain_receipt()
        } else {
            State::default()
        };
        let report = block(run(
            &replay,
            &replay,
            &config,
            state,
            (Mode::Preview, Limits::default()),
            replay.now,
        ))
        .unwrap();
        assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
        let lines = maintain_lines(&report);
        let due = hour == 9;
        if due {
            assert_eq!(lines[0], "maintain: repositories: up/a", "{name}");
        }
        assert_eq!(lines[usize::from(due)], expected, "{name}");
        let planned = lines.len() == 4;
        assert_eq!(
            planned,
            expected.contains("digest due"),
            "{name}: {lines:#?}"
        );
        // A preview records no plan: the day is not done.
        let done = !planned && due;
        assert_eq!(
            report.maintain.map(|d| d.done),
            due.then_some(done),
            "{name}"
        );
    }
}

/// The plan names the digest issue as its item, asks for a run that hands
/// back a comment, forbids any write to the pull requests' repositories and
/// any mention of embargoed work, and leaves the recorded heads out: the
/// pass records them itself. It gives each pull request's full head, the
/// day's date, and an example of the digest review's Maintain view reads,
/// which is the fixture checked against review's parser.
/// A preview prints it for a nonpublic digest, and otherwise previews a start.
#[test]
fn maintain_plans_a_read_only_digest() {
    let not_public = "maintain: plan only; cgwalters-forge/bot-ops is not public: ";
    let unread = "maintain: plan only; cannot read whether cgwalters-forge/bot-ops is public: ";
    // The digest repository as GitHub answers it, if it does, and the start
    // of the line the preview prints for the digest.
    let cases = [
        (Some(visible(false)), not_public),
        (
            Some(json!({"private": true, "visibility": "internal"})),
            not_public,
        ),
        (None, unread),
        (Some(json!({"private": false})), unread),
        (
            Some(visible(true)),
            "maintain: start research cgwalters-forge/bot-ops#9 pool=claude",
        ),
    ];
    let mut plans = vec![];
    for (visibility, expected) in cases {
        let (config, mut replay) = maintained(9, vec![], 40.0);
        if let Some(body) = &visibility {
            digest_visibility(&mut replay, body.clone());
        }
        let report = preview(&config, &replay);
        let lines = maintain_lines(&report);
        assert!(lines[2].starts_with(expected), "{visibility:?} {lines:#?}");
        assert_eq!(
            lines[3],
            "maintain: record the plan on cgwalters-forge/bot-ops#9; a preview records nothing"
        );
        if let Some(at) = lines[2].find(": {\"") {
            plans.push(lines[2][at + 2..].to_owned());
        }
    }
    let [plan, ..] = &plans[..] else {
        panic!("no plan printed")
    };
    assert_eq!(plans.len(), 4);
    let plan: Value = serde_json::from_str(plan).unwrap();
    let parsed = plan::JobPlan::from_json(&plan).unwrap();
    assert_eq!(
        (parsed.repo.as_str(), parsed.item, parsed.kind.as_str()),
        ("cgwalters-forge/bot-ops", 9, "research")
    );
    assert_eq!(parsed.pool, "claude");
    for needle in [
        "Never comment on, review, approve, label, close, push to or otherwise write to any pull request \
         or anything else in those repositories",
        "Your only output is one new comment on cgwalters-forge/bot-ops#9",
        "Never mention embargoed",
        &format!(
            "- https://github.com/up/a/pull/1 (up/a#1, head {})",
            head("aa")
        ),
        &format!(
            "- https://github.com/up/a/pull/2 (up/a#2, head {})",
            head("bb")
        ),
        "whose checks all passed at the head listed below, with a reason that says so",
        "`date` is 2026-10-23.",
        "the full head listed above, copied exactly",
    ] {
        assert!(parsed.brief.contains(needle), "{needle}: {}", parsed.brief);
    }
    for needle in ["up/a#3", "\"up/a#1\"", "heads\"", "aa0000000000)"] {
        assert!(!parsed.brief.contains(needle), "{needle}: {}", parsed.brief);
    }
    let fixture = include_str!("../../../../fixtures/pass/maintain-digest.md");
    let fixture = plan::fenced_json(fixture, maintain::DIGEST_FENCE).unwrap();
    let shown = plan::fenced_json(&parsed.brief, maintain::DIGEST_FENCE).unwrap();
    assert_eq!(shown, fixture);
    let fixture = fixture.unwrap();
    assert_eq!(fixture["schema"], maintain::DIGEST_SCHEMA);
    for group in ["approve", "closer", "waiting", "stale"] {
        assert!(!fixture[group].as_array().unwrap().is_empty(), "{group}");
    }
}

/// A repository as GitHub names it: public, or private.
fn visible(public: bool) -> Value {
    let visibility = if public { "public" } else { "private" };
    json!({"private": !public, "visibility": visibility})
}

/// The recording answering the read of the digest repository with `body`.
fn digest_visibility(replay: &mut Replay, body: Value) {
    replay.requests.push(json!({
        "request": {"get": format!("{API_ROOT}repos/cgwalters-forge/bot-ops")},
        "answer": {"status": 200, "link": null, "body": body},
    }));
}

/// An applying pass starts the digest's run only where the digest
/// repository is public and a WIP slot is free: it posts the plan on the
/// digest issue, dispatches it and records the heads there, and the day is
/// done. A private repository, or one whose visibility it cannot read, gets
/// the recorded plan alone; with no slot free, nothing is written, and a
/// later pass tries again.
#[test]
fn maintain_dispatches_only_a_public_digest() {
    let started = "maintain: start research cgwalters-forge/bot-ops#9 pool=claude record=";
    // The recording has 2 runs in flight.
    let waits = "maintain: wip: 2 of 2 runs active; the digest waits";
    let unread = "maintain: plan only; cannot read whether cgwalters-forge/bot-ops is public: ";
    // The repository's answer, the WIP limit, and the line the pass says.
    let cases = [
        (visible(true), 8, started),
        (
            visible(false),
            8,
            "maintain: plan only; cgwalters-forge/bot-ops is not public: ",
        ),
        // Read without its visibility: private by `private`, so the plan
        // is recorded, but it is not read as public.
        (json!({"private": true}), 8, unread),
        (visible(true), 2, waits),
    ];
    for (visibility, wip, expected) in cases {
        let name = format!("{visibility} wip {wip}");
        let (mut config, mut replay) = maintained(9, vec![], 40.0);
        config.wip = wip;
        digest_visibility(&mut replay, visibility);
        replay
            .requests
            .push(json!({"request": {"get": format!("{API_ROOT}user")},
            "answer": {"status": 200, "link": null, "body": {"login": "cgwalters-bot"}}}));
        let forge = Forge {
            replay,
            ..Forge::new(None, false)
        };
        let report = apply(&forge, &config);
        assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
        let lines = maintain_lines(&report);
        assert!(lines[2].starts_with(expected), "{name}: {lines:#?}");
        let dispatched = expected == started;
        let digests = (forge.dispatches().into_iter())
            .filter(|d| d == "cgwalters-forge/bot-ops#9")
            .count();
        assert_eq!(digests, usize::from(dispatched), "{name}");
        let comments = forge.comments_on("cgwalters-forge/bot-ops", 9);
        let plans: Vec<Value> = (comments.iter())
            .filter_map(|c| plan::fenced_json(c, PLAN_FENCE).unwrap())
            .collect();
        assert_eq!(plans.len(), usize::from(dispatched), "{name}");
        if let [plan] = &plans[..] {
            assert_eq!(
                (&plan["kind"], &plan["item"]),
                (&json!("research"), &json!(9))
            );
        }
        let recorded = comments
            .iter()
            .any(|c| c.starts_with(maintain::PLAN_MARKER));
        assert_eq!(recorded, expected != waits, "{name}");
        assert_eq!(report.maintain.unwrap().done, expected != waits, "{name}");
        let writes = forge.writes.borrow();
        assert!(
            !writes.iter().any(|(_, url, _)| url.contains("up/a")),
            "{name}"
        );
    }
}

/// Includes and a reused ranking must both pass a fresh visibility read,
/// before their names or pull request material reaches public output.
#[test]
fn maintain_rejects_nonpublic_sources() {
    for cached in [false, true] {
        for (status, body) in [
            (200, visible(false)),
            (200, json!({"private": false, "visibility": "internal"})),
            (200, json!({"private": false})),
            (404, json!({})),
        ] {
            let (mut config, mut replay) = maintained(9, vec![], 40.0);
            config.maintain.as_mut().unwrap().repos.include = vec!["up/a".into()];
            digest_visibility(&mut replay, visible(true));
            let mut state = State::default();
            if cached {
                state.maintain = preview(&config, &replay).maintain;
            }
            let response = answer(&mut replay, &format!("{API_ROOT}repos/up/a"));
            response["status"] = status.into();
            response["body"] = body;
            let forge = Forge {
                replay,
                ..Forge::new(None, false)
            };
            let report = block(run(
                &forge,
                &forge,
                &config,
                state,
                (Mode::Actions(&own()), Limits::default()),
                forge.replay.now,
            ))
            .unwrap();
            assert_eq!(report.failures, 1, "{:#?}", report.lines);
            assert_eq!(
                maintain_lines(&report),
                ["maintain error: a maintain source repository is nonpublic or unreadable"]
            );
            assert!(forge.comments_on("cgwalters-forge/bot-ops", 9).is_empty());
        }
    }
}

#[test]
fn maintain_resolves_a_ref_without_work_or_follow() {
    let (mut config, mut replay) = maintained(9, vec![], 40.0);
    config.work_repositories.clear();
    config.follow.clear();
    config.dispatch_ref = None;
    digest_visibility(&mut replay, visible(true));
    replay
        .requests
        .push(json!({"request": {"get": format!("{API_ROOT}user")},
        "answer": {"status": 200, "link": null, "body": {"login": "cgwalters-bot"}}}));
    replay.requests.push(
        json!({"request": {"get": format!("{API_ROOT}repos/cgwalters-forge/tracker")},
        "answer": {"status": 200, "link": null, "body": {"default_branch": "main"}}}),
    );
    let forge = Forge {
        replay,
        ..Forge::new(None, false)
    };
    let report = apply(&forge, &config);
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    assert_eq!(forge.dispatches(), ["cgwalters-forge/bot-ops#9"]);
    let writes = forge.writes.borrow();
    let (_, _, body) = writes
        .iter()
        .find(|(_, url, _)| url.ends_with("/dispatches"))
        .unwrap();
    assert_eq!(body["ref"], "main");
}

/// Maintain runs before stage 4, but must use the same admission gate before
/// posting a plan: another pass may take the last free slot after our read.
#[test]
fn maintain_waits_for_an_overlapping_pass() {
    for unreadable in [false, true] {
        let (mut config, mut replay) = maintained(9, vec![], 40.0);
        config.wip = 3; // Two dispatch runs are active; one slot is free.
        config.work_repositories.clear();
        digest_visibility(&mut replay, visible(true));
        let forge = Forge {
            replay,
            passes: vec![json!({"id": 4243,
                "html_url": "https://github.com/cgwalters-forge/tracker/actions/runs/4243"})],
            ..Forge::new(unreadable.then_some("workflows/77/runs?"), false)
        };
        let report = apply(&forge, &config);
        let why = if unreadable {
            "whether another pass is in progress is unknown"
        } else {
            "another pass is in progress, https://github.com/cgwalters-forge/tracker/actions/runs/4243"
        };
        assert!(
            maintain_lines(&report)
                .iter()
                .any(|l| l.starts_with(&format!("maintain: {why}"))),
            "{:#?}",
            report.lines
        );
        assert!(!report.maintain.unwrap().done);
        assert!(forge.writes.borrow().is_empty());
    }
}

/// Failed preparation must not mark heads reviewed; lost send responses
/// still settle the day. Discovered plans only veto recovery, never supply
/// trusted heads, including when the poll cache is lost after a public run.
#[test]
fn maintain_dispatch_failures_and_cache_loss() {
    for case in ["writer", "readback", "403", "422", "500", "send", "success"] {
        let (mut config, mut replay) = maintained(9, vec![], 40.0);
        config.work_repositories.clear();
        digest_visibility(&mut replay, visible(true));
        let login = if case == "writer" {
            "visitor"
        } else {
            "cgwalters-bot"
        };
        replay
            .requests
            .push(json!({"request": {"get": format!("{API_ROOT}user")},
            "answer": {"status": 200, "link": null, "body": {"login": login}}}));
        let forge = Forge {
            replay,
            login,
            dispatch_status: case.parse().ok(),
            ..Forge::new(
                (case == "send").then_some("/dispatches"),
                case == "readback",
            )
        };
        let report = apply(&forge, &config);
        let sent = matches!(case, "500" | "send" | "success");
        assert_eq!(
            report.failures,
            usize::from(case != "success"),
            "{case}: {:#?}",
            report.lines
        );
        assert_eq!(report.maintain.as_ref().unwrap().done, sent, "{case}");
        let recorded = forge
            .comments_on("cgwalters-forge/bot-ops", 9)
            .iter()
            .any(|c| c.starts_with(maintain::PLAN_MARKER));
        assert_eq!(recorded, sent, "{case}");
        let before = forge.dispatches();
        let retry = apply(&forge, &config);
        assert_eq!(retry.failures, 0, "{case}: {:#?}", retry.lines);
        assert!(
            maintain_lines(&retry)
                .contains(&"maintain: a research plan already exists today; the digest waits"),
            "{case}: {:#?}",
            retry.lines
        );
        assert_eq!(forge.dispatches(), before, "{case}");
    }
}

/// An applying pass records the plan and the heads it was planned for in
/// its own comment on a private digest issue, and writes nothing else;
/// nothing is dispatched. Later passes the same day read its state and search, read
/// and list nothing; one whose state was lost trusts no existing marker.
/// Before it records a plan, a pass reuses the day's
/// repositories and does not search again.
#[test]
fn maintain_records_its_plan_and_lists_once_a_day() {
    let (config, _) = maintained(9, vec![], 40.0);
    let recording = || {
        let (_, mut replay) = maintained(9, vec![], 40.0);
        for (url, body) in [
            ("repos/cgwalters-forge/bot-ops", visible(false)),
            ("user", json!({"login": "cgwalters-bot"})),
        ] {
            replay
                .requests
                .push(json!({"request": {"get": format!("{API_ROOT}{url}")},
                "answer": {"status": 200, "link": null, "body": body}}));
        }
        replay
    };
    let forge = Forge {
        replay: recording(),
        ..Forge::new(None, false)
    };
    let report = apply(&forge, &config);
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    let lines = maintain_lines(&report);
    let recorded = "maintain: recorded the plan in https://github.com/cgwalters-forge/bot-ops/issues/9#issuecomment-";
    assert!(lines[3].starts_with(recorded), "{lines:#?}");
    let writes = forge.writes.borrow();
    let posted: Vec<&Value> = (writes.iter())
        .filter(|(m, url, _)| m == "POST" && url.ends_with("bot-ops/issues/9/comments"))
        .map(|(_, _, body)| body)
        .collect();
    let [posted] = posted[..] else {
        panic!("{writes:#?}")
    };
    let body = text(posted, "/body");
    assert!(body.starts_with(maintain::PLAN_MARKER), "{body}");
    // The brief's example stays inside a JSON string: the bot's plan comment
    // holds no digest block that review's Maintain view could show.
    assert_eq!(
        plan::fenced_json(body, maintain::DIGEST_FENCE).unwrap(),
        None
    );
    let record = plan::fenced_json(body, maintain::PLAN_BLOCK)
        .unwrap()
        .unwrap();
    // A plan only acknowledges nothing: no run assessed its pull requests.
    assert_eq!(
        (&record["from"], &record["heads"], &record["pending"]),
        (
            &json!(TODAY),
            &json!({}),
            &json!({"plan": null, "heads": {"up/a#1": head("aa"), "up/a#2": head("bb")}})
        )
    );
    assert_eq!(record["plan"]["item"], 9);
    assert!(!writes.iter().any(|(_, url, _)| url.contains("up/a")));
    assert!(!forge.dispatches().iter().any(|d| d.contains("bot-ops")));
    let day = report.maintain.clone().unwrap();
    let expected = day.clone();
    assert_eq!(day.from, TODAY);
    assert_eq!(day.repos, ["up/a"]);
    assert!(day.done);
    assert!(day.configuration.is_some());
    assert!(
        day.comment
            .as_ref()
            .unwrap()
            .starts_with(&format!("{DIGEST}#issuecomment-"))
    );
    // Kept in the state, though no pull request is followed.
    let kept = report.state.as_ref().and_then(|s| s.maintain.as_ref());
    assert_eq!(kept, Some(&expected));
    let state = State::from_json(&report.state.as_ref().unwrap().to_json()).unwrap();
    assert_eq!(state.maintain.as_ref(), Some(&expected));
    // The recording without the requests that contain any of `needles`,
    // which a pass over it must not make.
    let without = |needles: &[&str]| {
        let mut replay = recording();
        replay.requests.retain(|e| {
            let request = e["request"].to_string();
            !needles.iter().any(|n| request.contains(n))
        });
        replay
    };
    let (search, comments, list) = ("search/issues", r#""name":"bot-ops","number":9"#, "/pulls?");
    let quiet = without(&[search, comments, list]);
    let later = |replay: &Replay, state: State| {
        block(run(
            replay,
            replay,
            &config,
            state,
            (Mode::Preview, Limits::default()),
            replay.now,
        ))
        .unwrap()
    };
    let report = later(&quiet, state);
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    assert_eq!(
        maintain_lines(&report),
        [format!(
            "maintain: done for today; the next is due from {NEXT}"
        )]
    );
    // The state lost, it searches and lists again, ignoring even an older
    // forged marker claiming today's plan and matching heads.
    let mut lost = recording();
    answer(&mut lost, comments)["data"]["repository"]["issue"]["comments"]["nodes"] =
        json!([on_digest(5, Some(TODAY), json!({}))]);
    let report = later(&lost, State::default());
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    assert!(
        maintain_lines(&report)[1]
            .contains("2 of 2 incoming pull request(s) changed since no plan")
    );
    // A day not done yet keeps its repositories: no search.
    let unsearched = without(&[search]);
    let state = State {
        maintain: Some(maintain::Day {
            done: false,
            ..expected
        }),
        ..State::default()
    };
    let report = later(&unsearched, state);
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    let lines = maintain_lines(&report);
    assert!(
        lines[0].starts_with("maintain: digest due: 2 of 2"),
        "{lines:#?}"
    );
}

/// `forge`'s next comment on the digest issue, by `author`, as a run
/// posting `body` would add it.
fn post_on_digest(forge: &Forge, author: &str, body: String) {
    let id = forge.ids.get() + 1;
    forge.ids.set(id);
    let key = ("cgwalters-forge/bot-ops".to_owned(), 9);
    forge
        .added
        .borrow_mut()
        .entry(key)
        .or_default()
        .push(json!({
            "id": id, "url": format!("{DIGEST}#issuecomment-{id}"), "body": body,
            "createdAt": "2026-10-23T18:00:00Z", "lastEditedAt": null,
            "author": {"login": author}, "editor": null,
        }));
}

/// A digest for the recording's day approving `approve` at the given heads
/// and waiting on `waiting`, with `heads` claiming both, which is not read.
fn digest_of(approve: &[(&str, String)], waiting: &[&str]) -> String {
    let approve: Vec<Value> = (approve.iter())
        .map(|(pr, head)| json!({"pr": pr, "head": head, "reason": "Small; CI green."}))
        .collect();
    let waiting: Vec<Value> = (waiting.iter())
        .map(|pr| json!({"pr": pr, "reply": "Please rebase."}))
        .collect();
    let block = json!({"schema": maintain::DIGEST_SCHEMA, "date": "2026-10-23",
        "heads": {"up/a#1": "aa0000000000", "up/a#2": "bb0000000000"},
        "approve": approve, "closer": [], "waiting": waiting, "stale": []});
    format!("Digest.\n\n```{}\n{block}\n```\n", maintain::DIGEST_FENCE)
}

/// An applying pass records the dispatched pull requests' full heads as
/// pending, none acknowledged, and a second pass the same day starts
/// nothing. The next day's pass acknowledges only those the bot's digest
/// after the plan's dispatch record lists at the planned head: unlisted
/// and changed-head entries, a failed or missing digest, one by another
/// account and a lost cache leave them due. Its preview writes nothing.
#[test]
fn maintain_acknowledges_only_what_the_digest_assessed() {
    let (aa, bb) = (head("aa"), head("bb"));
    let full = digest_of(&[("up/a#1", aa.clone())], &["UP/A#2"]);
    let missing =
        "maintain: no digest for 2026-10-23 followed its dispatch; its 2 pull request(s) stay due";
    let due = |n: usize, reviews: &str| {
        format!(
            "maintain: digest due: {n} of 2 incoming pull request(s) changed since {DIGEST}#issuecomment-*; \
             reviews {reviews}"
        )
    };
    // The comment a run adds after the dispatch, by whom, whether the poll
    // cache is kept, and the next day's lines after its repositories, in
    // order, each with all of its parts between `*`.
    let cases = [
        (
            "full",
            Some(("cgwalters-bot", full.clone())),
            true,
            vec![
                format!("maintain: the digest {DIGEST}#issuecomment-*assessed 2 of 2 planned pull request(s)"),
                "maintain: none of 2 incoming pull request(s) changed since".to_owned(),
            ],
        ),
        (
            "partial",
            Some(("cgwalters-bot", digest_of(&[("up/a#1", aa.clone())], &[]))),
            true,
            vec![
                "assessed 1 of 2 planned pull request(s); due again: up/a#2".to_owned(),
                due(1, "up/a#2"),
            ],
        ),
        ("no digest", None, true, vec![missing.to_owned(), due(2, "up/a#1, up/a#2")]),
        (
            "failed run",
            Some(("cgwalters-bot", "The run failed.".to_owned())),
            true,
            vec![missing.to_owned(), due(2, "up/a#1, up/a#2")],
        ),
        (
            "changed head",
            // Listed again without a head, it still moved.
            Some((
                "cgwalters-bot",
                digest_of(&[("up/a#1", head("a1")), ("up/a#2", bb)], &["up/a#1"]),
            )),
            true,
            vec![
                "assessed 1 of 2 planned pull request(s); due again: up/a#1".to_owned(),
                due(1, "up/a#1"),
            ],
        ),
        (
            "forged by another author",
            Some(("collaborator", full.clone())),
            true,
            vec![missing.to_owned(), due(2, "up/a#1, up/a#2")],
        ),
        (
            "lost cache",
            Some(("cgwalters-bot", full)),
            false,
            vec![
                "maintain: digest due: 2 of 2 incoming pull request(s) changed since no plan; reviews up/a#1, up/a#2"
                    .to_owned(),
            ],
        ),
    ];
    for (name, comment, kept, expected) in cases {
        let (mut config, mut replay) = maintained(9, vec![], 40.0);
        config.work_repositories.clear();
        digest_visibility(&mut replay, visible(true));
        replay
            .requests
            .push(json!({"request": {"get": format!("{API_ROOT}user")},
            "answer": {"status": 200, "link": null, "body": {"login": "cgwalters-bot"}}}));
        // The next day's searches look back from it.
        let searches: Vec<Value> = (replay.requests.iter())
            .filter(|e| text(e, "/request/get").contains("updated%3A%3E%3D2026-07-25"))
            .map(|e| {
                let mut e = e.clone();
                let url = text(&e, "/request/get").replace("2026-07-25", "2026-07-26");
                e["request"]["get"] = url.into();
                e
            })
            .collect();
        replay.requests.extend(searches);
        let forge = Forge {
            replay,
            ..Forge::new(None, false)
        };
        let report = apply(&forge, &config);
        assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
        assert_eq!(forge.dispatches(), ["cgwalters-forge/bot-ops#9"], "{name}");
        let comments = forge.comments_on("cgwalters-forge/bot-ops", 9);
        let record = (comments.iter())
            .find(|c| c.starts_with(maintain::PLAN_MARKER))
            .and_then(|c| plan::fenced_json(c, maintain::PLAN_BLOCK).unwrap())
            .unwrap();
        assert_eq!(record["schema"], maintain::PLAN_SCHEMA);
        assert_eq!(record["heads"], json!({}), "{name}");
        assert_eq!(
            record["pending"]["heads"],
            json!({"up/a#1": aa, "up/a#2": head("bb")}),
            "{name}"
        );
        assert!(record["pending"]["plan"].is_string(), "{name}");
        let state = report.state.unwrap();
        if let Some((author, body)) = &comment {
            post_on_digest(&forge, author, body.clone());
        }
        // The same day, the pending dispatch holds the day: no second start.
        let again = block(run(
            &forge,
            &forge,
            &config,
            state.clone(),
            (Mode::Actions(&own()), Limits::default()),
            forge.replay.now,
        ))
        .unwrap();
        assert!(
            maintain_lines(&again)
                .iter()
                .any(|l| l.starts_with("maintain: done for today")),
            "{name}: {:#?}",
            again.lines
        );
        assert_eq!(forge.dispatches().len(), 1, "{name}");
        let writes = forge.writes.borrow().len();
        let state = if kept { state } else { State::default() };
        let next = block(run(
            &forge,
            &forge,
            &config,
            state,
            (Mode::Preview, Limits::default()),
            forge.replay.now + time::Duration::days(1),
        ))
        .unwrap();
        assert_eq!(next.failures, 0, "{name}: {:#?}", next.lines);
        let lines = maintain_lines(&next);
        assert_eq!(lines[0], "maintain: repositories: up/a", "{name}");
        let matches = |line: &str, expected: &str| expected.split('*').all(|p| line.contains(p));
        // What follows, the day's start, is the other tests'.
        assert!(lines.len() > expected.len(), "{name}: {lines:#?}");
        for (line, expected) in lines[1..].iter().zip(&expected) {
            assert!(matches(line, expected), "{name}: {expected}: {lines:#?}");
        }
        assert_eq!(forge.writes.borrow().len(), writes, "{name}: preview wrote");
        assert_eq!(forge.dispatches().len(), 1, "{name}");
    }
}

/// Neither an older forged marker at bootstrap nor a surviving forged
/// marker after deletion/marker removal can supply a day or heads. A receipt
/// from another digest issue cannot authorize the matching comment ID.
#[test]
fn maintain_requires_its_creation_receipt() {
    let heads = json!({"up/a#1": "aa0000000000", "up/a#2": "bb0000000000"});
    let forged = on_digest(6, Some(TODAY), heads.clone());
    let cases = [
        (
            "bootstrap",
            State::default(),
            vec![on_digest(5, Some(TODAY), heads)],
        ),
        ("deleted", maintain_receipt(), vec![forged.clone()]),
        (
            "marker removed",
            maintain_receipt(),
            vec![on_digest(5, None, json!({})), forged.clone()],
        ),
        (
            "different issue",
            {
                let mut state = maintain_receipt();
                state.maintain.as_mut().unwrap().comment =
                    Some(format!("{DIGEST}0#issuecomment-6"));
                state
            },
            vec![forged],
        ),
    ];
    for (name, state, comments) in cases {
        let (config, mut replay) = maintained(9, comments, 40.0);
        for (url, body) in [
            ("repos/cgwalters-forge/bot-ops", visible(false)),
            ("user", json!({"login": "cgwalters-bot"})),
        ] {
            replay
                .requests
                .push(json!({"request": {"get": format!("{API_ROOT}{url}")},
                "answer": {"status": 200, "link": null, "body": body}}));
        }
        let forge = Forge {
            replay,
            ..Forge::new(None, false)
        };
        let report = block(run(
            &forge,
            &forge,
            &config,
            state,
            (Mode::Local, Limits::default()),
            forge.replay.now,
        ))
        .unwrap();
        assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
        assert!(
            maintain_lines(&report)[1]
                .contains("2 of 2 incoming pull request(s) changed since no plan"),
            "{name}: {:#?}",
            report.lines
        );
        let writes = forge.writes.borrow();
        assert!(
            writes
                .iter()
                .any(|(m, url, _)| m == "POST" && url.ends_with("bot-ops/issues/9/comments")),
            "{name}: {writes:#?}"
        );
        assert!(
            !writes
                .iter()
                .any(|(m, url, _)| m == "PATCH" && url.contains("bot-ops/issues/comments/")),
            "{name}: {writes:#?}"
        );
        let state = State::from_json(&report.state.unwrap().to_json()).unwrap();
        assert!(state.maintain.unwrap().done, "{name}");
    }
}

/// Even a pending same-day cache must be re-derived with new selection or
/// digest settings, and a done day cannot bypass this invalidation either.
#[test]
fn maintain_invalidates_changed_configuration() {
    let (config, replay) = maintained(9, vec![], 40.0);
    let cached = preview(&config, &replay).state.unwrap();
    let cases = [
        (json!({"exclude": ["up/a", "up/c"]}), "", false),
        (json!({"include": ["inc/z"]}), "inc/z, up/a", false),
        (json!({"max": 1}), "up/a", false),
        (json!({"min": 2}), "up/a, up/b", false),
        (json!({"sinceDays": 89}), "up/a", false),
        (json!({}), "", true),
    ];
    for (edit, expected, digest_changed) in cases {
        let mut repos = json!({"sinceDays": 90, "min": 3, "max": 2, "exclude": ["up/c"]});
        repos
            .as_object_mut()
            .unwrap()
            .extend(edit.as_object().unwrap().clone());
        let (mut config, mut replay) = maintained_with(9, vec![], 40.0, repos);
        if digest_changed {
            config.maintain.as_mut().unwrap().digest.repository = "up/digest".into();
        }
        if edit.get("sinceDays").is_some() {
            for request in &mut replay.requests {
                if let Some(url) = request["request"]["get"].as_str() {
                    request["request"]["get"] = url.replace("2026-07-25", "2026-07-26").into();
                }
            }
        }
        // An excluded repository must never even be listed.
        if expected.is_empty() {
            replay
                .requests
                .retain(|e| !e["request"].to_string().contains("/pulls?"));
        }
        for done in [false, true] {
            let mut state = cached.clone();
            state.maintain.as_mut().unwrap().done = done;
            let report = block(run(
                &replay,
                &replay,
                &config,
                state,
                (Mode::Preview, Limits::default()),
                replay.now,
            ))
            .unwrap();
            assert_eq!(
                report.failures, 0,
                "{edit}, digest={digest_changed}, done={done}: {:#?}",
                report.lines
            );
            assert_eq!(
                maintain_lines(&report)[0],
                format!("maintain: repositories: {expected}")
            );
            assert_eq!(report.maintain.unwrap().repos.join(", "), expected);
        }
    }
}

/// A pre-due pass and a search failure must preserve the receipt for a later
/// day, even though neither produces a new daily decision.
#[test]
fn maintain_preserves_receipt_without_a_new_day() {
    for hour in [9, 16] {
        let (config, mut replay) = maintained(hour, vec![], 40.0);
        if hour == 9 {
            answer(&mut replay, &operator_search("author"))["status"] = 503.into();
        }
        let state = maintain_receipt();
        let expected = state.maintain.clone();
        let report = block(run(
            &replay,
            &replay,
            &config,
            state,
            (Mode::Preview, Limits::default()),
            replay.now,
        ))
        .unwrap();
        assert_eq!(
            report.failures,
            usize::from(hour == 9),
            "{:#?}",
            report.lines
        );
        let state = State::from_json(&report.state.unwrap().to_json()).unwrap();
        assert_eq!(state.maintain, expected);
    }
}

/// While a digest is due, the other stages leave its share of each pool's
/// allowance: claude, allowed 54.3%, holds them past 43.4% used, and stage 4
/// starts nothing. Maintain may use the share and no more: past the
/// allowance, which claude's burst would let the others go beyond, it waits
/// too. A plan recorded today leaves the whole allowance to the others.
#[test]
fn maintain_share_is_left_while_a_digest_is_due() {
    let openai = "openai is held until 2026-10-24T11:15:10Z";
    let started = "start triage cgwalters-forge/tracker#446 pool=claude";
    let others = |used: &str| {
        format!(
            "hold pools: claude has used {used}% of the 43.4% that the due maintain digest leaves the other stages; {openai}; new runs wait"
        )
    };
    let posted = vec![on_digest(5, Some(TODAY), json!({}))];
    let cases = [
        (
            "well under",
            vec![],
            40.0,
            Some("claude"),
            started.to_owned(),
        ),
        (
            "within the share",
            vec![],
            45.0,
            Some("claude"),
            others("45"),
        ),
        ("past the allowance", vec![], 56.0, None, others("56")),
        ("not due", posted, 45.0, None, started.to_owned()),
    ];
    for (name, comments, used, pool, expected) in cases {
        let due = comments.is_empty();
        let (config, replay) = maintained(9, comments, used);
        let report = block(run(
            &replay,
            &replay,
            &config,
            maintain_receipt(),
            (Mode::Preview, Limits::default()),
            replay.now,
        ))
        .unwrap();
        assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
        let lines = maintain_lines(&report);
        match (due, pool) {
            (true, Some(pool)) => assert!(
                lines[2].contains(&format!(r#""pool":"{pool}""#)),
                "{name}: {lines:#?}"
            ),
            (true, None) => assert_eq!(
                lines[2],
                format!(
                    "maintain: no pool: claude has used {used}% of the 54.3% of its allowance, past which maintain does not start; {openai}"
                ),
                "{name}"
            ),
            (false, _) => assert_eq!(lines.len(), 2, "{name}: {lines:#?}"),
        }
        assert!(
            report.lines.contains(&expected),
            "{name}: {expected}: {:#?}",
            report.lines
        );
    }
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
        let preview = block(run(
            &replay,
            &replay,
            &config,
            State::default(),
            preview,
            replay.now,
        ));
        let forge = Forge::new(None, false);
        let local = (Mode::Local, limits);
        let applied = block(run(
            &forge,
            &forge,
            &config,
            State::default(),
            local,
            forge.replay.now,
        ));
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
            // Then what it spent, every GraphQL point an estimate here.
            let line = report.summary_line();
            let spent = format!(
                "{summary}, requests {}, points ~{}",
                report.spent.requests, report.spent.points
            );
            assert_eq!(line, spent);
            assert!(report.spent.requests > 0 && report.spent.points > 0);
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
        block(run_within(
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
        ))
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

/// A budget that runs out once candidates were held and started defers the
/// rest to the next pass, as a full wip does, rather than failing it
/// (tracker#908); one too small for the first candidate is still a failure.
#[test]
fn apply_defers_the_rest_when_its_budget_runs_out() {
    let config = config(json!({"wip": 8}));
    let cases = [
        (
            168,
            "budget: 61 requests left, fewer than a start may take (64); 1 more candidate(s) wait",
            0,
        ),
        (
            84,
            "budget: 62 requests left, fewer than a start may take (64); 9 more candidate(s) wait",
            1,
        ),
    ];
    for (limit, line, failures) in cases {
        let forge = Forge::new(None, false);
        let admit_by = Instant::now() + Duration::from_secs(PASS_SECONDS_LIMIT);
        let report = block(run_within(
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
        ))
        .unwrap();
        assert_eq!(last_scheduled(&report), line, "limit {limit}");
        assert_eq!(
            report.failures, failures,
            "limit {limit}: {:#?}",
            report.lines
        );
        if failures == 0 {
            let holds = (report.lines.iter())
                .filter(|l| l.starts_with("hold cgwalters-forge/"))
                .count();
            assert_eq!(holds, 3, "limit {limit}: {:#?}", report.lines);
            assert_eq!(forge.dispatches().len(), 5, "limit {limit}");
        }
    }
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

/// The text of tracker#446 for the overlap check, which names no path:
/// nothing to overlap.
fn unpathed(replay: &mut Replay) {
    let mut text = replay
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
    replay.requests.push(text);
}

/// A plan that names no pool is sent with the one pace chose; the dispatch
/// record and the run are the plan protocol's.
#[test]
fn implement_takes_the_paced_pool() {
    for (ticket, expected) in [(0, "claude"), (1, "openai")] {
        let forge = {
            let mut forge = Forge::new(None, false);
            implement_plan(&mut forge.replay);
            unpathed(&mut forge.replay);
            let mut snapshot = usage(&mut forge.replay);
            snapshot["pools"]["openai"]["hold"] = json!(false);
            snapshot["pools"]["openai"]["used_percent"] = json!(1);
            set_usage(&mut forge.replay, &snapshot);
            forge
        };
        let report =
            crate::pool_choice::with_index(ticket, || apply(&forge, &config(json!({"wip": 3}))));
        assert_eq!(report.failures, 0, "{:#?}", report.lines);
        let line = format!(
            "start implement cgwalters-forge/tracker#446 plan=9@2026-10-23T00:00:00Z pool={expected} record="
        );
        assert!(
            report.lines.iter().any(|l| l.starts_with(&line)),
            "{:#?}",
            report.lines
        );
        let writes = forge.writes.borrow();
        let (_, _, sent) = writes
            .iter()
            .find(|(_, url, _)| url.ends_with("/dispatches"))
            .unwrap();
        assert_eq!(sent["inputs"]["kind"], "implement");
        assert_eq!(sent["inputs"]["pool"], expected);
        assert_eq!(sent["ref"], "main");
    }
}

#[test]
fn generated_triage_plan_keeps_the_drawn_pool_and_explicit_held_plan_waits() {
    for explicit in [false, true] {
        let mut forge = Forge::new(None, false);
        only(&mut forge.replay, &["cgwalters-forge/tracker#446"]);
        unpathed(&mut forge.replay);
        if explicit {
            implement_plan(&mut forge.replay);
            let comments = &mut answer(&mut forge.replay, r#""name":"tracker","number":446"#)["data"]
                ["repository"]["issue"]["comments"]["nodes"];
            let body = comments.as_array_mut().unwrap().last_mut().unwrap();
            let pinned = body["body"].as_str().unwrap().replace(
                "\"kind\":\"implement\"",
                "\"kind\":\"implement\",\"pool\":\"openai\"",
            );
            body["body"] = json!(pinned);
            // Fixture's openai is held. The available claude must not replace it.
        } else {
            let mut snapshot = usage(&mut forge.replay);
            snapshot["pools"]["openai"]["hold"] = json!(false);
            set_usage(&mut forge.replay, &snapshot);
        }
        let report =
            crate::pool_choice::with_index(1, || apply(&forge, &config(json!({"wip": 3}))));
        assert_eq!(report.failures, 0, "{:#?}", report.lines);
        let sent = forge.dispatches();
        if explicit {
            assert!(sent.is_empty(), "{:#?}", report.lines);
            assert!(
                report
                    .lines
                    .iter()
                    .any(|l| l.contains("no pool: openai is held"))
            );
        } else {
            assert_eq!(sent.len(), 1, "{:#?}", report.lines);
            let writes = forge.writes.borrow();
            let (_, _, sent) = writes
                .iter()
                .find(|(_, url, _)| url.ends_with("/dispatches"))
                .unwrap();
            assert_eq!(sent["inputs"]["pool"], "openai");
            assert_eq!(sent["inputs"]["kind"], "triage");
            assert!(
                forge
                    .comments_on("cgwalters-forge/tracker", 446)
                    .iter()
                    .any(|body| body.replace(' ', "").contains("\"pool\":\"openai\""))
            );
        }
    }
}

/// With `bodyPlans` (agent-board#342), a P0 or P1 issue with no plan comment
/// whose body holds a writer's plan is started by it, once: a copy posted as
/// its plan comment, then the plan protocol's record and run. A body with no
/// plan block or by someone else, a lower Priority or the setting off leave
/// it to triage; a body someone else edited last, or an invalid plan, hold it.
#[test]
fn body_plans_start_a_p0_or_p1_issue() {
    let bot = "cgwalters-bot";
    let block = body_plan_block();
    let triage = "start triage cgwalters-forge/tracker#446 pool=claude record=";
    let implement = "start implement cgwalters-forge/tracker#446 plan=body pool=claude record=";
    let invalid = "hold cgwalters-forge/tracker#446: its body's plan is invalid: ";
    // Name, bodyPlans, the issue's Priority, its body, the body's author and
    // editor (empty for one GitHub cannot name), and the issue's line.
    type Case<'a> = (
        &'a str,
        bool,
        &'a str,
        String,
        &'a str,
        Option<&'a str>,
        String,
    );
    #[rustfmt::skip]
    let cases: Vec<Case> = vec![
        ("the bot's plan on P0", true, "P0", block.clone(), bot, None, implement.into()),
        ("the operator's on P1, edited by the bot", true, "P1", block.clone(), "cgwalters", Some(bot), implement.into()),
        ("the setting off", false, "P0", block.clone(), bot, None, triage.into()),
        ("P2", true, "P2", block.clone(), bot, None, triage.into()),
        ("no plan block", true, "P0", "Write the staging design down.".into(), bot, None, triage.into()),
        ("a visitor's plan", true, "P0", block.clone(), "visitor", None, triage.into()),
        ("a plan a visitor edited", true, "P0", block.clone(), bot, Some("visitor"),
            format!("{invalid}newest plan https://github.com/cgwalters-forge/tracker/issues/446 was last edited by visitor")),
        ("a plan an unnamed account edited", true, "P0", block.clone(), bot, Some(""),
            format!("{invalid}newest plan https://github.com/cgwalters-forge/tracker/issues/446 was last edited by an unidentified account")),
        ("an invalid plan", true, "P0", format!("```{PLAN_FENCE}\n{{}}\n```\n"), bot, None,
            format!("{invalid}newest plan https://github.com/cgwalters-forge/tracker/issues/446: plan has no schema")),
        // Then held as a plan comment would be.
        ("a research plan", true, "P0", block.replace("implement", "research"), bot, None,
            "hold cgwalters-forge/tracker#446: plan body is for research, which is dispatched by hand".into()),
        ("a plan outside the work repositories", true, "P0", block.replace("cgwalters-forge/tracker", "o/r"), bot, None,
            "hold cgwalters-forge/tracker#446: its plan's o/r#446 is not in a work repository".into()),
    ];
    for (name, on, priority, body, author, editor, expected) in cases {
        let forge = body_plan_forge(priority, &body, author, editor);
        let config = config(json!({"bodyPlans": on}));
        let first = apply(&forge, &config);
        assert_eq!(first.failures, 0, "{name}: {:#?}", first.lines);
        let line = first.lines.iter().find(|l| l.contains("tracker#446"));
        assert!(
            line.is_some_and(|l| l.starts_with(&expected)),
            "{name}: no line {expected:?} in {:#?}",
            first.lines
        );
        if expected.starts_with("hold") {
            assert!(forge.writes.borrow().is_empty(), "{name}");
            continue;
        }
        let comments = forge.comments_on("cgwalters-forge/tracker", 446);
        assert_eq!(comments.len(), 2, "{name}: {comments:#?}");
        let kind = if expected == implement {
            "implement"
        } else {
            "triage"
        };
        assert!(
            comments[0].contains(&format!("\"kind\": \"{kind}\"")),
            "{name}: {}",
            comments[0]
        );
        assert_eq!(
            comments[0].starts_with(BODY_PLAN),
            kind == "implement",
            "{name}"
        );
        assert!(
            comments[1].starts_with("<!-- agent-board-dispatch: "),
            "{name}"
        );
        let writes = forge.writes.borrow();
        let (_, _, sent) = (writes.iter())
            .find(|(_, url, _)| url.ends_with("/dispatches"))
            .unwrap();
        assert_eq!(sent["inputs"]["kind"], kind, "{name}");
        assert_eq!(sent["inputs"]["pool"], "claude", "{name}");
        drop(writes);
        // The copy is the issue's plan now, recorded once.
        let second = apply(&forge, &config);
        assert_eq!(second.failures, 0, "{name}: {:#?}", second.lines);
        assert_eq!(forge.dispatches().len(), 1, "{name}: {:#?}", second.lines);
    }
}

/// An issue body holding the bot's implement plan for tracker#446.
fn body_plan_block() -> String {
    let plan = json!({"schema": JOB_PLAN_SCHEMA, "repo": "cgwalters-forge/tracker", "item": 446,
        "kind": "implement", "runner": "hosted", "brief": "Write the staging design down."});
    format!("Some context.\n\n```{PLAN_FENCE}\n{plan}\n```\n")
}

/// The recording with only tracker#446, at `priority`, whose `body` is by
/// `author` and last edited by `editor` (empty for one GitHub cannot name).
fn body_plan_forge(priority: &str, body: &str, author: &str, editor: Option<&str>) -> Forge {
    let mut forge = Forge::new(None, false);
    only(&mut forge.replay, &["cgwalters-forge/tracker#446"]);
    unpathed(&mut forge.replay);
    on_446(&mut forge, |node| {
        node["priority"] = json!({"name": priority})
    });
    let variables = json!({"owner": "cgwalters-forge", "name": "tracker", "number": 446});
    let issue = json!({"url": "https://github.com/cgwalters-forge/tracker/issues/446",
        "title": "Work", "body": body, "createdAt": "2026-10-20T00:00:00Z",
        "lastEditedAt": editor.map(|_| "2026-10-21T00:00:00Z"),
        "author": {"login": author},
        "editor": editor.filter(|e| !e.is_empty()).map(|e| json!({"login": e}))});
    forge
        .replay
        .requests
        .push(json!({"request": graphql_key(BODY_QUERY, &variables),
        "answer": {"data": {"repository": {"issue": issue}}}}));
    forge
}

/// Applies `edit` to tracker#446's board item in every recorded board page.
fn on_446(forge: &mut Forge, edit: impl Fn(&mut Value)) {
    for entry in &mut forge.replay.requests {
        let nodes = entry["answer"].pointer_mut("/data/repositoryOwner/projectV2/items/nodes");
        for node in nodes.and_then(Value::as_array_mut).into_iter().flatten() {
            if node["content"]["number"] == 446 {
                edit(node);
            }
        }
    }
}

/// With `bodyPlans`, a held P0 issue whose body holds the bot's plan stays
/// held: the holds come before its body is read, so nothing is written.
#[test]
fn body_plans_leave_a_held_issue_held() {
    fn label(node: &mut Value, name: &str) {
        let labels = &mut node["content"]["labels"];
        labels["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name": name}));
        labels["totalCount"] = (labels["totalCount"].as_u64().unwrap() + 1).into();
    }
    type Edit = fn(&mut Value);
    let cases: Vec<(&str, Edit)> = vec![
        ("labelled needs-human", |n| label(n, "needs-human")),
        ("labelled needs-design", |n| label(n, "needs-design")),
        ("board Status is Blocked", |n| {
            n["status"] = json!({"name": "Blocked"})
        }),
    ];
    for (why, edit) in cases {
        let mut forge = body_plan_forge("P0", &body_plan_block(), "cgwalters-bot", None);
        on_446(&mut forge, edit);
        // Reading the body fails the issue, so a hold checked only later
        // (as the start's recheck also does) fails the test.
        *forge.fail.borrow_mut() = Some(BODY_QUERY.into());
        let report = apply(&forge, &config(json!({"bodyPlans": true})));
        let expected = format!("hold cgwalters-forge/tracker#446: {why}");
        assert!(
            report.lines.contains(&expected),
            "{why}: no line {expected:?} in {:#?}",
            report.lines
        );
        assert!(forge.writes.borrow().is_empty(), "{why}");
    }
}

/// A body plan is copied only as it was read, with still no plan comment,
/// and dispatched only while the copy is the newest plan: a plan written in
/// between, which the pass did not check, starts nothing.
#[test]
fn body_plans_refuse_a_plan_changed_while_read() {
    let block = body_plan_block();
    // Another writer's plan, for a repository outside the work repositories.
    let other = json!({"url": "https://github.com/cgwalters-forge/tracker/issues/446#issuecomment-1",
        "body": block.replace("cgwalters-forge/tracker", "o/r"), "createdAt": "2026-12-01T00:00:00Z",
        "lastEditedAt": null, "author": {"login": "cgwalters"}, "editor": null});
    let comments = "comments(first: 100";
    // Name, the query raced, from which of its reads, the edit, and the
    // line the issue gets and whether the copy was posted.
    type Edit = fn(&mut Value, &Value);
    let edited: Edit = |data, _| {
        let body = &mut data["repository"]["issue"]["body"];
        *body = json!(body.as_str().unwrap().replace("Write", "Rewrite"));
    };
    let planned: Edit = |data, other| {
        let nodes = data.pointer_mut("/repository/issue/comments/nodes");
        nodes
            .and_then(Value::as_array_mut)
            .unwrap()
            .push(other.clone());
    };
    let copied: Edit = |data, other| {
        let nodes = data.pointer_mut("/repository/issue/comments/nodes");
        let nodes = nodes.and_then(Value::as_array_mut).unwrap();
        if nodes
            .iter()
            .any(|n| text(n, "/body").starts_with(BODY_PLAN))
        {
            nodes.push(other.clone());
        }
    };
    #[rustfmt::skip]
    let cases: [(&str, &str, usize, Edit, &str, bool); 3] = [
        ("the body edited", BODY_QUERY, 2, edited,
            "hold cgwalters-forge/tracker#446: its body changed since the pass read its plan", false),
        ("a plan comment posted", comments, 2, planned,
            "hold cgwalters-forge/tracker#446: a plan comment appeared since the pass read its body", false),
        ("a plan posted after the copy", comments, 1, copied,
            "start implement cgwalters-forge/tracker#446 plan=body pool=claude failed: the newest plan is 1@2026-12-01T00:00:00Z, not ", true),
    ];
    for (name, query, from, edit, expected, posted) in cases {
        let mut forge = body_plan_forge("P0", &block, "cgwalters-bot", None);
        let (query, other, reads) = (query.to_owned(), other.clone(), Cell::new(0));
        forge.racing = Some(Box::new(move |request, data| {
            if request.contains(&query) {
                reads.set(reads.get() + 1);
                if reads.get() >= from {
                    edit(data, &other);
                }
            }
        }));
        let report = apply(&forge, &config(json!({"bodyPlans": true})));
        let line = report.lines.iter().find(|l| l.contains("tracker#446"));
        assert!(
            line.is_some_and(|l| l.starts_with(expected)),
            "{name}: no line {expected:?} in {:#?}",
            report.lines
        );
        assert!(forge.dispatches().is_empty(), "{name}");
        let comments = forge.comments_on("cgwalters-forge/tracker", 446);
        assert_eq!(comments.len(), usize::from(posted), "{name}: {comments:#?}");
    }
}

// ---- Pure logic ----

/// The keys a `revisit` section needs beside it: triage, on pool `a`.
fn revisit(section: Value) -> Value {
    json!({"triage": true, "planPools": ["a"], "pools": {"a": {}},
        "usage": {"repository": "o/ops", "issue": 1}, "revisit": section})
}

/// A `maintain` section, its keys replaced by `edit`'s.
fn maintain_section(edit: Value) -> Value {
    let repos = json!({"sinceDays": 90, "min": 5, "max": 15});
    let mut section = json!({"repos": repos, "share": 0.2,
        "digest": {"repository": "o/ops", "issue": 7}, "hourUtc": 9});
    section
        .as_object_mut()
        .unwrap()
        .extend(edit.as_object().unwrap().clone());
    section
}

/// The keys a `maintain` section needs beside it: pool `a`, paced.
fn maintain(edit: Value) -> Value {
    json!({"pools": {"a": {}}, "usage": {"repository": "o/ops", "issue": 1},
        "maintain": maintain_section(edit)})
}

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
    let usage = json!({"url": "http://b.example/usage", "repository": "o/ops", "issue": 1});
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
            json!({"triageFollowUpDays": 31}),
            "triageFollowUpDays is not 0 to 30",
        ),
        (
            json!({"triageFollowUpDays": 3}),
            "triageFollowUpDays needs triage",
        ),
        (
            json!({"replanAfterDays": 31}),
            "replanAfterDays is not 0 to 30",
        ),
        (
            json!({"replanAfterDays": -1}),
            "invalid value: integer `-1`, expected u64",
        ),
        (
            json!({"replanAfterDays": 1}),
            "replanAfterDays needs triage",
        ),
        (
            json!({"revisit": {"perPass": 1, "afterDays": 14}}),
            "revisit needs triage",
        ),
        (
            revisit(json!({"perPass": 0, "afterDays": 14})),
            "revisit perPass is not 1 to 5",
        ),
        (
            revisit(json!({"perPass": 6, "afterDays": 14})),
            "revisit perPass is not 1 to 5",
        ),
        (
            revisit(json!({"perPass": 1, "afterDays": 0})),
            "revisit afterDays is not 1 to 90",
        ),
        (
            revisit(json!({"perPass": 1, "afterDays": 14, "heldAfterDays": 91})),
            "revisit heldAfterDays is not 1 to 90",
        ),
        (
            revisit(json!({"perPass": 1, "afterDays": 14, "pools": []})),
            "revisit pools is empty",
        ),
        (
            revisit(json!({"perPass": 1, "afterDays": 14, "pools": ["b"]})),
            "revisit pools names b, which is not in pools",
        ),
        (revisit(json!({"perPass": 1})), "missing field `afterDays`"),
        (
            revisit(json!({"perPass": 1, "afterDays": 14, "every": 1})),
            "unknown field `every`",
        ),
        (
            json!({"maintain": maintain_section(json!({}))}),
            "maintain needs pools",
        ),
        (maintain(json!({"repos": "up/a"})), "invalid type: string"),
        (
            maintain(json!({"repos": {"sinceDays": 90, "min": 5}})),
            "missing field `max`",
        ),
        (
            maintain(json!({"repos": {"sinceDays": 0, "min": 5, "max": 15}})),
            "maintain repos sinceDays is not 1 to 365",
        ),
        (
            maintain(json!({"repos": {"sinceDays": 90, "min": 0, "max": 15}})),
            "maintain repos min is not at least 1",
        ),
        (
            maintain(json!({"repos": {"sinceDays": 90, "min": 5, "max": 21}})),
            "maintain repos max is not 1 to 20",
        ),
        (
            maintain(json!({"repos": {"sinceDays": 90, "min": 5, "max": 1,
                "include": ["up/a", "up/b"]}})),
            "maintain repos include names more than max repositories",
        ),
        (
            maintain(json!({"repos": {"sinceDays": 90, "min": 5, "max": 15,
                "include": ["up/a"], "exclude": ["UP/A"]}})),
            "maintain repos names UP/A twice",
        ),
        (
            maintain(json!({"repos": {"sinceDays": 90, "min": 5, "max": 15, "exclude": ["up"]}})),
            "invalid repository",
        ),
        (
            maintain(json!({"repos": {"sinceDays": 90, "min": 5, "max": 15, "other": 1}})),
            "unknown field `other`",
        ),
        (
            maintain(json!({"share": 0})),
            "maintain share is not above 0 and below 1",
        ),
        (
            maintain(json!({"share": 1})),
            "maintain share is not above 0 and below 1",
        ),
        (
            maintain(json!({"hourUtc": 24})),
            "maintain hourUtc is not 0 to 23",
        ),
        (
            maintain(json!({"digest": {"repository": "up/a", "issue": 1},
                "repos": {"sinceDays": 90, "min": 5, "max": 15, "include": ["UP/A"]}})),
            "maintain digest repository is in maintain repos include",
        ),
        (
            maintain(json!({"digest": {"repository": "o/ops", "issue": 0}})),
            "maintain digest issue is not a positive number",
        ),
        (maintain(json!({"pools": ["a"]})), "unknown field `pools`"),
        (
            json!({"epics": {"repository": "o/r", "inAnyRepository": true}}),
            "unknown field `inAnyRepository`",
        ),
        (
            json!({"usage": {"repository": "o/ops", "issue": 1, "url": "ftp://broker"}}),
            "usage url is not an http(s) URL with a host",
        ),
        (
            json!({"usage": {"repository": "o/ops", "issue": 1, "url": "http://"}}),
            "usage url is not an http(s) URL with a host",
        ),
        (
            json!({"pr": {"rebaseRounds": 4}}),
            "rebaseRounds is not 0 to 3",
        ),
        (json!({"pr": {"staleDays": 31}}), "staleDays is not 0 to 30"),
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
        (
            json!({"follow": ["o/r"], "pr": {"fixPools": ["mistral"]},
                "usage": {"url": "http://broker.example/usage", "repository": "o/ops", "issue": 1}}),
            "pr pool mistral is not in pools",
        ),
        (
            json!({"budget": {"weeklyUsd": 1000}, "usage": {"repository": "o/ops", "issue": 1}}),
            "budget needs usage.url",
        ),
        (
            json!({"budget": {"weeklyUsd": 0}, "usage": {"url": "http://b.example/usage", "repository": "o/ops", "issue": 1}}),
            "budget weeklyUsd is not above 0",
        ),
        (
            json!({"budget": {"weeklyUsd": 1000000}, "usage": {"url": "http://b.example/usage", "repository": "o/ops", "issue": 1}}),
            "budget weeklyUsd is not above 0 and at most 100000",
        ),
        (
            json!({"budget": {"weeklyUsd": 1000, "monthlyUsd": 1}}),
            "unknown field `monthlyUsd`",
        ),
        (
            json!({"budget": {"source": "broker", "weeklyUsd": 1000, "url": "http://b.example/usage/grant",
                "pools": {"claude": "anthropic"}}, "pools": {"claude": {}}, "usage": usage.clone()}),
            "budget weeklyUsd is refused with source broker",
        ),
        (
            json!({"budget": {"source": "broker", "pools": {"claude": "anthropic"}},
                "pools": {"claude": {}}, "usage": usage.clone()}),
            "budget url is not an http(s) URL",
        ),
        (
            json!({"budget": {"source": "broker", "url": "http://b.example/usage/grant",
                "pools": {"claude": "anthropic", "openai": "codex"}},
                "pools": {"claude": {}}, "usage": usage.clone()}),
            "budget pools names openai, which is not in pools",
        ),
        (
            json!({"budget": {"source": "broker", "url": "http://b.example/usage/grant",
                "pools": {"claude": "anthropic"}},
                "pools": {"claude": {}, "openai": {}}, "usage": usage.clone()}),
            "pool openai has no grant pool in budget pools",
        ),
        (
            json!({"budget": {"source": "broker", "url": "http://b.example/usage/grant",
                "pools": {"claude": "anthropic"}},
                "pools": {"claude": {"target": 0.5}}, "usage": usage.clone()}),
            "pools.claude sets target or burst, which the broker's grant replaces",
        ),
        (
            json!({"budget": {"source": "broker", "url": "http://b.example/usage/grant",
                "pools": {"claude": "anthropic"}},
                "pools": {"claude": {}}, "usage": {"repository": "o/ops", "issue": 1}}),
            "budget needs usage.url",
        ),
        (
            json!({"budget": {"weeklyUsd": 1000, "pools": {"claude": "anthropic"}},
                "pools": {"claude": {}}, "usage": usage.clone()}),
            "budget url and pools need source broker",
        ),
        (
            json!({"budget": {"source": "broker", "url": "http://b.example/usage/grant",
                "pools": {"claude": "not a pool"}}, "pools": {"claude": {}}, "usage": usage.clone()}),
            "budget pools.claude is not a pool name",
        ),
        (
            json!({"budget": {"weeklyUsd": 1000, "url": "http://b.example/usage/grant"},
                "usage": usage.clone()}),
            "budget url and pools need source broker",
        ),
        (json!({"budget": {}}), "budget needs weeklyUsd or source"),
        (
            json!({"budget": {"harnessRepositories": ["o/r", "O/R"]}}),
            "budget harnessRepositories names O/R twice",
        ),
        (
            json!({"budget": {"harnessRepositories": ["o/r", null]}}),
            "config.budget.harnessRepositories[1] is null",
        ),
        (
            json!({"budget": {"harnessRepositories": ["not a repo"]}}),
            "budget harnessRepositories: invalid repository",
        ),
        (
            json!({"budget": {"harnessRepositories": ["o/r"], "url": "http://b.example/usage/grant"}}),
            "budget url and pools need source broker",
        ),
        (
            json!({"budget": {"harnessRepositories": ["o/r"], "harness": ["o/s"]}}),
            "unknown field `harness`",
        ),
        (
            json!({"budget": {"harnessRepositories": ["o/r"], "weeklyUsd": 1000}}),
            "budget needs usage.url",
        ),
        (
            json!({"budget": {"source": "ledger"}}),
            "unknown variant `ledger`",
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
    // A held item waits afterDays unless heldAfterDays is set; a revisit
    // runs on the triage pools unless its own are named.
    let mut value = minimal.clone();
    let section = revisit(json!({"perPass": 2, "afterDays": 14}));
    value
        .as_object_mut()
        .unwrap()
        .extend(section.as_object().unwrap().clone());
    let parsed = Config::parse(&value.to_string()).unwrap();
    let section = parsed.revisit.as_ref().unwrap();
    assert_eq!(section.held_after_days, None);
    assert!(section.pools.is_none());
    let duplicate = r#"{"operator": "op", "bot": "bot", "dispatchRepo": "o/caller", "dispatchWorkflow": "dispatch.yml",
        "pools": {"a": {}, "a": {}}, "usage": {"repository": "o/ops", "issue": 1}}"#;
    assert!(
        format!("{:#}", Config::parse(duplicate).unwrap_err()).contains("pool a is named twice")
    );
    // The harness's repositories alone set no budget and need no broker.
    let mut harness = minimal.clone();
    harness["budget"] = json!({"harnessRepositories": ["o/r"]});
    let harness = Config::parse(&harness.to_string()).unwrap();
    assert_eq!(
        harness.harness_repositories(),
        Some(&["o/r".to_owned()][..])
    );
    assert!(harness.budget.unwrap().weekly_usd.is_none());
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
    let poll =
        config(json!({"pr": {"mergeQueue": {}, "rebaseRounds": 2, "staleDays": 7}})).poll(None);
    assert_eq!(poll.policy.merge_queue, Some(poll::QUEUE_TRIES));
    assert_eq!(poll.policy.rebase_rounds, 2);
    assert_eq!(poll.policy.stale_days, 7);
    assert_eq!(config(json!({})).poll(None).policy.stale_days, 0);
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
    // Every eligible pool gets a ticket regardless of headroom or order.
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
        choose_with(Some(&three), &pools, &names(&["a", "b"]), now, |_| Ok(0)),
        Ok("a".into())
    );
    assert_eq!(
        choose_with(Some(&three), &pools, &names(&["b", "a"]), now, |_| Ok(0)),
        Ok("b".into())
    );
    assert_eq!(
        choose_with(Some(&three), &pools, &names(&["a", "c"]), now, |_| Ok(1)),
        Ok("c".into())
    );
    assert_eq!(
        choose_with(Some(&three), &pools, &names(&["a", "c", "a"]), now, |len| {
            assert_eq!(len, 2);
            Ok(0)
        }),
        Ok("a".into())
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
        updated: None,
        author: Some("op".into()),
        labels: vec![],
        labels_complete: true,
        blocked_by: 0,
        pulls: vec![],
        pulls_complete: true,
        parent: None,
        priority: None,
        status: None,
        why: None,
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
        (
            "closed",
            |f| f.open = false,
            Some((HoldKind::Closed, "closed")),
        ),
        (
            "needs-human",
            |f| f.labels = vec!["Needs-Human".into()],
            Some((HoldKind::NeedsHuman, "labelled needs-human")),
        ),
        (
            "needs-design",
            |f| f.labels = vec!["needs-design".into()],
            Some((HoldKind::NeedsDesign, "labelled needs-design")),
        ),
        (
            "unread labels",
            |f| f.labels_complete = false,
            Some((HoldKind::Unreadable, "its labels could not all be read")),
        ),
        (
            "board Blocked",
            |f| f.status = Some("Blocked".into()),
            Some((HoldKind::Blocked, "board Status is Blocked")),
        ),
        (
            "dependency",
            |f| f.blocked_by = 2,
            Some((HoldKind::WaitingOn, "2 open dependencies")),
        ),
        (
            "bot pull request",
            |f| f.pulls = vec![(9, "o/r".into(), Some("bot".into()))],
            Some((HoldKind::PrOpen, "pull request o/r#9 is open")),
        ),
        (
            "visitor pull request",
            |f| f.pulls = vec![(9, "o/r".into(), Some("visitor".into()))],
            None,
        ),
        (
            "unread pull requests",
            |f| f.pulls_complete = false,
            Some((
                HoldKind::Unreadable,
                "its pull requests could not all be read",
            )),
        ),
        (
            "a trusted pull request among those read",
            |f| {
                f.pulls = vec![(9, "o/r".into(), Some("op".into()))];
                f.pulls_complete = false;
            },
            Some((HoldKind::PrOpen, "pull request o/r#9 is open")),
        ),
    ] {
        let mut f = base.clone();
        change(&mut f);
        let held = hold(&f, &BTreeMap::new(), &people);
        let held = held.as_ref().map(|h| (h.kind, h.reason.as_str()));
        assert_eq!(held, expected, "{name}");
    }
    let why = "a run is in flight, https://run";
    let held = Some(Hold::new(HoldKind::InFlight, why));
    assert_eq!(hold(&base, &busy, &people), held);
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

/// A [`Forge`] that GitHub refuses under a rate limit, past the client's
/// retries, from its `after`th request on; it counts what reaches it.
struct Throttled {
    forge: Forge,
    after: usize,
    sent: Cell<usize>,
}

impl Throttled {
    fn send(&self) -> Result<()> {
        let sent = self.sent.get();
        self.sent.set(sent + 1);
        ensure!(
            sent < self.after,
            RateLimited("GitHub rate limit: gave up after 3 retries".into())
        );
        Ok(())
    }
}

impl Broker for Throttled {
    fn usage(&self, url: &str) -> Result<Value> {
        self.forge.usage(url)
    }

    fn grant(&self, url: &str) -> Result<Value> {
        self.forge.grant(url)
    }
}

impl Rest for Throttled {
    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
        self.send()?;
        self.forge.get(url, validator)
    }

    fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        Throttled::send(self)?;
        self.forge.send(method, url, body)
    }
}

impl Transport for Throttled {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.send()?;
        Transport::graphql(&self.forge, query, variables)
    }
}

impl crate::github::GraphQl for Throttled {
    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        Transport::graphql(self, query, variables)
    }
}

/// A pass refused under a rate limit at any point sends nothing after the
/// refusal, reports it in one `rate-limited` line rather than a failure
/// per operation, and still ends with its summary.
#[test]
fn apply_stops_at_the_first_rate_limit() {
    let config = config(json!({"wip": 3}));
    // Where the refusal comes: the first request, stage 4's first read and
    // its capacity read, a start's write, the note after its run, and stage
    // 5's board read.
    for after in [0, 10, 20, 25, 28, 30] {
        let forge = Throttled {
            forge: Forge::new(None, false),
            after,
            sent: Cell::new(0),
        };
        let admit_by = Instant::now() + Duration::from_secs(PASS_SECONDS_LIMIT);
        let bounds = Bounds::new(PASS_REQUEST_LIMIT, admit_by);
        let report = block(run_within(
            &forge,
            &forge,
            &config,
            (
                State::default(),
                Mode::Actions(&own()),
                Limits::default(),
                forge.forge.replay.now,
            ),
            &bounds,
        ))
        .unwrap();
        let lines = &report.lines;
        assert_eq!(forge.sent.get(), after + 1, "{after}: {lines:#?}");
        assert!(!lines.iter().any(|l| l.contains(NOT_SENT)), "{lines:#?}");
        let limited: Vec<_> = (lines.iter())
            .filter(|l| l.starts_with("rate-limited: "))
            .collect();
        assert_eq!(limited.len(), 1, "{after}: {lines:#?}");
        assert!(
            limited[0].contains("GitHub rate limit: gave up after 3 retries; "),
            "{}",
            limited[0]
        );
        assert_eq!(lines.last(), Some(limited[0]), "{lines:#?}");
        // The refused operation's failure, and nothing for what was not sent.
        assert_eq!(report.failures, 1, "{after}: {lines:#?}");
        let spent = report.spent;
        assert_eq!((spent.requests, spent.rate_limited), (after + 1, true));
        let line = report.summary_line();
        assert!(line.starts_with(&report.summary), "{line}");
        assert!(line.ends_with(", rate-limited"), "{line}");
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

/// Follows `cgwalters-forge/agentic-job`, whose one open pull request, #9,
/// is the bot's draft that stopped at a checkpoint: stage 2 continues it
/// where pacing lets it.
fn followed_draft(replay: &mut Replay, settings: &mut Value) {
    const REPO: &str = "cgwalters-forge/agentic-job";
    let checkpoint =
        json!({"reason": "budget", "done": "half", "left": "half", "next": "the rest"});
    let pull = json!({
        "id": "PR_9", "number": 9, "url": format!("https://github.com/{REPO}/pull/9"),
        "title": "Do the thing", "state": "OPEN", "isDraft": true, "mergeable": "MERGEABLE",
        "body": format!("Closes #8.\n\n```checkpoint\n{checkpoint}\n```\n"),
        "headRefOid": "abc", "headRefName": "dispatch/implement/agent-run-1",
        "baseRefName": "main", "isCrossRepository": true,
        "baseRepository": {"nameWithOwner": REPO, "isPrivate": false, "defaultBranchRef": {"name": "main"}},
        "author": {"__typename": "User", "login": "cgwalters-bot"},
        "headRepositoryOwner": {"login": "cgwalters-bot"},
        "labels": {"nodes": [], "pageInfo": {"hasNextPage": false}},
        "closingIssuesReferences": {"nodes": [], "pageInfo": {"hasNextPage": false}},
        "autoMergeRequest": null, "mergeQueueEntry": null,
        "files": {"nodes": [{"path": "README.md"}], "pageInfo": {"hasNextPage": false}},
        "timelineItems": {"nodes": []}, "baseChange": {"nodes": []},
        "reviews": {"nodes": [], "pageInfo": {"hasPreviousPage": false}},
        "series": {"nodes": [{"commit": {"oid": "abc", "messageHeadline": "Do half the thing"}}],
            "pageInfo": {"hasNextPage": false}},
        "commits": {"nodes": [{"commit": {"oid": "abc", "committedDate": "2026-10-23T12:00:00Z",
            "message": "Do half the thing", "statusCheckRollup": null}}]},
    });
    let variables = json!({"owner": "cgwalters-forge", "name": "agentic-job", "number": 9});
    replay.requests.push(json!({
        "request": graphql_key(poll::PULL_QUERY, &variables),
        "answer": {"data": {"repository": {"pullRequest": pull}}},
    }));
    for (path, body) in [
        ("events?per_page=100", json!([])),
        (
            "pulls?state=open&per_page=100",
            json!([{"number": 9, "user": {"login": "cgwalters-bot"}}]),
        ),
        ("issues/9/comments?per_page=100", json!([])),
        ("rules/branches/main", json!([])),
        ("branches/main", json!({"name": "main", "protected": false})),
    ] {
        replay.requests.push(json!({
            "request": {"get": format!("{API_ROOT}repos/{REPO}/{path}")},
            "answer": {"status": 200, "link": null, "body": body},
        }));
    }
    settings["follow"] = json!([REPO]);
    settings["pr"] = json!({"fixPools": ["claude"]});
}

/// 23 October, live: a pass whose published snapshot was stale refused
/// every pull request's continuation, though it read the broker fresh in the
/// same pass. Stage 2 now paces by stage 1's reading, so the continuation
/// starts, and takes a slot from scheduling.
#[test]
fn apply_paces_pull_requests_by_this_pass_reading() {
    let mut forge = Forge::new(None, false);
    let mut settings = json!({"wip": 2});
    forge.replay.now += time::Duration::minutes(45);
    let broker = json!({"body": usage::tests::broker()});
    with_broker(&mut forge.replay, &mut settings, broker);
    followed_draft(&mut forge.replay, &mut settings);
    for (url, body) in [
        (format!("{API_ROOT}user"), json!({"login": "cgwalters-bot"})),
        (
            format!("{API_ROOT}repos/cgwalters-forge/bot-ops"),
            json!({"private": true}),
        ),
    ] {
        let answer = json!({"status": 200, "link": null, "body": body});
        let request = json!({"request": {"get": url}, "answer": answer});
        forge.replay.requests.push(request);
    }
    let report = apply(&forge, &config(settings));
    let lines = &report.lines;
    for line in [
        "usage: published ",
        "cgwalters-forge/agentic-job#9 abc continue: continuing (round 1): its checkpoint says budget -> https://github.com/",
        "pools: claude normal 18%/145%, openai normal 3%/5%",
        "hold wip: 3 of 2 runs active",
    ] {
        assert!(
            lines.iter().any(|l| l.starts_with(line)),
            "{line}: {lines:#?}"
        );
    }
    assert_eq!(report.failures, 0, "{lines:#?}");
}

/// Past [`FOLLOW_MARGIN`] over the weekly budget, an applying pass starts no
/// pull request's continuation, which still goes on within the margin.
#[test]
fn budget_past_its_margin_holds_pull_request_rounds() {
    for (total, outcome) in [
        (1099.0, " -> https://github.com/"),
        (1100.0, " -> not started: every pool is held (claude)"),
    ] {
        let mut forge = Forge::new(None, false);
        let mut settings = json!({"wip": 2});
        with_spend(&mut forge.replay, &mut settings, spend(total));
        followed_draft(&mut forge.replay, &mut settings);
        for (url, body) in [
            (format!("{API_ROOT}user"), json!({"login": "cgwalters-bot"})),
            (
                format!("{API_ROOT}repos/cgwalters-forge/bot-ops"),
                json!({"private": true}),
            ),
        ] {
            let answer = json!({"status": 200, "link": null, "body": body});
            let request = json!({"request": {"get": url}, "answer": answer});
            forge.replay.requests.push(request);
        }
        let report = apply(&forge, &config(settings));
        let lines = &report.lines;
        let continuation = "cgwalters-forge/agentic-job#9 abc continue: continuing (round 1)";
        let line = lines.iter().find(|l| l.starts_with(continuation));
        assert!(
            line.is_some_and(|l| l.contains(outcome)),
            "{total}: {lines:#?}"
        );
    }
}

/// A run list longer than the request budget is read until the budget is
/// spent, then fails closed, saying how many runs it read: capacity is
/// unknown, so nothing starts.
#[test]
fn runs_past_the_budget_are_no_capacity() {
    let mut replay = without_epics(replay());
    let page = format!(
        "{API_ROOT}repos/cgwalters-forge/tracker/actions/workflows/dispatch.yml/runs?status=in_progress&per_page=100&page=2"
    );
    // The second page links to itself, so the list goes on past the budget.
    second_page(&mut replay, json!({"id": 7}), Some(&page));
    for entry in &mut replay.requests {
        if entry["request"]["get"]
            .as_str()
            .is_some_and(|u| u.contains("status=in_progress"))
        {
            entry["answer"]["body"]["total_count"] = RUN_LIST_LIMIT.into();
        }
    }
    let report = preview(&config(json!({})), &replay);
    let error = report
        .lines
        .iter()
        .find(|l| l.starts_with("schedule error: in_progress runs: "))
        .unwrap_or_else(|| panic!("{:#?}", report.lines));
    assert!(
        error
            == &format!(
                "schedule error: in_progress runs: 3 read of {RUN_LIST_LIMIT}; capacity is unknown: the pass's request budget ({PASS_REQUEST_LIMIT}) is spent"
            ),
        "{error}"
    );
    assert!(!report.lines.iter().any(|l| l.starts_with("start ")));
    // The Status stage finds the budget spent too.
    assert_eq!(report.failures, 2, "{:#?}", report.lines);
}
