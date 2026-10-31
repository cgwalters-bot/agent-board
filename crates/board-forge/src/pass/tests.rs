use super::*;

const CONFIG: &str = include_str!("../../../../fixtures/pass/config.json");
/// The live board read on 23 October 2026 by `pass --record`, with a
/// synthetic usage snapshot (the live one is private): claude under its
/// pace, openai held over it. Stage 3's reads of the live epics tracker#596,
/// #766 and #890 were recorded that evening, seeded with this recording,
/// and appended to it ([`epics`]'s tests replay them at that time). The
/// reads of its two dispatched implement runs' merged pull requests were
/// appended by hand, finding none ([`merged`] gives tracker#468's one).
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

/// tracker#468's implement run, whose dispatch record the recording has.
const MERGED_RUN: u64 = 39718949900;

/// tracker#468's implement run's head branch with one merged pull request
/// by `author` saying `body`, merged into `base`.
fn merged(replay: &mut Replay, author: &str, body: &str, base: &str) {
    let branch = format!("{IMPLEMENT_BRANCH_PREFIX}{MERGED_RUN}\"");
    answer(replay, &branch)["data"]["repository"]["pullRequests"]["nodes"] = json!([{
        "url": "https://github.com/cgwalters-forge/tracker/pull/901", "body": body,
        "mergedAt": "2026-10-22T12:00:00Z", "baseRefName": base, "author": {"login": author}}]);
}

/// [`merged`]: the bot's pull request closing tracker#468, into its default branch.
fn closing(replay: &mut Replay) {
    merged(replay, "cgwalters-bot", "Does it.\n\nCloses #468", "main");
}

/// tracker#468's recorded comments.
fn comments_468(replay: &mut Replay) -> &mut Vec<Value> {
    (replay.requests.iter_mut())
        .filter_map(|e| e["answer"].pointer_mut("/data/repository/issue/comments/nodes"))
        .filter_map(Value::as_array_mut)
        .find(|n| (n.iter()).any(|c| text(c, "/url").ends_with("#issuecomment-6332112108")))
        .unwrap()
}

/// tracker#468's variables and its [`close_read`], open and unlocked.
fn close_read_468() -> (Value, Value) {
    let variables = json!({"owner": "cgwalters-forge", "name": "tracker", "number": 468});
    let read = close_read(&variables, None, None);
    (variables, read)
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
        "field": "Priority", "first": epics::EPICS_PER_PAGE, "cursor": null});
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
    "wip: no slot left; 270 more candidate(s) wait",
];

/// The last line of scheduling, before stage 4b's and the Status stage's.
fn last_scheduled(report: &Report) -> &str {
    let lines = report.lines.iter().rev();
    lines
        .map(String::as_str)
        .find(|l| !l.starts_with("status") && !l.starts_with("promote: "))
        .unwrap()
}

/// Stage 3 on the recorded board: tracker#596, whose applied plan's
/// sub-issues have all closed since, is asked about rather than planned;
/// #890's changes wait a day after its last plan; #766 is held by its label.
pub(super) const EPIC_LINES: &[&str] = &[
    "epics: 3 live of 3 open in cgwalters-forge/tracker",
    "hold epic cgwalters-forge/tracker#596: every sub-issue is closed; its sub-issues changed (cgwalters-forge/agentic-job#630 closed, cgwalters-forge/agentic-job#715 closed, cgwalters-forge/agentic-job#716 closed and 3 more); label needs-human, Why Q: Every sub-issue of this epic is closed. Close it, or comment with what is left?",
    "hold epic cgwalters-forge/tracker#890: its sub-issues changed (cgwalters-forge/agent-board#285 closed, cgwalters-forge/agent-board#286 closed, cgwalters-forge/agent-board#287 closed and 5 more); planned again from 2026-10-24T13:34:49Z, 24 hours after its last request",
    "hold epic cgwalters-forge/tracker#766: labelled needs-human; otherwise due to be planned (its sub-issues changed (cgwalters-forge/actions#83 closed, cgwalters-forge/agent-board#167 closed, cgwalters-forge/agent-board#168 closed and 2 more))",
];

/// The recorded board's children of a parent that is not a live epic, held
/// after the candidates line, under `cgwalters-forge/`.
const NOT_LIVE: &[(&str, &str)] = &[
    ("agent-board#100", "tracker#649"),
    ("agentic-job#1736", "agentic-job#1735"),
    ("tracker#159", "tracker#101"),
    ("tracker#185", "tracker#33"),
    ("tracker#226", "tracker#225"),
    ("tracker#227", "tracker#225"),
    ("tracker#228", "tracker#225"),
    ("tracker#229", "tracker#225"),
    ("tracker#230", "tracker#225"),
    ("tracker#235", "tracker#33"),
    ("tracker#291", "tracker#286"),
    ("tracker#301", "tracker#238"),
    ("tracker#403", "tracker#401"),
    ("tracker#441", "tracker#430"),
    ("tracker#447", "tracker#446"),
    ("tracker#448", "tracker#446"),
    ("tracker#449", "tracker#446"),
    ("tracker#450", "tracker#446"),
    ("tracker#650", "tracker#649"),
    ("tracker#659", "tracker#649"),
    ("tracker#666", "tracker#649"),
    ("tracker#849", "tracker#848"),
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
    expected.push("candidates: 279 (0 opened by others, 22 outside a live epic)");
    let not_live: Vec<String> = NOT_LIVE
        .iter()
        .map(|(child, parent)| {
            format!(
                "hold cgwalters-forge/{child}: its parent cgwalters-forge/{parent} is not a live epic (open and labelled epic by the operator or the bot)"
            )
        })
        .collect();
    expected.extend(not_live.iter().map(String::as_str));
    // No epic is planned, so stage 4 has every slot.
    expected.extend(STARTED);
    // No promote section: stage 4b is off. The recording's whole-board read
    // is an empty board: the Status stage has its own recording.
    expected.push("promote: off");
    expected.push("status: 0 change(s) of 0 item(s) read");
    assert_eq!(report.lines, expected);
    assert_eq!(report.failures, 0);
}

/// `triage` true, or with the scope `all`, triages each candidate without a
/// plan. The scope `epics`, an object's default (agent-board#431), triages
/// only tracker#446, the child of a live epic, and holds the unparented
/// tracker#459 and #533, which no revisit draws either. The unparented
/// tracker#527's own plan starts it under each (`overlap` lets it), and
/// agentic-job#188's dispatched plan holds it as before. Each case: the
/// setting, and the lines from the candidates line to the Status stage's.
#[test]
fn triage_scope_epics_triages_only_epic_children() {
    let triaged = |n| format!("start triage cgwalters-forge/tracker#{n} pool=claude");
    let outside = |n| format!("hold cgwalters-forge/tracker#{n}: {OUTSIDE_EPICS}");
    let ignored = "overlap ignored: cgwalters-forge/tracker#527: open pull request cgwalters-forge/tracker#832 changes `README.md`, but it shares only documentation, and the issue is in the first Priority";
    let implement = "start implement cgwalters-forge/tracker#527 plan=6332955694@2026-10-22T04:47:39Z pool=claude";
    let planned = "hold cgwalters-forge/agentic-job#188: plan 6229359427@2026-10-17T03:59:02Z was dispatched already, https://github.com/cgwalters-forge/agentic-job/issues/188#issuecomment-6229434409".to_owned();
    let all = vec![
        "candidates: 5 (0 opened by others, 0 outside a live epic)".to_owned(),
        triaged(446),
        triaged(459),
        planned.clone(),
        ignored.to_owned(),
        implement.to_owned(),
        triaged(533),
        "revisit: 1 due, seed 1792768510, drawn: cgwalters-forge/agentic-job#188 (weight 4)"
            .to_owned(),
        "start revisit cgwalters-forge/agentic-job#188 pool=claude".to_owned(),
    ];
    let epics = vec![
        "candidates: 5 (0 opened by others, 0 outside a live epic, 4 unparented: run only with a plan)".to_owned(),
        triaged(446),
        outside(459),
        planned,
        ignored.to_owned(),
        implement.to_owned(),
        outside(533),
        "revisit: none of 5 candidate(s) is due".to_owned(),
    ];
    for (setting, expected) in [
        (json!(true), &all),
        (json!({"scope": "all"}), &all),
        (json!({}), &epics),
        (json!({"scope": "epics"}), &epics),
    ] {
        let mut replay = replay();
        only(
            &mut replay,
            &[
                "cgwalters-forge/tracker#446",
                "cgwalters-forge/tracker#459",
                "cgwalters-forge/agentic-job#188",
                "cgwalters-forge/tracker#527",
                "cgwalters-forge/tracker#533",
            ],
        );
        silent(&mut replay, &[("cgwalters-forge/agentic-job#188", 20)], &[]);
        let revisit = json!({"perPass": 1, "afterDays": 14});
        let config = config(json!({"triage": setting, "revisit": revisit, "overlap": {}}));
        let report = preview(&config, &replay);
        assert_eq!(report.failures, 0, "{setting}: {:#?}", report.lines);
        let from = (report.lines.iter()).position(|l| l.starts_with("candidates: "));
        let to = (report.lines.iter()).position(|l| l.starts_with("promote: "));
        let lines = &report.lines[from.unwrap()..to.unwrap()];
        assert_eq!(lines, expected.as_slice(), "{setting}");
        let kinds: Vec<HoldKind> = (report.holds.iter())
            .filter(|e| e.hold.reason == OUTSIDE_EPICS)
            .map(|e| e.hold.kind)
            .collect();
        let held = if expected == &epics { 2 } else { 0 };
        assert_eq!(kinds, vec![HoldKind::OutsideEpics; held], "{setting}");
    }
}

/// With `overlap` set (agent-board#426), tracker#832's overlap with P0
/// tracker#527, only `README.md`, does not hold it: it starts, and the
/// overlap stays in the hold snapshot, not blocking and not the operator's.
/// Unset, it holds as in [`replay_idle_board_starts_work`].
#[test]
fn overlap_setting_ignores_a_documentation_overlap() {
    let report = preview(&config(json!({"overlap": {}})), &replay());
    let ignored = "overlap ignored: cgwalters-forge/tracker#527: open pull request \
        cgwalters-forge/tracker#832 changes `README.md`, but it shares only documentation, \
        and the issue is in the first Priority";
    let at = (report.lines.iter().position(|l| l == ignored))
        .unwrap_or_else(|| panic!("{:#?}", report.lines));
    let started = &report.lines[at + 1];
    assert!(
        started.starts_with("start implement cgwalters-forge/tracker#527 "),
        "{started}"
    );
    assert!(!report.lines.contains(&STARTED[4].to_owned()));
    assert_eq!(report.failures, 0);
    let snapshot = holds::snapshot(&report.holds, String::new(), report.tally.clone());
    let published: Vec<Value> = (snapshot.holds.iter())
        .filter(|h| h.item.as_deref() == Some("cgwalters-forge/tracker#527"))
        .map(|h| json!([h.kind, h.operator]))
        .collect();
    assert_eq!(published, [json!(["overlap-ignored", false])]);
}

/// Apply through the offline forge, including the dispatch record and the
/// actual published comment, not just a snapshot constructed from preview.
#[test]
fn apply_publishes_nonblocking_ignored_overlap() {
    for (name, draft, pushed, starts) in [
        ("documentation", false, 1, true),
        ("idle draft", true, 5, true),
        ("fresh push of old commit", true, 1, false),
    ] {
        let mut forge = Forge::new(None, false);
        only(&mut forge.replay, &["cgwalters-forge/tracker#527"]);
        let now = forge.replay.now;
        let ago = |h| (now - time::Duration::hours(h)).format(&Rfc3339).unwrap();
        let mut settings = json!({"overlap": {}, "holds": {
            "repository": "cgwalters-forge/bot-ops", "issue": 1}});
        if draft {
            // Disable only the documentation waiver by using another Priority.
            for entry in &mut forge.replay.requests {
                let nodes =
                    entry["answer"].pointer_mut("/data/repositoryOwner/projectV2/items/nodes");
                for node in nodes.and_then(Value::as_array_mut).into_iter().flatten() {
                    node["priority"] = json!({"name": "P1"});
                }
            }
            settings["overlap"]["staleHours"] = json!(0);
            let variables = json!({"owner": "cgwalters-forge", "name": "tracker", "number": 832});
            forge.replay.requests.push(json!({
                "request": graphql_key(overlap::DRAFT_QUERY, &variables),
                "answer": {"data": {"repository": {"pullRequest": {
                    "isDraft": true, "createdAt": ago(20),
                    "headRefOid": "a".repeat(40), "headRefName": "draft",
                    "headRepository": {"nameWithOwner": "cgwalters-forge/fork"},
                    "commits": {"nodes": [{"commit": {"committedDate": ago(100)}}]}
                }}}}
            }));
            forge.replay.requests.push(json!({
                "request": {"get": format!("{API_ROOT}repos/cgwalters-forge/fork/activity?ref=refs%2Fheads%2Fdraft&direction=desc&per_page=100")},
                "answer": {"status": 200, "link": null, "body": [{
                    "activity_type": "branch_creation", "ref": "refs/heads/draft",
                    "timestamp": ago(pushed), "before": "0".repeat(40), "after": "a".repeat(40)
                }]}
            }));
        }
        for (url, body) in [
            (
                format!("{API_ROOT}repos/cgwalters-forge/bot-ops"),
                json!({"private": true}),
            ),
            (format!("{API_ROOT}user"), json!({"login": "cgwalters-bot"})),
        ] {
            forge.replay.requests.push(json!({"request": {"get": url},
                "answer": {"status": 200, "link": null, "body": body}}));
        }
        let report = apply(&forge, &config(settings));
        assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
        assert_eq!(
            forge
                .dispatches()
                .contains(&"cgwalters-forge/tracker#527".into()),
            starts,
            "{name}: {:#?}",
            report.lines
        );
        let comments = forge.comments_on("cgwalters-forge/bot-ops", 1);
        let body = comments
            .iter()
            .find(|c| c.starts_with(holds::HOLDS_MARKER))
            .unwrap();
        let snapshot = holds::parse(body).unwrap();
        let entry = snapshot["holds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["item"] == "cgwalters-forge/tracker#527")
            .unwrap();
        assert_eq!(
            entry["kind"],
            if starts { "overlap-ignored" } else { "overlap" },
            "{name}"
        );
        assert_eq!(entry["operator"], false, "{name}");
        if starts {
            assert!(
                forge
                    .comments_on("cgwalters-forge/tracker", 527)
                    .iter()
                    .any(|c| c.contains("overlap ignored:")),
                "{name}"
            );
        }
    }
}

/// Spend at the weekly budget holds stage 3's planning run as it holds
/// stage 4's: a planning run is a new run too.
#[test]
fn budget_holds_epic_planning() {
    let mut replay = replay();
    unfinished(&mut replay);
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

/// A budget without `usage.url`, as a pass whose broker is out of reach
/// runs (agent-board#457), holds on the published snapshot's spend: below
/// the budget runs start, and at it, or with no reading, they wait.
#[test]
fn budget_without_usage_url_holds_on_the_published_spend() {
    let planned = "cgwalters-forge/tracker#596";
    for (spent, expected) in [
        (Some(json!(100.0)), None),
        (Some(json!(1000.0)), Some("budget: weekly budget reached")),
        (
            None,
            Some("budget: the usage snapshot has no spend reading"),
        ),
    ] {
        let mut replay = replay();
        unfinished(&mut replay);
        let mut snapshot = usage(&mut replay);
        if let Some(spent) = &spent {
            snapshot["budget"] = json!({"weekly_usd": 1000.0, "spent_usd": spent});
        }
        set_usage(&mut replay, &snapshot);
        let config = config(json!({"budget": {"weeklyUsd": 1000}}));
        assert!(config.usage.as_ref().unwrap().url.is_none());
        let report = preview(&config, &replay);
        let line = (report.lines.iter())
            .find(|l| {
                l.contains(planned)
                    && (l.starts_with("start planning") || l.starts_with("hold epic"))
            })
            .unwrap_or_else(|| panic!("{spent:?}: {:#?}", report.lines));
        match expected {
            None => assert!(line.starts_with("start planning"), "{spent:?}: {line}"),
            Some(why) => assert!(line.ends_with(why), "{spent:?}: {line}"),
        }
    }
}

/// Stage 3's read of the first page of tracker#`n`'s sub-issues.
pub(super) fn subs_request(n: u64, cursor: Value) -> Value {
    let variables = json!({"owner": "cgwalters-forge", "name": "tracker", "number": n,
        "field": "Priority", "first": epics::SUBS_PER_PAGE, "cursor": cursor});
    graphql_key(epics::EPIC_SUBS_QUERY, &variables)
}

/// The recorded first page of tracker#`n`'s sub-issues, as stage 3 reads them.
pub(super) fn subs_mut(replay: &mut Replay, n: u64) -> &mut Value {
    let request = subs_request(n, Value::Null);
    (replay.requests.iter_mut())
        .find(|e| e["request"] == request)
        .and_then(|e| e["answer"].pointer_mut("/data/repository/issue/subIssues"))
        .unwrap_or_else(|| panic!("no recorded sub-issues of tracker#{n}"))
}

/// Gives tracker#596 an open sub-issue, tracker#9999, so that it is planned
/// for its closed ones rather than asked about (agent-board#429).
pub(super) fn unfinished(replay: &mut Replay) {
    let subs = subs_mut(replay, 596);
    let mut open = subs["nodes"][0].clone();
    open["number"] = 9999.into();
    open["state"] = "OPEN".into();
    open["repository"] = json!({"nameWithOwner": "cgwalters-forge/tracker"});
    subs["nodes"].as_array_mut().unwrap().push(open);
    subs["totalCount"] = (subs["totalCount"].as_u64().unwrap() + 1).into();
}

/// Keeps only `issues` (`OWNER/REPO#N`) and the epics on every recorded board page,
/// so a scenario that scans past what the recording read stops where it does,
/// and children keep their epics' Priority.
pub(super) fn only(replay: &mut Replay, issues: &[&str]) {
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
                "wip: no slot left; 270 more candidate(s) wait",
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
                "wip: no slot left; 270 more candidate(s) wait",
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
            "an unreadable broker with a stale snapshot is one failure, and it holds",
            Box::new(|r, c| {
                r.now += time::Duration::minutes(45);
                with_broker(r, c, json!({"error": "GET /usage: HTTP status 502"}));
            }),
            vec![
                "usage error: reading the broker: GET /usage: HTTP status 502; pacing reads the published snapshot, but the usage snapshot is stale, from 2026-10-23T15:09:10Z",
                "hold pools: the usage snapshot is stale",
            ],
            0,
            1,
        ),
        (
            "an unreadable broker with a fresh snapshot is a warning, and paces by it",
            Box::new(|r, c| {
                with_broker(r, c, json!({"error": "GET /usage: timeout: global"}));
            }),
            vec![
                "usage warning: reading the broker: GET /usage: timeout: global; pacing reads the published snapshot",
                "start triage cgwalters-forge/tracker#446 pool=claude",
            ],
            6,
            0,
        ),
        (
            "an unreadable broker without a snapshot is one failure",
            Box::new(|r, c| {
                with_broker(r, c, json!({"error": "GET /usage: HTTP status 502"}));
                answer(r, "bot-ops/issues/1/comments")["body"] = json!([]);
            }),
            vec![
                "usage error: reading the broker: GET /usage: HTTP status 502; pacing reads the published snapshot, but there is no usage snapshot",
                "hold pools: there is no usage snapshot; new runs wait",
            ],
            0,
            1,
        ),
        (
            "an unreadable broker and an unreadable snapshot are one failure",
            Box::new(|r, c| {
                with_broker(r, c, json!({"error": "GET /usage: HTTP status 502"}));
                answer(r, "bot-ops/issues/1/comments")["status"] = 404.into();
            }),
            vec![
                "usage error: reading the usage snapshot: GitHub HTTP status 404",
                "usage error: reading the broker: GET /usage: HTTP status 502; pacing reads the published snapshot, but there is no usage snapshot",
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
                "hold pools: claude has no reading; openai has no reading; 277 more candidate(s) wait",
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
                "usage warning: reading the broker",
                "hold budget: the usage snapshot has no spend reading; new runs wait",
            ],
            0,
            0,
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
            0,
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
                "hold pools: claude has no reading; openai is held by its grant; 278 more candidate(s) wait",
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
                "hold pools: claude has no reading; fable has no reading; openai is held by its grant; 278 more candidate(s) wait",
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
            vec![
                "candidates: 0 (0 opened by others, 1 outside a live epic)",
                "hold cgwalters-forge/tracker#446: its parent cgwalters-forge/review#135 is not a live epic",
            ],
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
            "inWorkRepositories: the bot's epic is live too",
            Box::new(|r, c| {
                sub_issue(r, "cgwalters-forge/review", "cgwalters-bot");
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
            vec![
                "candidates: 0 (0 opened by others, 1 outside a live epic)",
                "hold cgwalters-forge/tracker#446: its parent cgwalters-forge/review#135 is not a live epic",
            ],
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
            vec![
                "candidates: 0 (0 opened by others, 1 outside a live epic)",
                "hold cgwalters-forge/tracker#446: its parent elsewhere/review#135 is not a live epic",
            ],
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
    /// Fails the first write whose method and URL contain it before it is
    /// made, unlike `fail`, which loses the answer to one that was.
    refuse: RefCell<Option<String>>,
    /// A comment another pass posted on an issue or pull request, which a
    /// read of its comments shows once this pass has posted one there: an
    /// overlapping pass's claim, posted between this one's read and its
    /// read-back.
    rival: Option<Value>,
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
            refuse: RefCell::new(None),
            rival: None,
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
        // An issue's state, which no read here answers from.
        if method == "PATCH"
            && path.split('/').nth(3) == Some("issues")
            && path.split('/').count() == 5
        {
            return Ok((200, json!({})));
        }
        if method == "PATCH" {
            let edited: u64 = path.rsplit('/').next().unwrap_or_default().parse()?;
            let at =
                (self.replay.now + time::Duration::seconds((id % 1000) as i64)).format(&Rfc3339)?;
            let mut html_url = Value::Null;
            for nodes in self.added.borrow_mut().values_mut() {
                for node in nodes.iter_mut().filter(|n| n["id"] == edited) {
                    node["body"] = body["body"].clone();
                    node["lastEditedAt"] = at.clone().into();
                    node["editor"] = json!({"login": self.login});
                    html_url = node["url"].clone();
                }
            }
            return Ok((200, json!({"html_url": html_url})));
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
        if method == "POST" && path.ends_with("/issues") {
            let repo = path
                .trim_start_matches("repos/")
                .trim_end_matches("/issues");
            let html_url = format!("https://github.com/{repo}/issues/{id}");
            return Ok((201, json!({"number": id, "html_url": html_url})));
        }
        if method == "POST" && path.ends_with("/sub_issues") {
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
        let posted = |(method, to, _): &(String, String, Value)| {
            method == "POST" && to.ends_with("/comments") && url.starts_with(&format!("{to}?"))
        };
        if let Some(rival) = &self.rival
            && self.writes.borrow().iter().any(posted)
            && let Some(comments) = response.body.as_array_mut()
        {
            comments.push(rival.clone());
        }
        Ok(response)
    }

    fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        let what = format!("{method} {url}");
        let mut refuse = self.refuse.borrow_mut();
        if refuse.as_deref().is_some_and(|r| what.contains(r)) {
            *refuse = None;
            bail!("connection refused during {what}");
        }
        drop(refuse);
        let written = self.write(method, url, body)?;
        self.failing(&what)?;
        Ok(written)
    }

    fn download(&self, url: &str, limit: u64) -> Result<Vec<u8>> {
        self.failing(&format!("GET {url}"))?;
        self.replay.download(url, limit)
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
            // And locked since then.
            Err(_) if query == CLOSE_QUERY => {
                close_read(&variables, Some("2026-10-01T00:00:00Z".into()), None)
            }
            // A comment whose edits were not recorded: the bot's, unedited.
            Err(_) if query == needs::EDITS_QUERY => {
                json!({"node": {"author": {"login": "cgwalters-bot"}, "lastEditedAt": null, "editor": null}})
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
/// `--max-writes` holds, but for one past a cap above zero where the pass
/// made progress, only to a private repository, and none where the comment
/// already has the same holds. tracker#446 is board Blocked and asks
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
    let one_past = ", one past --max-writes: it has this pass's progress";
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
        // One past a cap the usage comment filled (a cap of 1 reads the
        // broker) where the pass made progress, which is in this comment
        // alone: here its starts.
        (
            8,
            Some(1),
            true,
            None,
            format!(
                "{published}https://github.com/cgwalters-forge/bot-ops/issues/1#issuecomment-9000000022{one_past}"
            ),
            Some(&add),
        ),
        // Never past a cap of zero, nor a full one where nothing moved
        // (at WIP 2 nothing starts).
        (
            2,
            Some(1),
            true,
            None,
            format!("held by --max-writes: {publish}"),
            None,
        ),
        (
            8,
            Some(0),
            true,
            None,
            format!("held by --max-writes: {publish}"),
            None,
        ),
        (
            2,
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
            let comment = json!({"id": 7, "node_id": "IC_7", "user": {"login": "cgwalters-bot"},
                "body": body});
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
        if writes == Some(1) {
            let broker = json!({"body": usage::tests::broker()});
            with_broker(&mut forge.replay, &mut settings, broker);
        }
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
        if writes == Some(1) && wrote.is_some() {
            // A preview says what its apply would.
            let preview = (Mode::Preview, limits);
            let report = block(run(&forge, &forge, &config, State::default(), preview, now));
            let said = format!("{publish}{one_past}; a preview does not write it");
            let lines = report.unwrap().lines;
            assert!(lines.contains(&said), "{said}: {lines:#?}");
        }
        let report = block(run(&forge, &forge, &config, State::default(), mode, now)).unwrap();
        assert!(
            report.lines.iter().any(|l| l.starts_with(&line)),
            "{line}: {:#?}",
            report.lines
        );
        assert_eq!(report.failures, usize::from(!private), "{line}");
        if writes == Some(1) && wrote.is_some() {
            assert!(report.summary.contains("writes 2/1"), "{}", report.summary);
        }
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
                     "live_epics": 0, "progress_at": snapshot["at"]},
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
    // So is each parent that is not a live epic, once for all its held
    // children, waiting on the operator where one of them is P0 or P1.
    let not_live = |item: &str, operator: bool, priority: &str| {
        json!([
            format!("cgwalters-forge/{item}"),
            "epic-not-live",
            operator,
            priority
        ])
    };
    let expected = json!([
        not_live("agentic-job#1735", true, "P1"),
        not_live("tracker#33", false, "P2"),
        not_live("tracker#101", false, "P2"),
        not_live("tracker#225", false, "P2"),
        not_live("tracker#238", false, "P2"),
        not_live("tracker#286", false, "P2"),
        not_live("tracker#401", false, "P2"),
        not_live("tracker#430", true, "P1"),
        not_live("tracker#446", true, "P0"),
        ["cgwalters-forge/tracker#596", "needs-human", true, "P0"],
        not_live("tracker#649", true, "P0"),
        ["cgwalters-forge/tracker#766", "needs-human", true, null],
        not_live("tracker#848", true, "P1"),
        [
            "cgwalters-forge/tracker#890",
            "triage-cooldown",
            false,
            "P0"
        ],
    ]);
    assert_eq!(Value::Array(epics), expected);
    // No entry is a held child of its own.
    let not_live_kind = |h: &&holds::Published| h.kind == HoldKind::EpicNotLive;
    let mut kinds = snapshot.holds.iter().filter(not_live_kind);
    assert!(kinds.clone().all(|h| h.epic));
    let item = Some("cgwalters-forge/tracker#649");
    let held = kinds.find(|h| h.item.as_deref() == item);
    assert_eq!(
        held.unwrap().reason,
        "not a live epic, so its 4 open child issue(s) wait until the operator or the bot labels it epic, among them cgwalters-forge/tracker#650 (P0), cgwalters-forge/tracker#659 (P0), cgwalters-forge/tracker#666 (P0) and 1 more"
    );
}

/// `nodes` as the one page of comments [`plan::issue_comments`] reads of
/// `repo#number`, in place of any recorded.
fn comments_on(forge: &mut Forge, (repo, number): (&str, u64), nodes: Vec<Value>) {
    let (owner, name) = repo.split_once('/').unwrap();
    let variables = json!({"owner": owner, "name": name, "number": number, "after": null});
    let request = graphql_key(plan::COMMENTS_QUERY, &variables);
    forge.replay.requests.retain(|e| e["request"] != request);
    let page = json!({"nodes": nodes, "pageInfo": {"hasNextPage": false, "endCursor": null}});
    let answer = json!({"data": {"repository": {"issue": {"comments": page}}}});
    let entry = json!({"request": request, "answer": answer});
    forge.replay.requests.push(entry);
}

/// A comment `id` by `author`, unedited, written on 20 October.
fn comment_node(id: u64, author: &str, body: &str) -> Value {
    json!({"url": format!("https://github.com/o/r/issues/1#issuecomment-{id}"), "body": body,
        "createdAt": "2026-10-20T00:00:00Z", "lastEditedAt": null,
        "author": {"login": author}, "editor": null})
}

/// With `holds.needsYou`, the published snapshot lists the holds that wait
/// on the operator, with their bot summaries' questions and
/// recommendations, and each operator-action proposal, three on one issue
/// three asks; ranked by Priority then age and counted from that list, with
/// every source read whole. A pass that reads the same list again at once
/// writes nothing; one that cannot read the proposals keeps those it
/// published, carried, and is not complete; and one whose stages failed
/// publishes none, so that its `validated_at` ages rather than say an empty
/// list.
#[test]
fn apply_publishes_what_needs_you() {
    let bot_ops = format!("{API_ROOT}repos/cgwalters-forge/bot-ops");
    let broker = "cgwalters-bot/praxis-credential-broker";
    let proposals = json!({"repository": "cgwalters-forge/bot-ops", "label": "operator-action",
        "repositories": ["cgwalters-forge/tracker", broker]});
    let at = json!({"repository": "cgwalters-forge/bot-ops", "issue": 1, "needsYou": true,
        "proposals": proposals});
    let with_wip = |wip: u64| config(json!({"holds": at, "wip": wip}));
    let config = with_wip(8);
    let proposal_issues = format!(
        "{bot_ops}/issues?state=open&labels=operator-action&sort=created&direction=asc&per_page=100"
    );
    let forge_with = |comment: Option<&str>| {
        let mut forge = Forge::new(None, false);
        // #446's summary asks and recommends; #527's bold lines are empty,
        // so its hold's reason stands.
        let summaries = [
            (
                446,
                "**Decision needed:** Ship 1.0 today?\n- **Recommendation**: Yes, CI is green.\nMore.",
            ),
            (
                527,
                "Triage.\n\n**Decision needed:**\n**Recommendation:**   ",
            ),
        ];
        for (number, body) in summaries {
            let nodes = vec![comment_node(number, "cgwalters-bot", body)];
            comments_on(&mut forge, ("cgwalters-forge/tracker", number), nodes);
        }
        // Three proposals on one issue, one acting on the broker's
        // repository, and a visitor's, which is none.
        let proposal = |title: &str, repo: &str| {
            let action = json!({"type": "add_label", "repo": repo, "number": 3, "label": "ready"});
            let value = json!({"schema": crate::action::ACTION_SCHEMA, "title": title,
                "why": "Because.\nMore.", "actions": [action]});
            format!("```agent-board-action\n{value}\n```")
        };
        let tracker = "cgwalters-forge/tracker";
        let nodes = vec![
            comment_node(21, "cgwalters-bot", &proposal("Label tracker#3", tracker)),
            comment_node(22, "cgwalters", &proposal("Label the broker's #3", broker)),
            comment_node(
                23,
                "cgwalters-bot",
                &proposal("Label tracker#3 again", tracker),
            ),
            comment_node(24, "visitor", &proposal("A visitor's", tracker)),
        ];
        comments_on(&mut forge, ("cgwalters-forge/bot-ops", 5), nodes);
        // Two wait on the operator, #527 without a Priority; a parked
        // design does not.
        let labelled = [
            (446, NEEDS_HUMAN),
            (527, NEEDS_HUMAN),
            (459, "needs-design"),
        ];
        for entry in &mut forge.replay.requests {
            let nodes = entry["answer"].pointer_mut("/data/repositoryOwner/projectV2/items/nodes");
            for node in nodes.and_then(Value::as_array_mut).into_iter().flatten() {
                let number = node["content"]["number"].as_u64();
                let Some((_, label)) = labelled.iter().find(|(n, _)| Some(*n) == number) else {
                    continue;
                };
                let labels = &mut node["content"]["labels"];
                labels["nodes"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"name": label}));
                labels["totalCount"] = (labels["totalCount"].as_u64().unwrap() + 1).into();
                if number == Some(527) {
                    node["priority"] = Value::Null;
                } else if number == Some(446) {
                    node["why"] = json!({"text": "Q: Ship it?"});
                }
            }
        }
        if let Some(body) = comment {
            let comments = &mut answer(&mut forge.replay, "bot-ops/issues/1/comments")["body"];
            let comment = json!({"id": 7, "node_id": "IC_7", "user": {"login": "cgwalters-bot"},
                "body": body});
            comments.as_array_mut().unwrap().push(comment);
        }
        let reply = |body| json!({"status": 200, "link": null, "body": body});
        let issues = json!([{"number": 5}, {"number": 9, "pull_request": {}}]);
        for (url, body) in [
            (bot_ops.clone(), json!({"private": true})),
            (format!("{API_ROOT}user"), json!({"login": "cgwalters-bot"})),
            (proposal_issues.clone(), issues),
        ] {
            let request = json!({"request": {"get": url}, "answer": reply(body)});
            forge.replay.requests.push(request);
        }
        forge
    };
    let published = |config: &Config| {
        let forge = forge_with(None);
        let report = apply(&forge, config);
        (forge.writes.borrow().iter())
            .find(|(m, url, _)| m == "POST" && url.starts_with(&bot_ops))
            .map(|(_, _, body)| text(body, "/body").to_owned())
            .unwrap_or_else(|| panic!("{:#?}", report.lines))
    };
    let body = published(&config);
    let snapshot = holds::parse(&body).unwrap();
    let list = &snapshot["needs_you"];
    assert_eq!(list["schema"], holds::NEEDS_YOU_SCHEMA);
    assert_eq!(
        json!([
            list["validated_at"],
            list["changed_at"],
            list["complete"],
            list["sources"],
            list["summaries"],
            list["omitted"],
        ]),
        json!([snapshot["at"], snapshot["at"], true,
            {"epics": "complete", "items": "complete", "reviews": "off", "proposals": "complete", "digest": "off"},
            "complete", 0])
    );
    let items = list["items"].as_array().unwrap();
    assert_eq!(list["count"], items.len());
    let asks: Vec<Value> = (items.iter())
        .map(|a| {
            json!([
                a["key"],
                a["priority"],
                a["ask"],
                a["recommendation"],
                a["source"],
                a["head"]
            ])
        })
        .collect();
    let tracker = |n| format!("cgwalters-forge/tracker#{n}");
    let proposal = |n| format!("cgwalters-forge/bot-ops#5/comment-{n}");
    let because = "Because.";
    assert_eq!(
        asks,
        [
            json!([
                tracker(446),
                "P0",
                "Ship 1.0 today?",
                "Yes, CI is green.",
                "items",
                null
            ]),
            json!([
                proposal(21),
                null,
                "Label tracker#3",
                because,
                "proposals",
                null
            ]),
            json!([
                proposal(22),
                null,
                "Label the broker's #3",
                because,
                "proposals",
                null
            ]),
            json!([
                proposal(23),
                null,
                "Label tracker#3 again",
                because,
                "proposals",
                null
            ]),
            json!([
                tracker(527),
                null,
                "labelled needs-human",
                null,
                "items",
                null
            ]),
        ]
    );
    // Each proposal is acted on by itself: its action names its comment,
    // and its address is made from that.
    assert_eq!(
        json!([
            items[2]["action"],
            items[2]["source_ref"]["type"],
            items[2]["target"]
        ]),
        json!([
            {"type": "operator_action", "repository": "cgwalters-forge/bot-ops", "number": 5, "comment": 22},
            "comment",
            "https://github.com/cgwalters-forge/bot-ops/issues/5#issuecomment-22"
        ])
    );
    let design =
        (snapshot["holds"].as_array().unwrap().iter()).find(|h| h["item"] == tracker(459).as_str());
    assert_eq!(design.map(|h| &h["kind"]), Some(&json!("needs-design")));
    // Each hold ask is one of the operator's holds on an open item, and
    // each of those is one, in rank order.
    let operator: BTreeSet<String> = (snapshot["holds"].as_array().unwrap().iter())
        .filter(|h| h["operator"] == true && h["kind"] != "closed")
        .filter_map(|h| h["item"].as_str().map(str::to_ascii_lowercase))
        .collect();
    let keys: Vec<String> = (items.iter())
        .filter(|a| a["source_ref"]["type"] == "hold")
        .map(|a| text(a, "/key").to_owned())
        .collect();
    assert_eq!(keys.iter().cloned().collect::<BTreeSet<_>>(), operator);
    assert_eq!(keys.len(), operator.len());
    let ranks: Vec<(bool, &str, &str)> = (items.iter())
        .map(|a| {
            let priority = a["priority"].as_str();
            (
                priority.is_none(),
                priority.unwrap_or_default(),
                text(a, "/waiting_since"),
            )
        })
        .collect();
    assert!(ranks.is_sorted(), "{ranks:?}");
    // Fewer slots than candidates: stage 4 stops partway, so the
    // candidates past it may wait on the operator too. Every slot taken: it
    // reads none, and its holds are the last published.
    for (wip, items) in [(3, "partial"), (2, "unread")] {
        let snapshot = holds::parse(&published(&with_wip(wip))).unwrap();
        let list = &snapshot["needs_you"];
        let read = json!([list["sources"]["items"], list["complete"]]);
        assert_eq!(read, json!([items, false]), "wip {wip}");
    }
    // The same list read again at once: nothing to write.
    let forge = forge_with(Some(&body));
    let report = apply(&forge, &config);
    assert!(
        report.lines.contains(&"holds: unchanged".to_owned()),
        "{:#?}",
        report.lines
    );
    // The list a pass rewrote, its count that of its items.
    let edited = |forge: &Forge, report: &Report| {
        let edited = (forge.writes.borrow().iter())
            .find(|(m, url, _)| m == "PATCH" && url.starts_with(&bot_ops))
            .map(|(_, _, body)| text(body, "/body").to_owned());
        let edited = edited.unwrap_or_else(|| panic!("{:#?}", report.lines));
        let list = holds::parse(&edited).unwrap()["needs_you"].clone();
        assert_eq!(list["count"], list["items"].as_array().unwrap().len());
        list
    };
    let carried = |list: &Value| -> Vec<String> {
        (list["items"].as_array().unwrap().iter())
            .filter(|a| a["carried"] == true)
            .map(|a| text(a, "/key").to_owned())
            .collect()
    };
    let said = |report: &Report, line: &str| {
        let lines = &report.lines;
        assert!(lines.iter().any(|l| l.starts_with(line)), "{lines:#?}");
    };
    // The proposals unreadable: those published are kept, carried, and the
    // list says it is not complete rather than that none wait; but not
    // from a hold comment another account last edited.
    let foreign = json!({"author": {"login": "cgwalters-bot"},
        "lastEditedAt": "2026-10-20T00:00:00Z", "editor": {"login": "cgwalters"}});
    for (name, edit, kept) in [
        (
            "the bot's",
            None,
            vec![proposal(21), proposal(22), proposal(23)],
        ),
        ("edited by another", Some(foreign), vec![]),
    ] {
        let mut forge = forge_with(Some(&body));
        let listing = json!({"get": proposal_issues});
        let entry = (forge.replay.requests.iter_mut()).find(|e| e["request"] == listing);
        entry.unwrap()["answer"]["status"] = json!(502);
        if let Some(node) = edit {
            let request = graphql_key(needs::EDITS_QUERY, &json!({"id": "IC_7"}));
            let answer = json!({"data": {"node": node}});
            forge
                .replay
                .requests
                .push(json!({"request": request, "answer": answer}));
        }
        let report = apply(&forge, &config);
        let list = edited(&forge, &report);
        let read = json!([list["sources"]["proposals"], list["complete"]]);
        assert_eq!(read, json!(["unread", false]), "{name}");
        assert_eq!(carried(&list), kept, "{name}");
        said(&report, "needs-you proposals error: ");
        if kept.is_empty() {
            said(
                &report,
                "needs-you: the published list carries nothing over: ",
            );
        }
    }
    // Too few requests left for the proposals' listing: it is not sent, and
    // the proposals, read in part, keep what they published. Nor are the
    // summaries read, and the holds keep their published questions. Every
    // slot taken, stage 4 starts nothing, so spends as much on less.
    let full = with_wip(2);
    let within = |forge: &Forge, limit| {
        let bounds = Bounds::new(limit, Instant::now() + Duration::from_secs(3600));
        let state = (State::default(), Mode::Actions(&own()), Limits::default());
        let within = (state.0, state.1, state.2, forge.replay.now);
        let report = block(run_within(forge, forge, &full, within, &bounds)).unwrap();
        (report, limit - bounds.left.get())
    };
    let (_, used) = within(&forge_with(Some(&body)), PASS_REQUEST_LIMIT);
    // The hold comment's edits, the listing, its issue and two summaries
    // are the last reads of an unchanged pass: leave room for the first.
    let forge = forge_with(Some(&body));
    let (report, _) = within(&forge, used - 5 + needs::NEEDS_READS - 4);
    let list = edited(&forge, &report);
    let read = json!([
        list["sources"]["proposals"],
        list["summaries"],
        list["complete"]
    ]);
    assert_eq!(read, json!(["partial", "partial", false]));
    assert_eq!(carried(&list), [proposal(21), proposal(22), proposal(23)]);
    let asks: Vec<&Value> = (list["items"].as_array().unwrap().iter())
        .map(|a| &a["ask"])
        .collect();
    let original: Vec<&Value> = (items.iter()).map(|a| &a["ask"]).collect();
    assert_eq!(asks, original);
    said(
        &report,
        "needs-you proposals error: the pass's request budget",
    );
    // A later pass that reads the same list keeps its `changed_at`, and
    // so does one that cannot read #446's summary: it keeps the question
    // and recommendation published rather than fall back.
    let earlier = "2026-10-01T00:00:00Z";
    let older = ["changed_at", "validated_at"]
        .iter()
        .fold(body.clone(), |b, f| {
            let now = format!("\"{f}\":{}", snapshot["at"]);
            b.replace(&now, &format!("\"{f}\":\"{earlier}\""))
        });
    assert_ne!(older, body);
    for (name, unread, summaries) in [("same", false, "complete"), ("unread", true, "partial")] {
        let mut forge = forge_with(Some(&older));
        if unread {
            let variables = json!({"owner": "cgwalters-forge", "name": "tracker", "number": 446,
                "after": null});
            let request = graphql_key(plan::COMMENTS_QUERY, &variables);
            forge.replay.requests.retain(|e| e["request"] != request);
        }
        let report = apply(&forge, &config);
        let list = edited(&forge, &report);
        assert_eq!(list["changed_at"], earlier, "{name}");
        assert_eq!(list["summaries"], summaries, "{name}");
        assert_eq!(list["items"], snapshot["needs_you"]["items"], "{name}");
        if unread {
            said(
                &report,
                "needs-you summary error: cgwalters-forge/tracker#446",
            );
        }
    }
    // A pass whose stages failed publishes no list at all.
    let forge = forge_with(Some(&body));
    let past = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let state = (State::default(), Mode::Actions(&own()), Limits::default());
    let within = (state.0, state.1, state.2, forge.replay.now);
    let bounds = Bounds::new(PASS_REQUEST_LIMIT, past);
    let report = block(run_within(&forge, &forge, &config, within, &bounds)).unwrap();
    assert!(report.lines.iter().all(|l| !l.starts_with("holds")));
    assert!(forge.writes.borrow().is_empty());
}

/// With `holds`, a pass whose published snapshot has had its holds for
/// seven hours, and no progress for five, says each stall: a preview, which
/// makes no progress of its own, on the holds issue with no log to name,
/// and on each epic or P0 and P1 item held by something the pass is stuck
/// on, not on those that wait on a person.
#[test]
fn preview_says_the_stalls() {
    let at = json!({"repository": "cgwalters-forge/bot-ops", "issue": 1});
    let config = config(json!({"holds": at}));
    let report = preview(&config, &replay());
    let now = replay().now;
    let hours_ago = |h: i64| {
        (now - Duration::from_secs(h as u64 * 3600))
            .format(&Rfc3339)
            .unwrap()
    };
    let mut published = holds::snapshot(&report.holds, hours_ago(7), report.tally.clone());
    published.pass.progress_at = hours_ago(5);
    let stuck: Vec<String> = (published.holds.iter())
        .filter(|h| h.kind.stalls() && !h.operator)
        .filter(|h| h.epic || matches!(h.priority.as_deref(), Some("P0" | "P1")))
        .filter_map(|h| h.item.clone())
        .collect();
    assert!(!stuck.is_empty(), "the board has a hold to stall on");
    let mut replay = replay();
    let body = holds::render(&published).unwrap();
    let comments = &mut answer(&mut replay, "bot-ops/issues/1/comments")["body"];
    let comment = json!({"id": 7, "user": {"login": "cgwalters-bot"}, "body": body});
    comments.as_array_mut().unwrap().push(comment);
    let report = preview(&config, &replay);
    let stalls: Vec<&String> = (report.lines.iter())
        .filter(|l| l.starts_with("stall "))
        .collect();
    // Stage 4's candidates less those waiting by design.
    let idle = format!(
        "stall idle: no apply, dispatch or merge for 4 hours or more, since {}, with ",
        hours_ago(5)
    );
    let free: usize = (stalls.last().and_then(|l| l.strip_prefix(&idle)))
        .and_then(|l| l.strip_suffix(" candidate(s) free to start"))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{stalls:#?}"));
    assert!((1..report.tally.1.candidates.unwrap()).contains(&free));
    let held: Vec<String> = (stalls.iter())
        .filter_map(|l| l.strip_prefix("stall hold "))
        .map(|l| {
            l.trim_start_matches("epic ")
                .split(':')
                .next()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(held, stuck);
    let since = format!(
        "the same hold for 6 hours or more, since {}: ",
        hours_ago(7)
    );
    assert!(
        stalls[..held.len()].iter().all(|l| l.contains(&since)),
        "{stalls:#?}"
    );
}

/// The recorded board's `item` (`REPO#N` under `cgwalters-forge/`) at
/// board Status Blocked, a person's hold.
fn blocked(replay: &mut Replay, item: &str) {
    let items = "/data/repositoryOwner/projectV2/items/nodes";
    for entry in &mut replay.requests {
        let nodes = entry["answer"].pointer_mut(items);
        for node in nodes.and_then(Value::as_array_mut).into_iter().flatten() {
            let content = &node["content"];
            let on = format!(
                "{}#{}",
                text(content, "/repository/nameWithOwner"),
                content["number"]
            );
            if on == format!("cgwalters-forge/{item}") {
                node["status"] = json!({"name": "Blocked"});
            }
        }
    }
}

/// With `holds` and no `stall` setting, successive applying passes, each
/// reading the snapshot the one before published to the bot's comment,
/// keep each hold's `since` and the pass's `progress_at` until a threshold
/// (agent-board#454, #459): the idle alarm at four hours without a start,
/// tracker#527's P0 overlap's at six, each on the operator. A pass that
/// would publish the same alarms writes nothing, and they clear once the
/// hold changes (#527 board Blocked) or a run starts. tracker#446, board
/// Blocked throughout, waits on a person and never alarms. `--max-dispatch
/// 0` keeps the passes before the last from starting anything.
#[test]
fn apply_raises_and_clears_the_stalls() {
    let bot_ops = format!("{API_ROOT}repos/cgwalters-forge/bot-ops");
    // The forge's first write is the comment it adds, with the first id.
    let comment = 9_000_000_001_u64;
    let add = format!("POST {bot_ops}/issues/1/comments");
    let edit = format!("PATCH {bot_ops}/issues/comments/{comment}");
    let mut forge = Forge::new(None, false);
    blocked(&mut forge.replay, "tracker#446");
    let reply = |body| json!({"status": 200, "link": null, "body": body});
    for (url, body) in [
        (bot_ops.clone(), json!({"private": true})),
        (format!("{API_ROOT}user"), json!({"login": "cgwalters-bot"})),
    ] {
        let request = json!({"request": {"get": url}, "answer": reply(body)});
        forge.replay.requests.push(request);
    }
    let recorded = usage(&mut forge.replay);
    let config = config(json!({"holds": {"repository": "cgwalters-forge/bot-ops", "issue": 1}}));
    let start = forge.replay.now;
    let stamp = |hours| {
        (start + time::Duration::hours(hours))
            .format(&Rfc3339)
            .unwrap()
    };
    let (holds_issue, overlapped) = ("cgwalters-forge/bot-ops#1", "cgwalters-forge/tracker#527");
    let overlap = "Waiting: open pull request cgwalters-forge/tracker#832 changes `README.md`.";
    let held = format!(
        "the same hold for 6 hours or more, since {}: {overlap}",
        stamp(0)
    );
    let idle = |free: usize| {
        format!(
            "no apply, dispatch or merge for 4 hours or more, since {}, with {free} candidate(s) free to start; see the pass log https://github.com/cgwalters-forge/tracker/actions/runs/4242",
            stamp(0)
        )
    };
    // Hours after the first pass, whether it may start runs, whether #527
    // is board Blocked, its write of the hold comment, the hours of its
    // `progress_at` and of #527's `since`, and its alarms by item and reason,
    // each since the first pass. Of the 9 candidates, the dispatched
    // agentic-job#188 and tracker#468 and the Blocked ones are not free.
    type Case<'a> = (
        i64,
        bool,
        bool,
        Option<&'a String>,
        i64,
        i64,
        Vec<(&'a str, String)>,
    );
    let idle_only = vec![(holds_issue, idle(6))];
    let both = vec![(overlapped, held), (holds_issue, idle(6))];
    let changed_hold = vec![(holds_issue, idle(5))];
    // Each threshold is reached exactly: four hours, then six.
    let cases: Vec<Case> = vec![
        (0, false, false, Some(&add), 0, 0, vec![]),
        (3, false, false, None, 0, 0, vec![]),
        (4, false, false, Some(&edit), 0, 0, idle_only),
        (6, false, false, Some(&edit), 0, 0, both.clone()),
        (8, false, false, None, 0, 0, both),
        (9, false, true, Some(&edit), 0, 9, changed_hold),
        (10, true, true, Some(&edit), 10, 9, vec![]),
    ];
    for (hours, starts, changed, wrote, progress, since, alarms) in cases {
        if changed {
            blocked(&mut forge.replay, "tracker#527");
        }
        // The broker's snapshot as fresh as when it was recorded.
        let mut fresh = recorded.clone();
        let updated = at(text(&recorded, "/updated_at")) + time::Duration::hours(hours);
        fresh["updated_at"] = updated.format(&Rfc3339).unwrap().into();
        set_usage(&mut forge.replay, &fresh);
        let limits = Limits {
            dispatch: (!starts).then_some(0),
            ..Limits::default()
        };
        let before = forge.writes.borrow().len();
        let mode = (Mode::Actions(&own()), limits);
        let now = start + time::Duration::hours(hours);
        let report = block(run(&forge, &forge, &config, State::default(), mode, now)).unwrap();
        assert_eq!(report.failures, 0, "{hours}: {:#?}", report.lines);
        let stalls: Vec<&String> = (report.lines.iter())
            .filter(|l| l.starts_with("stall "))
            .collect();
        let said: Vec<String> = (alarms.iter())
            .map(|(item, reason)| match *item == holds_issue {
                true => format!("stall idle: {reason}"),
                false => format!("stall hold {item}: {reason}"),
            })
            .collect();
        assert_eq!(stalls, said.iter().collect::<Vec<_>>(), "{hours}");
        let writes = forge.writes.borrow()[before..].to_vec();
        let published: Vec<(String, String)> = (writes.iter())
            .filter(|(_, _, body)| text(body, "/body").starts_with(holds::HOLDS_MARKER))
            .map(|(m, url, body)| (format!("{m} {url}"), text(body, "/body").to_owned()))
            .collect();
        let to: Vec<&String> = published.iter().map(|(w, _)| w).collect();
        assert_eq!(to, Vec::from_iter(wrote), "{hours}: {:#?}", report.lines);
        // A pass that starts nothing writes nothing else.
        assert_eq!(forge.dispatches().is_empty(), !starts, "{hours}");
        if !starts {
            assert_eq!(writes.len(), published.len(), "{hours}: {writes:#?}");
        }
        let Some((_, body)) = published.into_iter().next() else {
            assert!(report.lines.contains(&"holds: unchanged".into()), "{hours}");
            continue;
        };
        let snapshot = holds::parse(&body).unwrap();
        assert_eq!(snapshot["at"], stamp(hours), "{hours}");
        assert_eq!(snapshot["pass"]["progress_at"], stamp(progress), "{hours}");
        let entries = snapshot["holds"].as_array().unwrap();
        let on = |item: &str, kind: &str| {
            (entries.iter())
                .find(|h| h["item"] == item && h["kind"] == kind)
                .unwrap_or_else(|| panic!("{hours}: no {kind} on {item}: {body}"))
        };
        assert_eq!(
            on("cgwalters-forge/tracker#446", "blocked")["operator"],
            false
        );
        let kind = if changed { "blocked" } else { "overlap" };
        assert_eq!(on(overlapped, kind)["since"], stamp(since), "{hours}");
        let stalled: Vec<Value> = (entries.iter())
            .filter(|h| h["kind"] == "stalled")
            .map(|h| json!([h["item"], h["reason"], h["operator"], h["since"]]))
            .collect();
        let expected: Vec<Value> = (alarms.iter())
            .map(|(item, reason)| json!([item, reason, true, stamp(0)]))
            .collect();
        assert_eq!(stalled, expected, "{hours}");
        // The next pass reads what this one published.
        let comments = &mut answer(&mut forge.replay, "bot-ops/issues/1/comments")["body"];
        let comments = comments.as_array_mut().unwrap();
        comments.retain(|c| c["id"] != comment);
        comments.push(json!({"id": comment, "user": {"login": "cgwalters-bot"}, "body": body}));
    }
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

/// An epics read GitHub refuses, as it refused one query of every epic's
/// sub-issues once tracker#333 made 19 epics, holds stage 3 for the pass,
/// with the reason, and stage 4 dispatches as it would have without it.
#[test]
fn epics_error_still_dispatches() {
    const REFUSED: &str =
        "GitHub GraphQL: RESOURCE_LIMITS_EXCEEDED: Resource limits for this query exceeded.";
    let config = config(json!({}));
    let clean = Forge::new(None, false);
    assert_eq!(apply(&clean, &config).failures, 0);
    let mut forge = Forge::new(None, false);
    let listing = graphql_key(epics::EPICS_QUERY, &json!({}))["graphql"].clone();
    let entry = (forge.replay.requests.iter_mut())
        .find(|e| e["request"]["graphql"] == listing)
        .unwrap();
    entry["answer"] = json!({"error": REFUSED});
    let report = apply(&forge, &config);
    assert!(
        report.lines.contains(&format!("epics error: {REFUSED}")),
        "{:#?}",
        report.lines
    );
    assert!(!clean.dispatches().is_empty());
    assert_eq!(forge.dispatches(), clean.dispatches());
    assert_eq!(report.failures, 1, "{:#?}", report.lines);
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

/// `forge` with the operator's direction on operator#3: `body` by `author`
/// and last edited by `editor`, in a repository `private` or not.
fn directed(forge: &mut Forge, (author, editor): (&str, Option<&str>), body: &str, private: bool) {
    let visibility = if private { "private" } else { "public" };
    let variables = json!({"owner": "cgwalters-forge", "name": "operator", "number": 3});
    let issue = json!({"url": "https://github.com/cgwalters-forge/operator/issues/3",
        "title": "Direction", "body": body, "createdAt": "2026-10-20T00:00:00Z",
        "lastEditedAt": editor.map(|_| "2026-10-27T00:00:00Z"),
        "author": {"login": author}, "editor": editor.map(|e| json!({"login": e}))});
    forge.replay.requests.extend([
        json!({"request": {"get": format!("{API_ROOT}repos/cgwalters-forge/operator")},
            "answer": {"status": 200, "link": null,
                "body": {"private": private, "visibility": visibility}}}),
        json!({"request": graphql_key(BODY_QUERY, &variables),
            "answer": {"data": {"repository": {"issue": issue}}}}),
    ]);
}

/// With `direction`, every brief the pass writes starts with the
/// operator's direction and its record carries the direction's hash; a plan
/// written by hand is sent as it is. A direction in a public repository,
/// last edited by anyone but a writer, or too long is an error, and the
/// pass goes on without it.
#[test]
fn apply_puts_the_operator_direction_in_front_of_its_briefs() {
    let config =
        config(json!({"direction": {"repository": "cgwalters-forge/operator", "issue": 3}}));
    let long = "x".repeat(DIRECTION_LIMIT + 1);
    let said = "Prefer small changes.";
    for (who, body, private, error) in [
        (("cgwalters", None), said, true, None),
        (("cgwalters-bot", Some("cgwalters")), said, true, None),
        (("cgwalters", Some("cgwalters-bot")), said, true, None),
        (
            ("cgwalters", Some("visitor")),
            said,
            true,
            Some(
                "cgwalters-forge/operator#3 was last written by visitor, neither cgwalters nor cgwalters-bot",
            ),
        ),
        (
            ("visitor", None),
            said,
            true,
            Some("was last written by visitor"),
        ),
        (
            ("cgwalters", None),
            said,
            false,
            Some("cgwalters-forge/operator is not private"),
        ),
        (
            ("cgwalters", None),
            &long,
            true,
            Some("cgwalters-forge/operator#3 has 4001 characters, more than 4000"),
        ),
    ] {
        let mut forge = Forge::new(None, false);
        implement_plan(&mut forge.replay);
        unpathed(&mut forge.replay);
        directed(&mut forge, who, body, private);
        let report = apply(&forge, &config);
        let errors: Vec<&String> = (report.lines.iter())
            .filter(|l| l.starts_with("direction error: "))
            .collect();
        assert_eq!(report.failures, errors.len(), "{:#?}", report.lines);
        match error {
            Some(error) => assert!(
                errors.len() == 1 && errors[0].contains(error),
                "{who:?}: {errors:?}"
            ),
            None => assert!(errors.is_empty(), "{who:?}: {errors:?}"),
        }
        let heading = format!("{DIRECTION_HEADING}\n\n{said}\n\n{TASK_HEADING}\n\n");
        let writes = forge.writes.borrow();
        let sent: Vec<&Value> = (writes.iter())
            .filter(|(_, url, _)| url.ends_with("/dispatches"))
            .map(|(_, _, body)| &body["inputs"])
            .collect();
        assert!(
            sent.iter().any(|i| i["kind"] == "triage")
                && sent.iter().any(|i| i["kind"] != "triage"),
            "{sent:#?}"
        );
        for inputs in sent {
            let directed = error.is_none() && inputs["kind"] == "triage";
            let task = text(inputs, "/task");
            assert_eq!(task.starts_with(&heading), directed, "{task}");
            assert!(!task.contains(DIRECTION_HEADING) || directed, "{task}");
        }
        let hash = |number: u64| {
            let records = forge.comments_on("cgwalters-forge/tracker", number);
            let mut hashes = records.iter().filter_map(|c| plan::recorded_direction(c));
            hashes.next().map(str::to_owned)
        };
        let expected = format!("{:x}", Sha256::digest(said.as_bytes()));
        let expected = error
            .is_none()
            .then(|| expected[..FINGERPRINT_CHARS].to_owned());
        assert_eq!(hash(459), expected);
        assert_eq!(hash(446), None);
    }
}

/// A triage run's answer on tracker#446, `days` from the pass's clock, by
/// `author` and edited by `editor`, for [`Forge`] to read after its record:
/// as [`Forge`] dates the comments it posts, seconds later for each, so
/// that one posted after another is newer.
fn answered(forge: &Forge, (author, editor): (&str, Option<&str>), body: &str, days: i64) {
    let id = forge.ids.get() + 1;
    forge.ids.set(id);
    let offset = time::Duration::days(days) + time::Duration::seconds((id % 1000) as i64);
    let created = (forge.replay.now + offset).format(&Rfc3339).unwrap();
    let edited = editor.map(|_| created.clone());
    let key = ("cgwalters-forge/tracker".to_owned(), 446);
    forge.added.borrow_mut().entry(key).or_default().push(json!({
        "id": id, "url": format!("https://github.com/cgwalters-forge/tracker/issues/446#issuecomment-{id}"),
        "body": body, "createdAt": created, "lastEditedAt": edited,
        "author": {"login": author}, "editor": editor.map(|e| json!({"login": e})),
    }));
}

/// The operator's issue as [`CLOSE_QUERY`] reads it, `locked` at that time
/// or not, and its body `edited` at that time or not.
fn close_read(variables: &Value, locked: Option<String>, edited: Option<String>) -> Value {
    let (repo, number) = (text(variables, "/name"), &variables["number"]);
    let locks: Vec<Value> = (locked.iter()).map(|at| json!({"createdAt": at})).collect();
    json!({"repository": {"issue": {
        "url": format!("https://github.com/cgwalters-forge/{repo}/issues/{number}"),
        "state": "OPEN", "title": "Work", "body": "Work.", "createdAt": "2026-10-01T00:00:00Z",
        "lastEditedAt": edited, "author": {"login": "cgwalters"},
        "editor": edited.as_ref().map(|_| json!({"login": "cgwalters"})),
        "locked": locked.is_some(), "locks": {"nodes": locks}, "unlocks": {"nodes": []},
        "renames": {"nodes": []}, "reopens": {"nodes": []}}}})
}

/// Makes an event of `kind` at `at` the newest of its kind in `data`, a
/// [`close_read`], which reads the newest of each kind alone.
fn add_event(data: &mut Value, kind: &str, at: &str) {
    let alias = match kind {
        "LockedEvent" => "locks",
        "UnlockedEvent" => "unlocks",
        "RenamedTitleEvent" => "renames",
        "ReopenedEvent" => "reopens",
        _ => panic!("no {kind} events are read"),
    };
    let events = data.pointer_mut(&format!("/repository/issue/{alias}/nodes"));
    *events.unwrap() = json!([{"createdAt": at}]);
}

/// With `triageFollowUpDays`, a triage that ended without a plan is acted on
/// by its verdict, read only from the bot's answer after the run's record:
/// one for the operator labels the issue, any other waits a day and then
/// triages it again only if something but the bot's comments changed, and
/// the operator's comment triages it again at once; with `triageClose`, a
/// `done` or `obsolete` one closes the issue instead. Each case: its answers
/// (author, editor, verdict or body, days from now), `triageClose` and
/// `--max-writes`, the line for tracker#446 (`{record}`, `{answer}` and
/// `{due}` filled in), whether it was triaged again and what the pass
/// wrote for the verdict: [`LABEL`], a close's `state_reason`, or nothing.
#[test]
fn apply_follows_up_a_triage_by_its_verdict() {
    type Answer = (&'static str, Option<&'static str>, &'static str, i64);
    type Case = (
        &'static str,
        Vec<Answer>,
        (bool, Option<usize>),
        &'static str,
        bool,
        &'static str,
    );
    let verdict = |v: &str| format!("Found.\n\n```{TRIAGE_FENCE}\n{{\"verdict\": \"{v}\"}}\n```\n");
    let bot = "cgwalters-bot";
    let cases: Vec<Case> = vec![
        (
            "done asks the operator to close it, which the pass does not",
            vec![(bot, None, "done", 0)],
            (false, None),
            "hold cgwalters-forge/tracker#446: triage verdict done, {answer}; label needs-human",
            false,
            LABEL,
        ),
        (
            "so does obsolete",
            vec![(bot, None, "obsolete", 0)],
            (false, None),
            "hold cgwalters-forge/tracker#446: triage verdict obsolete, {answer}; label needs-human",
            false,
            LABEL,
        ),
        (
            "with triageClose, done closes it as completed",
            vec![(bot, None, "done", 0)],
            (true, None),
            "hold cgwalters-forge/tracker#446: triage verdict done, {answer}; close as completed",
            false,
            "completed",
        ),
        (
            "and obsolete as not planned",
            vec![(bot, None, "obsolete", 0)],
            (true, None),
            "hold cgwalters-forge/tracker#446: triage verdict obsolete, {answer}; close as not_planned",
            false,
            "not_planned",
        ),
        (
            "with triageClose, a visitor's comment keeps it open",
            vec![
                ("visitor", None, "Agreed, done.", 0),
                (bot, None, "done", 0),
            ],
            (true, None),
            "hold cgwalters-forge/tracker#446: triage verdict done, {answer}; label needs-human, not closed: its body or a comment is not only a writer's",
            false,
            LABEL,
        ),
        (
            "unlocked, it keeps it open, as a visitor's comment may have been deleted",
            vec![(bot, None, "done", 0), ("", None, "unlocked", 0)],
            (true, None),
            "hold cgwalters-forge/tracker#446: triage verdict done, {answer}; label needs-human, not closed: it was not locked throughout its triage",
            false,
            LABEL,
        ),
        (
            "so does a lock only after its triage started",
            vec![(bot, None, "done", 0), ("", None, "locked", 1)],
            (true, None),
            "hold cgwalters-forge/tracker#446: triage verdict done, {answer}; label needs-human, not closed: it was not locked throughout its triage",
            false,
            LABEL,
        ),
        (
            "and the operator's edit of the body after the verdict",
            vec![(bot, None, "done", 0), ("", None, "edited", 1)],
            (true, None),
            "hold cgwalters-forge/tracker#446: triage verdict done, {answer}; label needs-human, not closed: the issue was edited after this verdict",
            false,
            LABEL,
        ),
        (
            "reopened after its line, or not closed, it asks the operator",
            vec![(bot, None, "done", 0), (bot, None, "closed", 0)],
            (true, None),
            "hold cgwalters-forge/tracker#446: triage verdict done, {answer}; label needs-human, not closed: it was reopened, or not closed, after the pass's line for this verdict",
            false,
            LABEL,
        ),
        (
            "so does one closed by anyone else and reopened, without the line",
            vec![(bot, None, "done", 0), ("", None, "reopened", 1)],
            (true, None),
            "hold cgwalters-forge/tracker#446: triage verdict done, {answer}; label needs-human, not closed: it was reopened after this verdict",
            false,
            LABEL,
        ),
        (
            "a supervisor note the bot posted keeps it open",
            vec![(bot, None, "note", 0), (bot, None, "done", 0)],
            (true, None),
            "hold cgwalters-forge/tracker#446: triage verdict done, {answer}; label needs-human, not closed: its body or a comment is the supervisor agent's text",
            false,
            LABEL,
        ),
        (
            "so does any comment after the verdict, an ask's hold the bot posted too",
            vec![(bot, None, "done", 0), (bot, None, "ask", 0)],
            (true, None),
            "hold cgwalters-forge/tracker#446: triage verdict done, {answer}; label needs-human, not closed: a comment was posted or edited after this verdict",
            false,
            LABEL,
        ),
        (
            "and the operator's edit of an earlier comment after the verdict",
            vec![
                ("cgwalters", None, "Also the docs.", 0),
                (bot, None, "done", 0),
                ("", None, "edit-first", 1),
            ],
            (true, None),
            "hold cgwalters-forge/tracker#446: triage verdict done, {answer}; label needs-human, not closed: a comment was posted or edited after this verdict",
            false,
            LABEL,
        ),
        (
            "but not the status comment of the triage's own run",
            vec![(bot, None, "done", 0), (bot, None, "status", 0)],
            (true, None),
            "hold cgwalters-forge/tracker#446: triage verdict done, {answer}; close as completed",
            false,
            "completed",
        ),
        (
            "needs-operator still asks the operator",
            vec![(bot, None, "needs-operator", 0)],
            (true, None),
            "hold cgwalters-forge/tracker#446: triage verdict needs-operator, {answer}; label needs-human",
            false,
            LABEL,
        ),
        (
            "needs-design asks the operator",
            vec![(bot, None, "needs-design", 0)],
            (true, None),
            "hold cgwalters-forge/tracker#446: triage verdict needs-design, {answer}; label needs-human",
            false,
            LABEL,
        ),
        (
            "--max-writes holds the label",
            vec![(bot, None, "done", 0)],
            (false, Some(0)),
            "hold cgwalters-forge/tracker#446: triage verdict done, {answer}; held by --max-writes: label needs-human",
            false,
            "",
        ),
        (
            "nor does it label without room for the comment",
            vec![(bot, None, "done", 0)],
            (false, Some(1)),
            "hold cgwalters-forge/tracker#446: triage verdict done, {answer}; held by --max-writes: label needs-human",
            false,
            "",
        ),
        (
            "nor close",
            vec![(bot, None, "obsolete", 0)],
            (true, Some(1)),
            "hold cgwalters-forge/tracker#446: triage verdict obsolete, {answer}; held by --max-writes: close as not_planned",
            false,
            "",
        ),
        (
            "a visitor's done closes nothing",
            vec![("visitor", None, "done", 0)],
            (true, None),
            "hold cgwalters-forge/tracker#446: its triage, {record}, gave no verdict",
            false,
            "",
        ),
        (
            "partly waits a day",
            vec![(bot, None, "partly", 0)],
            (false, None),
            "hold cgwalters-forge/tracker#446: triage verdict partly, {answer}; triaged again from {due} if anything changed, or once cgwalters comments",
            false,
            "",
        ),
        (
            "partly, its day past and nothing changed, is not triaged again",
            vec![(bot, None, "partly", -4)],
            (false, None),
            "hold cgwalters-forge/tracker#446: triage verdict partly, {answer}; nothing changed since its triage",
            false,
            "",
        ),
        (
            "a plan posted before fingerprints is triaged again after the days, changed or not",
            vec![("", None, "unmarked", 0), (bot, None, "partly", -4)],
            (false, None),
            "start triage cgwalters-forge/tracker#446 pool=claude record=",
            true,
            "",
        ),
        (
            "nor after the bot's own comment",
            vec![(bot, None, "partly", -4), (bot, None, "Still partly.", -3)],
            (false, None),
            "hold cgwalters-forge/tracker#446: triage verdict partly, {answer}; nothing changed since its triage",
            false,
            "",
        ),
        (
            "a comment by anyone else since makes it due",
            vec![
                (bot, None, "partly", -4),
                ("visitor", None, "Fixed by #9?", -3),
            ],
            (false, None),
            "start triage cgwalters-forge/tracker#446 pool=claude record=",
            true,
            "",
        ),
        (
            "the operator's comment after the verdict triages it again, unlabelled",
            vec![
                (bot, None, "done", 0),
                ("cgwalters", None, "Not done: see #2.", 0),
            ],
            (false, None),
            "start triage cgwalters-forge/tracker#446 pool=claude record=",
            true,
            "",
        ),
        (
            "a visitor's comment after it does not",
            vec![
                (bot, None, "partly", 0),
                ("visitor", None, "Please retry.", 0),
            ],
            (false, None),
            "hold cgwalters-forge/tracker#446: triage verdict partly, {answer}",
            false,
            "",
        ),
        (
            "a visitor's verdict is none",
            vec![("visitor", None, "done", 0)],
            (false, None),
            "hold cgwalters-forge/tracker#446: its triage, {record}, gave no verdict; triaged again from",
            false,
            "",
        ),
        (
            // Posted after the plan, it is a writer's comment someone else
            // edited, which the plan protocol holds as a tampered plan.
            "a verdict a visitor edited is none",
            vec![(bot, Some("visitor"), "done", 0)],
            (false, None),
            "hold cgwalters-forge/tracker#446: its newest plan is invalid: newest plan {answer} was last edited by visitor, who may not write plans",
            false,
            "",
        ),
        (
            "two verdicts in one answer are none",
            vec![(bot, None, "two", 0)],
            (false, None),
            "hold cgwalters-forge/tracker#446: its triage, {record}, gave no verdict",
            false,
            "",
        ),
    ];
    for (name, answers, (close, writes), expected, again, held) in cases {
        let config = config(json!({"triageFollowUpDays": 3, "triageClose": close}));
        let mut forge = Forge::new(None, false);
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
            // The operator's edit, `days` from now, of their first answer.
            if said == "edit-first" {
                let at = (forge.replay.now + time::Duration::days(days))
                    .format(&Rfc3339)
                    .unwrap();
                let key = ("cgwalters-forge/tracker".to_owned(), 446);
                let mut added = forge.added.borrow_mut();
                let first = (added.get_mut(&key).unwrap().iter_mut())
                    .find(|c| c["author"]["login"] == "cgwalters")
                    .unwrap();
                first["lastEditedAt"] = at.into();
                first["editor"] = json!({"login": "cgwalters"});
                continue;
            }
            // The issue as a close reads it: not locked, locked `days` from
            // now, or locked but its body edited or the issue reopened then.
            if let "unlocked" | "locked" | "edited" | "reopened" = said {
                let at = (forge.replay.now + time::Duration::days(days))
                    .format(&Rfc3339)
                    .unwrap();
                let since = Some("2026-10-01T00:00:00Z".to_owned());
                let (locked, edited) = match said {
                    "unlocked" => (None, None),
                    "locked" => (Some(at.clone()), None),
                    "edited" => (since, Some(at.clone())),
                    _ => (since, None),
                };
                let variables =
                    json!({"owner": "cgwalters-forge", "name": "tracker", "number": 446});
                let mut data = close_read(&variables, locked, edited);
                if said == "reopened" {
                    add_event(&mut data, "ReopenedEvent", &at);
                }
                forge.replay.requests.push(json!({
                    "request": graphql_key(CLOSE_QUERY, &variables),
                    "answer": {"data": data}}));
                continue;
            }
            let body = match said {
                "two" => verdict("done").repeat(2),
                // The pass's line for a close on the first answer.
                "closed" => {
                    let key = ("cgwalters-forge/tracker".to_owned(), 446);
                    let first = text(&forge.added.borrow()[&key][2], "/url").to_owned();
                    let line = &wrote("", &first, "completed")[0].1;
                    line.as_str().unwrap().to_owned()
                }
                "note" => format!("Close it.\n\n{}", crate::input::SUPERVISOR_NOTE),
                // `ask create --apply`'s hold, crates/agent-board/src/ask.rs.
                "ask" => "An operator question is being opened for this item; any earlier triage verdict no longer authorizes closure.".to_owned(),
                // agentic-job's status comment for the run the record names.
                "status" => {
                    let key = ("cgwalters-forge/tracker".to_owned(), 446);
                    let record = text(&forge.added.borrow()[&key][1], "/body").to_owned();
                    let run = epics::record_run(&record).unwrap();
                    format!("Done.{}", crate::supervise::health::status_end(run))
                }
                text if text.contains(' ') => text.to_owned(),
                said => verdict(said),
            };
            answered(&forge, (author, editor), &body, days);
        }
        let added = forge.added.borrow()[&("cgwalters-forge/tracker".to_owned(), 446)].clone();
        // The first verdict, after the plan and its record.
        let first = (added[2..].iter())
            .find(|c| text(c, "/body").contains(TRIAGE_FENCE))
            .unwrap_or(&added[2]);
        let answer = text(first, "/url").to_owned();
        let due = (at(text(first, "/createdAt")) + time::Duration::days(1))
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
        // A preview first, which says the same and writes nothing.
        let written = forge.writes.borrow().len();
        let preview = block(run(
            &forge,
            &forge,
            &config,
            State::default(),
            (Mode::Preview, limits),
            forge.replay.now,
        ))
        .unwrap();
        assert_eq!(preview.failures, 0, "{name}: {:#?}", preview.lines);
        assert_eq!(
            forge.writes.borrow().len(),
            written,
            "{name}: preview wrote"
        );
        if !held.is_empty() {
            let line = preview.lines.iter().find(|l| l.contains("tracker#446"));
            assert!(
                line.is_some_and(|l| l.starts_with(&expected)),
                "{name}: no preview line {expected:?} in {:#?}",
                preview.lines
            );
        }
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
        let issue = format!("{API_ROOT}repos/cgwalters-forge/tracker/issues/446");
        assert_eq!(
            verdict_writes(&forge, &issue),
            wrote(&issue, &answer, held),
            "{name}"
        );
    }
}

/// Before a first triage, the bot's earlier verdict with nothing after it
/// but the bot's comments, and the issue's body and title not edited since,
/// is acted on as a follow-up's would be, rather than triaged again. Each
/// case: the pass's settings, the answers before it (as in
/// [`apply_follows_up_a_triage_by_its_verdict`], the first verdict
/// `{answer}`), what of the issue was edited since (`body`, `title` or
/// nothing), the line for tracker#446 (`{answer}` filled in) and what the
/// pass wrote for it, or `None` for a triage.
#[test]
fn apply_takes_an_earlier_triage_verdict() {
    type Answer = (&'static str, Option<&'static str>, &'static str, i64);
    type Case<'a> = (
        &'a str,
        &'a Value,
        Vec<Answer>,
        &'a str,
        Option<(&'a str, &'a str)>,
    );
    let verdict = |v: &str| format!("Found.\n\n```{TRIAGE_FENCE}\n{{\"verdict\": \"{v}\"}}\n```\n");
    let bot = "cgwalters-bot";
    let off = json!({});
    let on = json!({"triageFollowUpDays": 3});
    let close = json!({"triageFollowUpDays": 3, "triageClose": true});
    let cases: Vec<Case> = vec![
        (
            "done asks the operator even with triageClose: no lock is known to cover its run",
            &close,
            vec![(bot, None, "done", -4)],
            "",
            Some((
                "earlier triage verdict done, {answer}; label needs-human, not closed: no lock is known to have covered its triage",
                LABEL,
            )),
        ),
        (
            "obsolete asks the operator without it",
            &on,
            vec![(bot, None, "obsolete", -4)],
            "",
            Some((
                "earlier triage verdict obsolete, {answer}; label needs-human",
                LABEL,
            )),
        ),
        (
            "partly holds for triageFollowUpDays",
            &on,
            vec![(bot, None, "partly", -1), (bot, None, "status", -1)],
            "",
            Some((
                "earlier triage verdict partly, {answer}; nothing changed since, triaged from ",
                "",
            )),
        ),
        (
            "a comment of the bot's since triages it, an ask's hold say",
            &on,
            vec![(bot, None, "partly", -1), (bot, None, "Still partly.", -1)],
            "",
            None,
        ),
        (
            "so does an edit since of an earlier comment",
            &close,
            vec![
                ("cgwalters", None, "Also the docs.", -5),
                (bot, None, "done", -4),
                ("", None, "edit-first", -3),
            ],
            "",
            None,
        ),
        (
            "and is triaged after them",
            &on,
            vec![(bot, None, "partly", -4)],
            "",
            None,
        ),
        (
            "a visitor's comment before the verdict keeps it open",
            &close,
            vec![
                ("visitor", None, "It is done.", -5),
                (bot, None, "done", -4),
            ],
            "",
            Some((
                "earlier triage verdict done, {answer}; label needs-human, not closed: no lock is known to have covered its triage",
                LABEL,
            )),
        ),
        (
            "a comment by anyone else since triages it",
            &on,
            vec![(bot, None, "partly", -4), ("visitor", None, "Still?", -3)],
            "",
            None,
        ),
        (
            "so does the operator's",
            &close,
            vec![(bot, None, "done", -4), ("cgwalters", None, "No.", -3)],
            "",
            None,
        ),
        (
            "so does an edit of the body since",
            &close,
            vec![(bot, None, "done", -4)],
            "body",
            None,
        ),
        (
            "and of the title alone",
            &close,
            vec![(bot, None, "done", -4)],
            "title",
            None,
        ),
        (
            "a visitor's verdict is none",
            &close,
            vec![("visitor", None, "done", -4)],
            "",
            None,
        ),
        (
            "nor any without triageFollowUpDays",
            &off,
            vec![(bot, None, "done", -4)],
            "",
            None,
        ),
    ];
    for (name, settings, answers, edited, expected) in cases {
        let config = config(settings.clone());
        let mut forge = Forge::new(None, false);
        let variables = json!({"owner": "cgwalters-forge", "name": "tracker", "number": 446});
        let at = (forge.replay.now - time::Duration::days(1))
            .format(&Rfc3339)
            .unwrap();
        // Only the answers below: the recording's comments are newer than
        // some of them, which would be news each verdict did not weigh.
        for entry in &mut forge.replay.requests {
            if entry["request"]["variables"]["number"] == 446
                && let Some(nodes) =
                    entry["answer"].pointer_mut("/data/repository/issue/comments/nodes")
            {
                *nodes = json!([]);
            }
        }
        let since = Some("2026-10-01T00:00:00Z".to_owned());
        let body = (edited == "body").then(|| at.clone());
        let mut data = close_read(&variables, since, body);
        if edited == "title" {
            add_event(&mut data, "RenamedTitleEvent", &at);
        }
        forge
            .replay
            .requests
            .push(json!({"request": graphql_key(CLOSE_QUERY, &variables),
                "answer": {"data": data}}));
        for (author, editor, said, days) in answers {
            if said == "edit-first" {
                let at = (forge.replay.now + time::Duration::days(days))
                    .format(&Rfc3339)
                    .unwrap();
                let key = ("cgwalters-forge/tracker".to_owned(), 446);
                let mut added = forge.added.borrow_mut();
                let first = (added.get_mut(&key).unwrap().iter_mut())
                    .find(|c| c["author"]["login"] == "cgwalters")
                    .unwrap();
                first["lastEditedAt"] = at.into();
                first["editor"] = json!({"login": "cgwalters"});
                continue;
            }
            let body = match said {
                "done" | "obsolete" | "partly" => verdict(said),
                "status" => format!("Done.{}", crate::supervise::health::status_end(7)),
                text => text.to_owned(),
            };
            answered(&forge, (author, editor), &body, days);
        }
        let added = forge.added.borrow()[&("cgwalters-forge/tracker".to_owned(), 446)].clone();
        let fenced = added
            .iter()
            .find(|c| text(c, "/body").contains(TRIAGE_FENCE));
        let answer = text(fenced.unwrap(), "/url").to_owned();
        let report = apply(&forge, &config);
        assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
        let line = report.lines.iter().find(|l| l.contains("tracker#446"));
        let issue = format!("{API_ROOT}repos/cgwalters-forge/tracker/issues/446");
        let triaged = forge
            .dispatches()
            .iter()
            .any(|d| d.ends_with("tracker#446"));
        match expected {
            Some((said, held)) => {
                let said = format!(
                    "hold cgwalters-forge/tracker#446: {}",
                    said.replace("{answer}", &answer)
                );
                assert!(
                    line.is_some_and(|l| l.starts_with(&said)),
                    "{name}: no line {said:?} in {:#?}",
                    report.lines
                );
                assert!(!triaged, "{name}");
                assert_eq!(
                    verdict_writes(&forge, &issue),
                    wrote(&issue, &answer, held),
                    "{name}"
                );
            }
            None => {
                assert!(triaged, "{name}: {:#?}", report.lines);
                assert_eq!(verdict_writes(&forge, &issue), vec![], "{name}");
            }
        }
    }
}

/// What [`apply_follows_up_a_triage_by_its_verdict`]'s `held` stands for.
const LABEL: &str = "label";

/// The writes acting on a triage verdict on `issue` (its API URL): a
/// label, a close and the comments that say so, by their first line.
fn verdict_writes(forge: &Forge, issue: &str) -> Vec<(String, Value)> {
    (forge.writes.borrow().iter())
        .filter(|(_, url, body)| {
            let said = text(body, "/body");
            let acted = url.ends_with("/labels")
                || url == issue
                || said.starts_with("<!-- agent-board-hold")
                || said.starts_with("The board pass closed");
            acted && url.starts_with(issue)
        })
        .map(|(_, url, body)| match body["body"].as_str() {
            Some(text) => (url.clone(), text.lines().next().into()),
            None => (url.clone(), body.clone()),
        })
        .collect()
}

/// [`verdict_writes`] of a verdict, the comment `answer`, `held` as
/// [`apply_follows_up_a_triage_by_its_verdict`] names it: the label and
/// the comment naming the verdict, or the line linking it and the close.
fn wrote(issue: &str, answer: &str, held: &str) -> Vec<(String, Value)> {
    let id = answer.rsplit_once("#issuecomment-").unwrap().1;
    match held {
        "" => vec![],
        LABEL => vec![
            (
                format!("{issue}/labels"),
                json!({"labels": ["needs-human"]}),
            ),
            (
                format!("{issue}/comments"),
                format!("<!-- agent-board-hold: verdict {id} -->").into(),
            ),
        ],
        reason => vec![
            (
                format!("{issue}/comments"),
                format!(
                    "The board pass closed this issue as {} on [this triage verdict]({answer}); reopen it if that is wrong.",
                    reason.replace('_', " ")
                )
                .into(),
            ),
            (
                issue.to_owned(),
                json!({"state": "closed", "state_reason": reason}),
            ),
        ],
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
    for (days, again) in [(-1, false), (-3, true)] {
        let forge = Forge::new(None, false);
        apply(&forge, &config);
        answered(&forge, ("cgwalters-bot", None), &partly, -4);
        answered(&forge, ("visitor", None), "Fixed by #9?", -4);
        let second = apply(&forge, &config);
        let start = "start triage cgwalters-forge/tracker#446 ";
        let started = |report: &Report| report.lines.iter().any(|l| l.starts_with(start));
        assert!(started(&second), "{:#?}", second.lines);
        answered(&forge, ("cgwalters-bot", None), &partly, days);
        let key = ("cgwalters-forge/tracker".to_owned(), 446);
        let second_verdict = at(text(
            forge.added.borrow()[&key].last().unwrap(),
            "/createdAt",
        ));
        answered(&forge, ("visitor", None), "Or by #10?", days);
        let third = apply(&forge, &config);
        assert_eq!(third.failures, 0, "{:#?}", third.lines);
        assert_eq!(started(&third), again, "{days}: {:#?}", third.lines);
        if !again {
            let due = second_verdict + time::Duration::days(2);
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

/// Under the triage scope `epics`, tracker#446 made an issue outside every
/// live epic after its triage: a verdict for the operator still labels it,
/// but nothing triages it again, neither its wait, a change nor the
/// operator's comment. Each case: its answers (author, verdict or body,
/// days from now) and how its line ends.
#[test]
fn triage_scope_epics_follows_up_no_issue_outside_them() {
    let verdict = |v: &str| format!("```{TRIAGE_FENCE}\n{{\"verdict\": \"{v}\"}}\n```\n");
    let bot = "cgwalters-bot";
    type Answer = (&'static str, String, i64);
    let cases: Vec<(&str, Vec<Answer>, &str)> = vec![
        (
            "done still asks the operator",
            vec![(bot, verdict("done"), 0)],
            "; label needs-human",
        ),
        (
            "partly, its day past and changed since, is not triaged again",
            vec![
                (bot, verdict("partly"), -4),
                ("visitor", "Fixed by #9?".into(), -3),
            ],
            OUTSIDE_EPICS,
        ),
        (
            "nor after the operator's comment",
            vec![
                (bot, verdict("partly"), 0),
                ("cgwalters", "Not done: see #2.".into(), 0),
            ],
            OUTSIDE_EPICS,
        ),
    ];
    let all = config(json!({"triageFollowUpDays": 3}));
    let epics = config(json!({"triageFollowUpDays": 3, "triage": {"scope": "epics"}}));
    for (name, answers, ends) in cases {
        let mut forge = Forge::new(None, false);
        apply(&forge, &all);
        let triaged = |forge: &Forge| {
            (forge.dispatches().iter())
                .filter(|d| d.ends_with("tracker#446"))
                .count()
        };
        assert_eq!(triaged(&forge), 1, "{name}");
        for entry in &mut forge.replay.requests {
            let nodes = entry["answer"]
                .pointer_mut("/data/repositoryOwner/projectV2/items/nodes")
                .and_then(Value::as_array_mut);
            for node in nodes.into_iter().flatten() {
                let content = &mut node["content"];
                if content["number"] == 446
                    && text(content, "/repository/nameWithOwner") == "cgwalters-forge/tracker"
                {
                    content["parent"] = Value::Null;
                }
            }
        }
        for (author, body, days) in answers {
            answered(&forge, (author, None), &body, days);
        }
        let report = apply(&forge, &epics);
        assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
        let line = report.lines.iter().find(|l| l.contains("tracker#446"));
        assert!(
            line.is_some_and(|l| l.ends_with(ends)),
            "{name}: no line ending {ends:?} in {:#?}",
            report.lines
        );
        assert_eq!(triaged(&forge), 1, "{name}");
    }
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
/// Once the run's pull request merged, it is closed when that closes it, or
/// else re-planned at once (agent-board#484).
/// tracker#468's record last changed 2026-10-22T03:49:24Z, a day and a half
/// before the recording. Each case: its settings, its edit of the
/// recording, the issue, how its two passes run, the start of its
/// line in each, the start of the first comment posted, if any (the intro
/// of the one triage plan dispatched, or the close's marker), and how many
/// closes were sent.
#[test]
fn apply_replans_an_open_issue_after_its_run() {
    type Case = (
        &'static str,
        Value,
        fn(&mut Replay),
        (&'static str, u64),
        (Pass, [&'static str; 2]),
        Option<&'static str>,
        usize,
    );
    /// How a case's passes run: applying, in a preview, or applying under
    /// `--max-writes`.
    #[derive(Clone, Copy)]
    enum Pass {
        Apply,
        Preview,
        Writes(usize),
    }
    use Pass::*;
    let held = "hold cgwalters-forge/tracker#468: plan 6332112108@2026-10-22T03:40:56Z was dispatched already, https://github.com/cgwalters-forge/tracker/issues/468#issuecomment-6332216967";
    let cooldown = "hold cgwalters-forge/tracker#468: plan 6332112108@2026-10-22T03:40:56Z was dispatched already, https://github.com/cgwalters-forge/tracker/issues/468#issuecomment-6332216967; re-planned from 2026-10-24T03:49:24Z";
    let close = "hold cgwalters-forge/tracker#468: its dispatched pull request https://github.com/cgwalters-forge/tracker/pull/901 merged and finishes it; close as completed";
    let capped = "hold cgwalters-forge/tracker#468: its dispatched pull request https://github.com/cgwalters-forge/tracker/pull/901 merged and finishes it; held by --max-writes: close as completed";
    let closed = "hold cgwalters-forge/tracker#468: its dispatched pull request https://github.com/cgwalters-forge/tracker/pull/901 merged; closed already";
    let outside = "hold cgwalters-forge/tracker#468: plan 6332112108@2026-10-22T03:40:56Z was dispatched already, https://github.com/cgwalters-forge/tracker/issues/468#issuecomment-6332216967; outside every live epic";
    let replanned = [
        "start triage cgwalters-forge/tracker#468 pool=claude record=",
        "hold cgwalters-forge/tracker#468: plan ",
    ];
    let marker = "<!-- agent-board-merged: https://github.com/cgwalters-forge/tracker/pull/901 -->";
    let tracker = |n| ("cgwalters-forge/tracker", n);
    let cases: Vec<Case> = vec![
        (
            "its pull request merged and closes it: closed, with one comment",
            json!({}),
            closing,
            tracker(468),
            (Apply, [close, close]),
            Some(marker),
            2,
        ),
        (
            "a preview says it closes it, and writes nothing",
            json!({}),
            closing,
            tracker(468),
            (Preview, [close, close]),
            None,
            0,
        ),
        (
            "--max-writes holds the close and its comment together",
            json!({}),
            closing,
            tracker(468),
            (Writes(1), [capped, capped]),
            None,
            0,
        ),
        (
            "the pass's comment is there already: closed without another",
            json!({}),
            |r| {
                closing(r);
                let body = "<!-- agent-board-merged: https://github.com/cgwalters-forge/tracker/pull/901 -->\nThe board pass closed this issue as completed.";
                let mut node = comment_node(1, "cgwalters-bot", body);
                node["createdAt"] = "2026-10-23T00:00:00Z".into();
                comments_468(r).push(node);
            },
            tracker(468),
            (Apply, [close, close]),
            None,
            2,
        ),
        (
            "closed already, it is not commented on",
            json!({}),
            |r| {
                closing(r);
                let (variables, mut read) = close_read_468();
                read["repository"]["issue"]["state"] = "CLOSED".into();
                r.requests
                    .push(json!({"request": graphql_key(CLOSE_QUERY, &variables),
                    "answer": {"data": read}}));
            },
            tracker(468),
            (Apply, [closed, closed]),
            None,
            0,
        ),
        (
            "reopened since it merged, it is not closed again",
            json!({"replanAfterDays": 2}),
            |r| {
                closing(r);
                let (variables, mut read) = close_read_468();
                add_event(&mut read, "ReopenedEvent", "2026-10-23T00:00:00Z");
                r.requests
                    .push(json!({"request": graphql_key(CLOSE_QUERY, &variables),
                    "answer": {"data": read}}));
            },
            tracker(468),
            (Apply, [cooldown, cooldown]),
            None,
            0,
        ),
        (
            "one by anyone but the bot does not count",
            json!({"replanAfterDays": 2}),
            |r| merged(r, "visitor", "Closes #468", "main"),
            tracker(468),
            (Apply, [cooldown, cooldown]),
            None,
            0,
        ),
        (
            "one merged into another branch than the default does not count",
            json!({"replanAfterDays": 2}),
            |r| {
                merged(
                    r,
                    "cgwalters-bot",
                    "Closes #468",
                    "dispatch/implement/agent-run-1",
                )
            },
            tracker(468),
            (Apply, [cooldown, cooldown]),
            None,
            0,
        ),
        (
            "a batch's extra is left to its batch",
            json!({"replanAfterDays": 2}),
            |r| {
                merged(r, "cgwalters-bot", "Does #540.", "main");
                for node in comments_468(r) {
                    if text(node, "/url").ends_with("#issuecomment-6332216967") {
                        let body = format!(
                            "{}\n<!-- {}: cgwalters-forge/tracker#540 -->",
                            text(node, "/body"),
                            batch::BATCH_MARKER
                        );
                        node["body"] = body.into();
                    }
                }
            },
            tracker(468),
            (Apply, [cooldown, cooldown]),
            None,
            0,
        ),
        (
            "outside every live epic under the triage scope epics, it is not closed",
            json!({"replanAfterDays": 1, "triage": {"scope": "epics"}}),
            closing,
            tracker(468),
            (Apply, [outside, outside]),
            None,
            0,
        ),
        (
            "one `Part of` it re-plans it before its days",
            json!({"replanAfterDays": 2}),
            |r| merged(r, "cgwalters-bot", "Does some.\n\nPart of #468", "main"),
            tracker(468),
            (Apply, replanned),
            Some(REPLAN),
            0,
        ),
        (
            "one that closes no issue re-plans it before its days too",
            json!({"replanAfterDays": 2}),
            |r| merged(r, "cgwalters-bot", "Does it.", "main"),
            tracker(468),
            (Apply, replanned),
            Some(REPLAN),
            0,
        ),
        (
            "as does one closing another issue (#4680 is not #468)",
            json!({"replanAfterDays": 2}),
            |r| merged(r, "cgwalters-bot", "Closes #4680", "main"),
            tracker(468),
            (Apply, replanned),
            Some(REPLAN),
            0,
        ),
        (
            "one short of a close, with re-planning off, is dispatched already",
            json!({}),
            |r| {
                merged(
                    r,
                    "cgwalters-bot",
                    "Part of cgwalters-forge/tracker#468.",
                    "main",
                )
            },
            tracker(468),
            (Apply, [held, held]),
            None,
            0,
        ),
        (
            "off, it is dispatched already",
            json!({}),
            |_| {},
            tracker(468),
            (Apply, [held, held]),
            None,
            0,
        ),
        (
            "before its days, with no merged pull request, it waits for them",
            json!({"replanAfterDays": 2}),
            |_| {},
            tracker(468),
            (Apply, [cooldown, cooldown]),
            None,
            0,
        ),
        (
            "past them, a triage plans the rest once",
            json!({"replanAfterDays": 1}),
            |_| {},
            tracker(468),
            (
                Apply,
                [
                    "start triage cgwalters-forge/tracker#468 pool=claude record=",
                    "hold cgwalters-forge/tracker#468: plan ",
                ],
            ),
            Some(REPLAN),
            0,
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
            (
                Apply,
                [
                    "hold cgwalters-forge/tracker#468: a run is in flight, https://github.com/cgwalters-forge/tracker/actions/runs/7",
                    "hold cgwalters-forge/tracker#468: a run is in flight, https://github.com/cgwalters-forge/tracker/actions/runs/7",
                ],
            ),
            None,
            0,
        ),
        (
            "an open bot pull request holds it",
            json!({"replanAfterDays": 1}),
            |r| closing_pull(r, ("cgwalters-forge/tracker", 468), "cgwalters-bot"),
            tracker(468),
            (
                Apply,
                [
                    "hold cgwalters-forge/tracker#468: pull request cgwalters-forge/tracker#900 is open",
                    "hold cgwalters-forge/tracker#468: pull request cgwalters-forge/tracker#900 is open",
                ],
            ),
            None,
            0,
        ),
        (
            "outside every live epic under the triage scope epics, it is not",
            json!({"replanAfterDays": 1, "triage": {"scope": "epics"}}),
            |_| {},
            tracker(468),
            (
                Apply,
                [
                    "hold cgwalters-forge/tracker#468: plan 6332112108@2026-10-22T03:40:56Z was dispatched already, https://github.com/cgwalters-forge/tracker/issues/468#issuecomment-6332216967; outside every live epic",
                    "hold cgwalters-forge/tracker#468: plan 6332112108@2026-10-22T03:40:56Z was dispatched already, https://github.com/cgwalters-forge/tracker/issues/468#issuecomment-6332216967; outside every live epic",
                ],
            ),
            None,
            0,
        ),
        (
            "a triage plan's record is followed up as before",
            json!({"replanAfterDays": 1, "triageFollowUpDays": 3}),
            |_| {},
            tracker(446),
            (
                Apply,
                [
                    "start triage cgwalters-forge/tracker#446 pool=claude record=",
                    "hold cgwalters-forge/tracker#446: its triage, ",
                ],
            ),
            Some("The board pass asks a triage run to plan this issue."),
            0,
        ),
    ];
    for (name, settings, edit, (repo, number), (mode, expected), intro, closes) in cases {
        let config = config(settings);
        let mut forge = Forge::new(None, false);
        edit(&mut forge.replay);
        let item = format!("{repo}#{number}");
        for (pass, expected) in expected.into_iter().enumerate() {
            let (mode, writes) = match mode {
                Apply => (Mode::Actions(&own()), None),
                Preview => (Mode::Preview, None),
                Writes(n) => (Mode::Actions(&own()), Some(n)),
            };
            let limits = Limits {
                writes,
                ..Limits::default()
            };
            let report = block(run(
                &forge,
                &forge,
                &config,
                State::default(),
                (mode, limits),
                forge.replay.now,
            ))
            .unwrap();
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
        // A close dispatches nothing, and a triage closes nothing.
        let started = forge.dispatches().iter().filter(|d| **d == item).count();
        assert_eq!(
            started,
            usize::from(intro.is_some() && closes == 0),
            "{name}"
        );
        let comments = forge.comments_on(repo, number);
        let posted = comments.first().and_then(|c| c.lines().next());
        assert_eq!(
            posted.map(|p| p.starts_with(intro.unwrap_or_default())),
            intro.map(|_| true),
            "{name}: {comments:#?}"
        );
        let markers = comments
            .iter()
            .filter(|c| c.starts_with("<!-- agent-board-merged"));
        assert!(markers.count() <= 1, "{name}: {comments:#?}");
        let issue = format!("{API_ROOT}repos/{repo}/issues/{number}");
        let writes = forge.writes.borrow();
        let closed = writes.iter().filter(|(m, url, body)| {
            m == "PATCH"
                && *url == issue
                && *body == json!({"state": "closed", "state_reason": "completed"})
        });
        assert_eq!(closed.count(), closes, "{name}: {writes:#?}");
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
    // #468's plan names tracker#17, a pull request, whose issue read fails
    // as GitHub's does, `answer` its error; only a NOT_FOUND asks what it is.
    fn target_17(r: &mut Replay, error: &str) {
        replan_468(r, r#""item":468,"#, r#""item":17,"#);
        let variables = json!({"owner": "cgwalters-forge", "name": "tracker", "number": 17});
        r.requests.push(json!({
            "request": graphql_key(&format!("{ISSUE_QUERY}{FACTS}"), &variables),
            "answer": {"error": error},
        }));
        r.requests.push(json!({
            "request": graphql_key(KIND_QUERY, &variables),
            "answer": {"data": {"repository": {"issueOrPullRequest": {"__typename": "PullRequest"}}}},
        }));
    }
    let pull_17: Edit = |r| {
        target_17(
            r,
            "GitHub GraphQL: NOT_FOUND: Could not resolve to an Issue with the number of 17.; response may be partial",
        );
    };
    let transient_17: Edit = |r| target_17(r, "GitHub GraphQL: connection reset");
    let triage: Edit = |r| replan_468(r, r#""kind":"implement","#, r#""kind":"triage","#);
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
        // tracker#318's plan named the closed pull request composefs-rs#17.
        (
            "a plan that names a pull request waits for the operator",
            json!({"reruns": true}),
            vec![run(1)],
            pull_17,
            "hold cgwalters-forge/tracker#468: its plan's cgwalters-forge/tracker#17 is a pull request; a plan names the issue it implements",
            0,
        ),
        (
            "one whose target cannot be read is unreadable",
            json!({"reruns": true}),
            vec![run(1)],
            transient_17,
            "hold cgwalters-forge/tracker#468: its plan's cgwalters-forge/tracker#17 could not be read: GitHub GraphQL: connection reset",
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
            "a failed triage outside every live epic is not re-run under the scope epics",
            json!({"reruns": true, "triage": {"scope": "epics"}}),
            vec![run(1)],
            triage,
            "hold cgwalters-forge/tracker#468: plan 6332112108@2026-10-22T03:40:56Z's triage run failed; outside every live epic",
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

/// What agentic-job's `run` said in the outage of agent-board#440.
const NOT_STARTED: &str = "the run did not start: runner-sandbox cannot run podman";

/// The caller's newest runs as [`Scheduler::guard`] lists them, and the
/// bot's open issues in tracker, where its alarms are, `alarms`.
fn newest_runs(replay: &mut Replay, runs: Vec<Value>, alarms: Vec<Value>) {
    let get = |url: String, body: Value| json!({"request": {"get": url}, "answer": {"status": 200, "body": body}});
    let runs_url = format!(
        "{API_ROOT}repos/cgwalters-forge/tracker/actions/workflows/dispatch.yml/runs?per_page=20"
    );
    let runs = json!({"total_count": runs.len(), "workflow_runs": runs});
    replay.requests.push(get(runs_url, runs));
    let issues = format!(
        "{API_ROOT}repos/cgwalters-forge/tracker/issues?state=open&creator=cgwalters-bot&sort=created&direction=desc&per_page=100&page=1"
    );
    replay.requests.push(get(issues, alarms.into()));
}

/// An Actions job of the caller's run named `name`, its steps `names`
/// ended as `conclusions` say ("" for not at all), failed where `failed`.
fn job(name: &str, names: &[&str], conclusions: &[&str], failed: bool) -> Value {
    let steps: Vec<Value> = (names.iter().zip(conclusions))
        .map(|(name, conclusion)| match *conclusion {
            "" => json!({"name": name, "conclusion": null}),
            _ => json!({"name": name, "conclusion": conclusion}),
        })
        .collect();
    let conclusion = if failed { "failure" } else { "success" };
    json!({"name": name, "conclusion": conclusion, "steps": steps})
}

/// agentic-job's agent job, its steps ended as `conclusions` say, the
/// agent's last; failed unless the agent's step succeeded.
fn agent_job(conclusions: [&str; 4]) -> Value {
    let names = [
        "Set up job",
        "Run actions/checkout",
        "Check inference proxy reachability",
        starts::AGENT_STEP,
    ];
    job(
        "run / agent",
        &names,
        &conclusions,
        conclusions[3] != "success",
    )
}

/// The agent job, failed at the agent's step.
fn agent_failed() -> Vec<Value> {
    vec![agent_job(["success", "success", "success", "failure"])]
}

/// The agent job, failed at the agent's step, whose log is `log`.
fn agent_logged(log: &str) -> Vec<Value> {
    let mut jobs = agent_failed();
    jobs[0]["log"] = log.into();
    jobs
}

/// The log of agentic-job's agent job in the broker restart of
/// agent-board#449 (tracker run 40957702666), cut to its end.
const TOKEN_LOST_LOG: &str = "\
2026-10-29T21:29:36.8140498Z ▶ Bash: cd crates/agentic-job/src; rg -n \"Gitlab|GitLab\" apply/mod.rs | sed -n 1,80p (ok, 0s)
2026-10-29T21:29:54.6856192Z warning: the inference proxy has given no count of this run's model requests 6 times in a row: a request-capped session must stop
2026-10-29T21:29:54.8410797Z ⚠ model request count is unavailable
2026-10-29T21:29:54.8411077Z ⚠ cancelling the session
2026-10-29T21:29:54.8504575Z done: cancelled, 19 tool calls
2026-10-29T21:30:01.1105672Z warning: the inference proxy at http://100.114.231.4:18080 no longer knows this run's token (HTTP 401; did it restart?), so the token admits nothing, but there is no usage record of the run
2026-10-29T21:30:01.6539257Z The agent claude ended: budget. Redacted 0 string(s).
2026-10-29T21:30:05.6493063Z ##[error]agentic-job run ended with exit state 3
";

/// Run `id`'s jobs as [`Scheduler::before_agent`] reads them: the profile
/// job passed, then `jobs`, the first that failed with the annotation
/// `message`, and with its log where it has a `log`.
fn failed_at(replay: &mut Replay, id: u64, jobs: Vec<Value>, message: &str) {
    let get = |url: String, body: Value| json!({"request": {"get": url}, "answer": {"status": 200, "body": body}});
    let repo = format!("{API_ROOT}repos/cgwalters-forge/tracker");
    let check = id + 1;
    let mut all = vec![json!({"name": "profile", "conclusion": "success", "steps": []})];
    all.extend(jobs);
    if let Some(failed) = all.iter_mut().find(|j| j["conclusion"] == "failure") {
        failed["id"] = check.into();
        failed["check_run_url"] = format!("{repo}/check-runs/{check}").into();
        if let Some(log) = failed.as_object_mut().and_then(|j| j.remove("log")) {
            let url = format!("{repo}/actions/jobs/{check}/logs");
            (replay.requests).push(json!({"request": {"download": url}, "answer": {"text": log}}));
        }
    }
    let jobs = json!({"total_count": all.len(), "jobs": all});
    let url = format!("{repo}/actions/runs/{id}/jobs?per_page={JOBS_PER_PAGE}");
    replay.requests.push(get(url, jobs));
    let annotations = json!([
        {"annotation_level": "failure", "message": message},
        {"annotation_level": "failure", "message": "Process completed with exit code 4."},
    ]);
    let url = format!("{repo}/check-runs/{check}/annotations?per_page=50");
    replay.requests.push(get(url, annotations));
}

/// A run of the caller's that ended with `conclusion`, created `minutes`
/// before the recording.
fn ended_run(replay: &Replay, id: u64, minutes: i64, conclusion: &str) -> Value {
    let created = (replay.now - time::Duration::minutes(minutes)).format(&Rfc3339);
    json!({"id": id, "status": "completed", "conclusion": conclusion, "run_attempt": 1,
        "created_at": created.unwrap(), "display_title": format!("implement o/r#{id} (claude)"),
        "html_url": format!("https://github.com/cgwalters-forge/tracker/actions/runs/{id}")})
}

/// An open alarm issue of the bot's in tracker, number 77, of `kind`.
fn alarm_issue(kind: &str) -> Value {
    json!({"number": 77, "html_url": "https://github.com/cgwalters-forge/tracker/issues/77",
        "body": format!("<!-- agent-board-alarm: {kind} -->\nAn older text.")})
}

/// The writes to tracker's issue 77, or that file an issue there.
fn alarm_writes(forge: &Forge) -> Vec<(String, Value)> {
    let issues = format!("{API_ROOT}repos/cgwalters-forge/tracker/issues");
    (forge.writes.borrow().iter())
        .filter(|(_, u, _)| *u == issues || *u == format!("{issues}/77"))
        .map(|(m, _, b)| (m.clone(), b.clone()))
        .collect()
}

#[test]
fn apply_reruns_and_releases_runs_that_never_started() {
    // tracker#468's implement plan was dispatched as run 39718949900.
    const ID: u64 = 39718949900;
    let url = format!("https://github.com/cgwalters-forge/tracker/actions/runs/{ID}");
    let checkout = || vec![agent_job(["success", "failure", "skipped", "skipped"])];
    // A runner lost before the agent's step: no step failed.
    let lost_runner = || vec![agent_job(["success", "success", "", ""])];
    // At its timeout the agent's step is cancelled: it had started.
    let timed_out = || vec![agent_job(["success", "success", "success", "cancelled"])];
    let policy = || {
        let names = ["Set up job", "Run policy"];
        vec![job("run / policy", &names, &["success", "failure"], true)]
    };
    // The agent ran; the apply job's runner then failed to set up.
    let after = || {
        let names = ["Set up job", "Run apply"];
        let apply = job("run / apply", &names, &["failure", "skipped"], true);
        vec![agent_job(["success"; 4]), apply]
    };
    let lost = "fatal: couldn't find remote ref main";
    let state_4 = "agentic-job run ended with exit state 4";
    let quoted = "the agent said: the run did not start";
    let rerun = format!(
        "run rerun cgwalters-forge/tracker#468: its implement run {url} failed before the agent started ({NOT_STARTED}), so its failed jobs run once more pool=claude run={url}"
    );
    let released = |why: &str| {
        format!(
            "hold cgwalters-forge/tracker#468: its implement run {url} failed before the agent started again after a rerun ({why}), so its record is released and its plan starts again"
        )
    };
    let state_3 = "agentic-job run ended with exit state 3";
    let lost_why = "the inference proxy no longer knew the run's token (HTTP 401; did it restart?)";
    let lost_rerun = format!(
        "run rerun cgwalters-forge/tracker#468: its implement run {url} lost its inference token mid-run ({lost_why}), so its failed jobs run once more pool=claude run={url}"
    );
    let lost_released = format!(
        "hold cgwalters-forge/tracker#468: its implement run {url} lost its inference token mid-run again after a rerun ({lost_why}), so its record is released and its plan starts again"
    );
    // The agent's condensed output quoting agentic-job's warning.
    let quoting = "2026-10-29T21:29:36.8140498Z ▶ Bash: rg 'warning: the inference proxy at X no longer knows this run's token (HTTP 401; did it restart?)' (ok, 0s)\n";
    let held = "hold cgwalters-forge/tracker#468: plan 6332112108@2026-10-22T03:40:56Z was dispatched already".to_owned();
    let again = format!(
        "hold cgwalters-forge/tracker#468: its implement run {url} ended (failure) again after a rerun"
    );
    let starts = json!({"starts": {}});
    // Name, settings, the run's attempt, where it failed and why, the line,
    // the reruns and the records released.
    type Case<'a> = (
        &'a str,
        Value,
        u64,
        (Vec<Value>, &'a str),
        String,
        usize,
        usize,
    );
    let cases: Vec<Case> = vec![
        (
            "a run that never started is re-run, reruns or not",
            starts.clone(),
            1,
            (agent_failed(), NOT_STARTED),
            rerun,
            1,
            0,
        ),
        (
            "its rerun never started either: its plan is not spent",
            starts.clone(),
            2,
            (agent_failed(), NOT_STARTED),
            released(NOT_STARTED),
            0,
            1,
        ),
        (
            "nor where agentic-job ended with exit state 4",
            starts.clone(),
            2,
            (agent_failed(), state_4),
            released(state_4),
            0,
            1,
        ),
        (
            "nor where a step before the agent's failed",
            starts.clone(),
            2,
            (checkout(), lost),
            released(lost),
            0,
            1,
        ),
        (
            "nor where the runner was lost before it",
            starts.clone(),
            2,
            (lost_runner(), "The runner has received a shutdown signal."),
            released("The runner has received a shutdown signal."),
            0,
            1,
        ),
        (
            "a run whose broker lost its token mid-run is re-run",
            starts.clone(),
            1,
            (agent_logged(TOKEN_LOST_LOG), state_3),
            lost_rerun,
            1,
            0,
        ),
        (
            "its rerun lost it again: its plan is not spent",
            starts.clone(),
            2,
            (agent_logged(TOKEN_LOST_LOG), state_3),
            lost_released.clone(),
            0,
            1,
        ),
        (
            "nor where it then failed with exit state 1",
            starts.clone(),
            2,
            (
                agent_logged(TOKEN_LOST_LOG),
                "agentic-job run ended with exit state 1 (inference_unauthorized)",
            ),
            lost_released,
            0,
            1,
        ),
        (
            "a run stopped at a limit with its token known is not re-run",
            starts.clone(),
            2,
            (agent_logged(quoting), state_3),
            held.clone(),
            0,
            0,
        ),
        (
            "nor one whose log cannot be read, which is no error",
            starts.clone(),
            2,
            (agent_failed(), state_3),
            held.clone(),
            0,
            0,
        ),
        (
            // Exit state 2: its log is not read, which no answer is recorded for.
            "an agent that started and failed is not re-run without reruns",
            starts.clone(),
            1,
            (agent_failed(), "agentic-job run ended with exit state 2"),
            held.clone(),
            0,
            0,
        ),
        (
            "nor is one that quotes agentic-job",
            starts.clone(),
            2,
            (agent_failed(), quoted),
            held.clone(),
            0,
            0,
        ),
        (
            "nor one cancelled at its timeout",
            starts.clone(),
            2,
            (
                timed_out(),
                "The job has exceeded the maximum execution time",
            ),
            held.clone(),
            0,
            0,
        ),
        (
            "nor one whose later job failed to set up",
            starts.clone(),
            2,
            (after(), lost),
            held.clone(),
            0,
            0,
        ),
        (
            "nor a refusal at the caller's own steps",
            starts.clone(),
            1,
            (policy(), "the task is outside the bounds"),
            held,
            0,
            0,
        ),
        (
            "with reruns, an agent that failed again waits for a person",
            json!({"starts": {}, "reruns": true}),
            2,
            (agent_failed(), "agentic-job run ended with exit state 2"),
            again,
            0,
            0,
        ),
    ];
    for (name, settings, attempt, (jobs, message), expected, reruns, releases) in cases {
        let config = config(settings);
        let mut forge = Forge::new(None, false);
        let mut run = ended_run(&forge.replay, ID, 60, "failure");
        run["run_attempt"] = attempt.into();
        run["display_title"] = "implement cgwalters-forge/tracker#468 (claude)".into();
        failed(&mut forge.replay, vec![run]);
        failed_at(&mut forge.replay, ID, jobs, message);
        newest_runs(&mut forge.replay, vec![], vec![]);
        // Without the recording's open tracker#889, which overlaps it.
        let pulls = (forge.replay.requests.iter_mut())
            .filter_map(|e| e["answer"].pointer_mut("/data/repository/pullRequests/nodes"))
            .filter_map(Value::as_array_mut);
        for nodes in pulls {
            nodes.retain(|n| n["number"] != 889);
        }
        text_468(&mut forge.replay, "Work.");
        let report = apply(&forge, &config);
        assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
        let line = (report.lines.iter())
            .find(|l| l.contains("cgwalters-forge/tracker#468:"))
            .map_or("", String::as_str);
        assert!(line.starts_with(&expected), "{name}: {:#?}", report.lines);
        let writes = forge.writes.borrow();
        let sent = (writes.iter())
            .filter(|(_, u, _)| u.ends_with(&format!("/runs/{ID}/rerun-failed-jobs")))
            .count();
        assert_eq!(sent, reruns, "{name}");
        let record = format!("{API_ROOT}repos/cgwalters-forge/tracker/issues/comments/6332216967");
        let edits: Vec<&Value> = (writes.iter())
            .filter(|(m, u, _)| m == "PATCH" && *u == record)
            .map(|(_, _, b)| b)
            .collect();
        assert_eq!(edits.len(), releases, "{name}");
        for edit in edits {
            let body = text(edit, "/body");
            // No marker, so the plan is no longer recorded; no error, which
            // the issue's readers may not see.
            assert!(!body.contains("agent-board-dispatch"), "{name}: {body}");
            assert!(!body.contains(message), "{name}: {body}");
            assert!(body.ends_with("A later dispatch may take it."), "{body}");
        }
        drop(writes);
        assert!(forge.dispatches().iter().all(|d| !d.ends_with("#468")));
    }
}

#[test]
fn apply_stops_starting_while_runs_never_start() {
    let starts = json!({"starts": {}});
    let urls = |ids: &[u64]| -> String {
        let urls: Vec<String> = (ids.iter())
            .map(|id| format!("https://github.com/cgwalters-forge/tracker/actions/runs/{id}"))
            .collect();
        urls.join(", ")
    };
    let said = format!(
        "breaker: the newest 3 runs failed before the agent started: {NOT_STARTED} ({})",
        urls(&[501, 502, 503])
    );
    let stopped = format!(
        "{said}; stage 4 starts nothing until a run starts cleanly, and probes it from 2026-10-23T16:"
    );
    let probe = format!("{said}; one run starts to probe it");
    // A run in flight newer than the three, or the newest of them re-run
    // since.
    const FLYING: &str = "flying";
    const RESTARTED: &str = "restarted";
    // How long ago the newest of the three was created, what else of them
    // differs (a run's id and its message), what else happened, the open
    // alarm issues, the breaker's line, the starts and the alarm writes.
    type Case<'a> = (
        &'a str,
        i64,
        Option<(u64, &'a str)>,
        &'a str,
        Vec<Value>,
        Option<String>,
        bool,
        Vec<(&'a str, &'a str)>,
    );
    let cases: Vec<Case> = vec![
        (
            "three that never started stop the starts and file the alarm",
            10,
            None,
            "",
            vec![],
            Some(stopped.clone()),
            false,
            vec![(
                "POST",
                "<!-- agent-board-alarm: breaker -->\nThe board pass stopped",
            )],
        ),
        (
            "an open alarm is updated",
            10,
            None,
            "",
            vec![alarm_issue("breaker")],
            Some(stopped.clone()),
            false,
            vec![(
                "PATCH",
                "<!-- agent-board-alarm: breaker -->\nThe board pass stopped",
            )],
        ),
        (
            "a run in flight does not clear it",
            10,
            None,
            FLYING,
            vec![],
            Some(stopped.clone()),
            false,
            vec![("POST", "<!-- agent-board-alarm: breaker -->")],
        ),
        (
            "an hour after the newest, one run starts to probe it",
            70,
            None,
            "",
            vec![alarm_issue("breaker")],
            Some(probe),
            true,
            vec![("PATCH", "<!-- agent-board-alarm: breaker -->")],
        ),
        (
            "a run re-run since is as new as its rerun: no probe yet",
            70,
            None,
            RESTARTED,
            vec![],
            Some(stopped),
            false,
            vec![("POST", "<!-- agent-board-alarm: breaker -->")],
        ),
        (
            "an agent that started clears it, and its alarm",
            10,
            Some((501, "agentic-job run ended with exit state 2")),
            "",
            vec![alarm_issue("breaker")],
            None,
            true,
            vec![("PATCH", "closed")],
        ),
        (
            "as do different errors",
            10,
            Some((503, "the run did not start: the broker is down")),
            "",
            vec![alarm_issue("breaker")],
            None,
            true,
            vec![("PATCH", "closed")],
        ),
        (
            "with nothing open, nothing is written",
            10,
            Some((502, "agentic-job run ended with exit state 2")),
            "",
            vec![],
            None,
            true,
            vec![],
        ),
    ];
    for (name, minutes, other, extra, alarms, expected, started, writes) in cases {
        let mut forge = Forge::new(None, false);
        let replay = &mut forge.replay;
        let mut runs = Vec::new();
        if extra == FLYING {
            let mut run = ended_run(replay, 504, 5, "failure");
            run["status"] = "in_progress".into();
            run["conclusion"] = Value::Null;
            runs.push(run);
        }
        for (n, id) in [501, 502, 503].into_iter().enumerate() {
            let mut run = ended_run(replay, id, minutes + 10 * n as i64, "failure");
            if extra == RESTARTED && id == 501 {
                let started = (replay.now - time::Duration::minutes(5)).format(&Rfc3339);
                run["run_started_at"] = started.unwrap().into();
                run["run_attempt"] = 2.into();
            }
            runs.push(run);
            let message = match other {
                Some((other, message)) if other == id => message,
                _ => NOT_STARTED,
            };
            failed_at(replay, id, agent_failed(), message);
        }
        newest_runs(replay, runs, alarms);
        failed(replay, vec![]);
        let report = apply(&forge, &config(starts.clone()));
        assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
        let line = report.lines.iter().find(|l| l.starts_with("breaker"));
        match &expected {
            Some(expected) => assert!(
                line.is_some_and(|l| l.starts_with(expected.as_str())),
                "{name}: {:#?}",
                report.lines
            ),
            None => assert_eq!(line, None, "{name}"),
        }
        let starts = report.lines.iter().filter(|l| l.starts_with("start "));
        match (started, expected.is_some()) {
            // A probe takes one slot.
            (true, true) => assert_eq!(starts.count(), 1, "{name}"),
            (true, false) => assert!(starts.count() > 1, "{name}"),
            (false, _) => assert_eq!(starts.count(), 0, "{name}"),
        }
        let written = alarm_writes(&forge);
        assert_eq!(written.len(), writes.len(), "{name}: {written:#?}");
        for ((method, body), (expected, has)) in written.iter().zip(&writes) {
            assert_eq!(method, expected, "{name}");
            let shown = body["body"]
                .as_str()
                .map_or(body.to_string(), str::to_owned);
            assert!(shown.contains(has), "{name}: {body:#}");
        }
        if let Some(("POST", _)) = writes.first() {
            assert_eq!(written[0].1["labels"], json!(["needs-human"]), "{name}");
        }
    }
}

#[test]
fn apply_raises_an_alarm_when_nothing_starts() {
    // tracker#446 alone, its closing pull requests more than the board's
    // page and not read after: held as unreadable, which the pass does not
    // expect to end by itself.
    let unread = |replay: &mut Replay| {
        only(replay, &["cgwalters-forge/tracker#446"]);
        let nodes = (replay.requests.iter_mut())
            .filter_map(|e| e["answer"].pointer_mut("/data/repositoryOwner/projectV2/items/nodes"))
            .filter_map(Value::as_array_mut)
            .flatten();
        for node in nodes.filter(|n| n["content"]["number"] == 446) {
            node["content"]["closedByPullRequestsReferences"]["totalCount"] = 12.into();
        }
    };
    type Edit = fn(&mut Replay);
    let none: Edit = |_| {};
    // Name, settings, the minutes since the newest run, the edit, the open
    // alarm issues, whether it stalls and the alarm writes.
    type Case<'a> = (&'a str, Value, i64, Edit, Vec<Value>, bool, Vec<&'a str>);
    let cases: Vec<Case> = vec![
        (
            "nothing started for an hour with an unexplained hold",
            json!({"starts": {}}),
            61,
            unread,
            vec![],
            true,
            vec!["POST"],
        ),
        (
            "an open alarm is updated to say so",
            json!({"starts": {}}),
            61,
            unread,
            vec![alarm_issue("stall")],
            true,
            vec!["PATCH"],
        ),
        (
            "a run started since",
            json!({"starts": {}}),
            30,
            unread,
            vec![alarm_issue("stall")],
            false,
            vec!["PATCH"],
        ),
        (
            "a longer wait",
            json!({"starts": {"stallAfterMinutes": 120}}),
            61,
            unread,
            vec![],
            false,
            vec![],
        ),
        (
            "candidates that start",
            json!({"starts": {}}),
            61,
            none,
            vec![],
            false,
            vec![],
        ),
    ];
    for (name, settings, minutes, edit, alarms, stalled, writes) in cases {
        let mut forge = Forge::new(None, false);
        edit(&mut forge.replay);
        let run = ended_run(&forge.replay, 501, minutes, "success");
        newest_runs(&mut forge.replay, vec![run], alarms);
        failed(&mut forge.replay, vec![]);
        let report = apply(&forge, &config(settings));
        assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
        let line = report.lines.iter().find(|l| l.starts_with("stall"));
        assert_eq!(line.is_some(), stalled, "{name}: {:#?}", report.lines);
        if let Some(line) = line {
            assert!(
                line.ends_with("free and 1 candidate(s) held as unreadable"),
                "{line}"
            );
        }
        let written = alarm_writes(&forge);
        let methods: Vec<&str> = written.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(methods, writes, "{name}: {written:#?}");
        for (method, body) in &written {
            match method.as_str() {
                "POST" => assert!(
                    text(body, "/body").starts_with("<!-- agent-board-alarm: stall -->"),
                    "{body:#}"
                ),
                _ if stalled => assert!(text(body, "/body").contains("held as unreadable")),
                _ => assert_eq!(body["state"], "closed", "{name}"),
            }
        }
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
/// for the day from `from`, recording `heads`, edited by the bot as the
/// pass leaves it, where `from` is some, or else a digest whose block
/// records `heads`, as a run could write one.
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
    let comment = json!({"url": format!("{DIGEST}#issuecomment-{id}"), "body": body,
        "createdAt": "2026-10-01T00:00:00Z", "lastEditedAt": null,
        "author": {"login": "cgwalters-bot"}, "editor": null});
    match from {
        Some(_) => edited_by(comment, Some("cgwalters-bot")),
        None => comment,
    }
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

/// A state holding the day before the recording's, done.
fn yesterdays_day() -> State {
    State {
        maintain: Some(maintain::Day {
            from: "2026-10-22T09:00:00Z".into(),
            repos: vec![],
            done: true,
            configuration: None,
            comment: None,
        }),
        ..State::default()
    }
}

/// `comment` as GitHub shows it never edited: as a run adds one.
fn unedited(mut comment: Value) -> Value {
    comment["lastEditedAt"] = Value::Null;
    comment["editor"] = Value::Null;
    comment
}

/// A day's digest is due once, from `hourUtc`, for the pull requests whose
/// head changed since the pass last recorded a plan, the operator's and the
/// bot's left out, with no state of the pass's. Only the pass's own plan
/// comment, which the bot edited last, is read: heads a run or a
/// collaborator wrote, in a digest, a comment like the pass's never edited
/// or an edit of the pass's, are not.
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
            "a run's comment like the pass's",
            9,
            vec![unedited(on_digest(5, Some(TODAY), both.clone()))],
            all_due.to_owned(),
        ),
        (
            "a run's later comment like the pass's",
            9,
            vec![
                on_digest(5, yesterday, one),
                unedited(on_digest(6, Some(TODAY), both.clone())),
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
            "an older comment of the pass's",
            9,
            vec![
                {
                    let mut older = on_digest(4, Some("2026-10-21T09:00:00Z"), json!({}));
                    older["lastEditedAt"] = json!("2026-10-21T09:30:00Z");
                    older
                },
                on_digest(5, yesterday, both.clone()),
            ],
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
        let report = block(run(
            &replay,
            &replay,
            &config,
            State::default(),
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
/// still settle the day, and a pass with no state finds that record.
/// Discovered job plans only veto recovery, never supply trusted heads.
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
        // A pass with no state finds the recorded plan, or else the posted
        // plan holds the day.
        let before = forge.dispatches();
        let retry = apply(&forge, &config);
        assert_eq!(retry.failures, 0, "{case}: {:#?}", retry.lines);
        let held = if sent {
            "maintain: today's plan is "
        } else {
            "maintain: a research plan already exists today; the digest waits"
        };
        assert!(
            maintain_lines(&retry).iter().any(|l| l.starts_with(held)),
            "{case}: {:#?}",
            retry.lines
        );
        assert_eq!(forge.dispatches(), before, "{case}");
    }
}

/// An applying pass records the plan and the heads it was planned for in
/// its own comment on a private digest issue, and writes nothing else;
/// nothing is dispatched. Later passes the same day read its state and search, read
/// and list nothing; one whose state was lost trusts no marker never edited.
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
    // It adds its comment without the marker, then edits the plan in.
    let writes = forge.writes.borrow();
    let on_issue: Vec<(&str, &str, &str)> = (writes.iter())
        .filter(|(_, url, _)| url.contains("bot-ops/issues/"))
        .map(|(m, url, body)| (m.as_str(), url.as_str(), text(body, "/body")))
        .collect();
    let [("POST", _, added), ("PATCH", edit, body)] = on_issue[..] else {
        panic!("{writes:#?}")
    };
    assert!(!added.contains(maintain::PLAN_MARKER), "{added}");
    let id = forge.added.borrow()[&("cgwalters-forge/bot-ops".to_owned(), 9)][0]["id"].clone();
    assert!(edit.ends_with(&format!("/issues/comments/{id}")), "{edit}");
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
    assert_eq!(day.comment, None);
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
    // The state lost, it searches and lists again, ignoring a marker never
    // edited, as a run could post, claiming today's plan.
    let mut lost = recording();
    answer(&mut lost, comments)["data"]["repository"]["issue"]["comments"]["nodes"] =
        json!([unedited(on_digest(5, Some(TODAY), json!({})))]);
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
/// nothing, its state kept or not. The next day's pass, its state kept or
/// not, acknowledges only those the bot's digest after the plan's dispatch
/// record lists at the planned head: unlisted and changed-head entries, a
/// failed or missing digest and one by another account leave them due. Its
/// preview writes nothing.
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
    // The comment a run adds after the dispatch, by whom, and the next
    // day's lines after its repositories, in order, each with all of its
    // parts between `*`.
    let cases = [
        (
            "full",
            Some(("cgwalters-bot", full.clone())),
            vec![
                format!(
                    "maintain: the digest {DIGEST}#issuecomment-*assessed 2 of 2 planned pull request(s)"
                ),
                "maintain: none of 2 incoming pull request(s) changed since".to_owned(),
            ],
        ),
        (
            "partial",
            Some(("cgwalters-bot", digest_of(&[("up/a#1", aa.clone())], &[]))),
            vec![
                "assessed 1 of 2 planned pull request(s); due again: up/a#2".to_owned(),
                due(1, "up/a#2"),
            ],
        ),
        (
            "no digest",
            None,
            vec![missing.to_owned(), due(2, "up/a#1, up/a#2")],
        ),
        (
            "failed run",
            Some(("cgwalters-bot", "The run failed.".to_owned())),
            vec![missing.to_owned(), due(2, "up/a#1, up/a#2")],
        ),
        (
            "changed head",
            // Listed again without a head, it still moved.
            Some((
                "cgwalters-bot",
                digest_of(&[("up/a#1", head("a1")), ("up/a#2", bb)], &["up/a#1"]),
            )),
            vec![
                "assessed 1 of 2 planned pull request(s); due again: up/a#1".to_owned(),
                due(1, "up/a#1"),
            ],
        ),
        (
            "forged by another author",
            Some(("collaborator", full)),
            vec![missing.to_owned(), due(2, "up/a#1, up/a#2")],
        ),
    ];
    for (name, comment, expected) in cases {
        let (config, forge) = two_days(maintained(9, vec![], 40.0));
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
        // So does a pass that lost the state, by the recorded plan.
        let again = block(run(
            &forge,
            &forge,
            &config,
            State::default(),
            (Mode::Actions(&own()), Limits::default()),
            forge.replay.now,
        ))
        .unwrap();
        assert_eq!(again.failures, 0, "{name}: {:#?}", again.lines);
        assert!(
            maintain_lines(&again)
                .iter()
                .any(|l| l.starts_with(&format!("maintain: today's plan is {DIGEST}"))),
            "{name}: {:#?}",
            again.lines
        );
        assert_eq!(forge.dispatches().len(), 1, "{name}");
        let writes = forge.writes.borrow().len();
        // The next day reads the same with the state kept or lost.
        for state in [state, State::default()] {
            let lines = next_day(&forge, &config, state, name);
            let matches =
                |line: &str, expected: &str| expected.split('*').all(|p| line.contains(p));
            // What follows, the day's start, is the other tests'.
            assert!(lines.len() >= expected.len(), "{name}: {lines:#?}");
            for (line, expected) in lines.iter().zip(&expected) {
                assert!(matches(line, expected), "{name}: {expected}: {lines:#?}");
            }
            assert_eq!(forge.writes.borrow().len(), writes, "{name}: preview wrote");
            assert_eq!(forge.dispatches().len(), 1, "{name}");
        }
    }
}

/// The recording of `maintained`, whose digest repository is public, as a
/// forge an applying pass writes to, and that answers the next day's searches.
fn two_days((mut config, mut replay): (Config, Replay)) -> (Config, Forge) {
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
    (config, forge)
}

/// The maintain lines of a preview over `forge` the next day, from `state`,
/// after its repositories.
fn next_day(forge: &Forge, config: &Config, state: State, name: &str) -> Vec<String> {
    let next = block(run(
        forge,
        forge,
        config,
        state,
        (Mode::Preview, Limits::default()),
        forge.replay.now + time::Duration::days(1),
    ))
    .unwrap();
    assert_eq!(next.failures, 0, "{name}: {:#?}", next.lines);
    let lines = maintain_lines(&next);
    assert_eq!(lines[0], "maintain: repositories: up/a", "{name}");
    lines[1..].iter().map(|l| l.to_string()).collect()
}

/// With more incoming pull requests than one digest reviews, and no state
/// kept, the next day skips those the digest assessed at unchanged heads and
/// reviews the rest: the first day's overflow, one the digest left out and
/// one whose head moved.
#[test]
fn maintain_advances_through_more_than_a_digest() {
    let (config, mut replay) = maintained(9, vec![], 40.0);
    let listing = format!("{API_ROOT}repos/up/a/pulls?state=open&per_page=100");
    let sha = |n: u32| head(&format!("{n:02}"));
    let pulls: Vec<Value> = (1..=25)
        .map(|n| json!({"number": n, "user": {"login": "visitor"}, "head": {"sha": sha(n)}}))
        .collect();
    answer(&mut replay, &listing)["body"] = pulls.into();
    let (config, mut forge) = two_days((config, replay));
    // #3's head moves once the first day's run is dispatched.
    forge.late = Some(Box::new(move |request, answer| {
        if request == listing {
            answer[2]["head"]["sha"] = head("f3").into();
        }
    }));
    let keys =
        |numbers: &[u32]| -> Vec<String> { numbers.iter().map(|n| format!("up/a#{n}")).collect() };
    let report = apply(&forge, &config);
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    let first: Vec<u32> = (1..=20).collect();
    let due = "maintain: digest due: 25 of 25 incoming pull request(s) changed since no plan";
    let due = format!("{due}; reviews {}", keys(&first).join(", "));
    assert!(
        maintain_lines(&report).contains(&due.as_str()),
        "{:#?}",
        report.lines
    );
    // The digest assesses all it was given but #5, at the heads listed.
    let assessed: Vec<u32> = first.into_iter().filter(|n| *n != 5).collect();
    let assessed: Vec<(String, String)> = (keys(&assessed).into_iter())
        .zip(assessed.iter().map(|n| sha(*n)))
        .collect();
    let assessed: Vec<(&str, String)> = (assessed.iter())
        .map(|(pr, head)| (pr.as_str(), head.clone()))
        .collect();
    post_on_digest(&forge, "cgwalters-bot", digest_of(&assessed, &[]));
    let lines = next_day(&forge, &config, State::default(), "day 2");
    let assessed = "assessed 19 of 20 planned pull request(s); due again: up/a#5";
    assert!(lines[0].contains(assessed), "{lines:#?}");
    let due = "maintain: digest due: 7 of 25 incoming pull request(s) changed since";
    let reviews = format!("; reviews {}", keys(&[3, 5, 21, 22, 23, 24, 25]).join(", "));
    assert!(lines[1].starts_with(due), "{lines:#?}");
    assert!(lines[1].ends_with(&reviews), "{lines:#?}");
}

/// A marker the bot never edited, as a run can post one, supplies no day or
/// heads, at bootstrap, after the pass's comment was deleted or lost its
/// marker, or named by an older state's creation receipt: the pass adds a
/// comment of its own and edits the plan into it, and edits no other.
#[test]
fn maintain_reads_only_the_comment_it_edited() {
    let heads = json!({"up/a#1": "aa0000000000", "up/a#2": "bb0000000000"});
    let forged = unedited(on_digest(6, Some(TODAY), heads.clone()));
    let receipt = || {
        let mut state = yesterdays_day();
        state.maintain.as_mut().unwrap().comment = Some(format!("{DIGEST}#issuecomment-6"));
        state
    };
    let cases = [
        ("bootstrap", State::default(), vec![forged.clone()]),
        ("receipt", receipt(), vec![forged.clone()]),
        (
            "marker removed",
            State::default(),
            vec![
                edited_by(on_digest(5, None, json!({})), Some("cgwalters-bot")),
                forged,
            ],
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
        let on_issue: Vec<(&str, &str)> = (writes.iter())
            .filter(|(_, url, _)| url.contains("bot-ops/issues/"))
            .map(|(m, url, _)| (m.as_str(), url.as_str()))
            .collect();
        let key = ("cgwalters-forge/bot-ops".to_owned(), 9);
        let id = forge.added.borrow()[&key][0]["id"].clone();
        let issues = format!("{API_ROOT}repos/cgwalters-forge/bot-ops/issues");
        let added = format!("{issues}/9/comments");
        let edited = format!("{issues}/comments/{id}");
        assert_eq!(
            on_issue,
            [("POST", added.as_str()), ("PATCH", edited.as_str())],
            "{name}"
        );
        let state = State::from_json(&report.state.unwrap().to_json()).unwrap();
        assert!(state.maintain.unwrap().done, "{name}");
    }
}

/// `forge`'s writes from the `from`th on, each as its method, its path
/// below the digest repository or the last part of another, and what it
/// wrote: the pass's marked plan comment, a job plan, a dispatch record,
/// or else the body's first line.
fn digest_writes(forge: &Forge, from: usize) -> Vec<String> {
    let repo = format!("{API_ROOT}repos/cgwalters-forge/bot-ops/");
    let writes = forge.writes.borrow();
    (writes[from..].iter())
        .map(|(method, url, body)| {
            let path = match url.strip_prefix(&repo) {
                Some(path) => path,
                None => url.rsplit('/').next().unwrap_or(url),
            };
            let body = text(body, "/body");
            let what = if body.starts_with(maintain::PLAN_MARKER) {
                "marked plan"
            } else if body.starts_with("<!-- agent-board-dispatch") {
                "dispatch record"
            } else if plan::fenced_json(body, PLAN_FENCE).is_ok_and(|p| p.is_some()) {
                "job plan"
            } else {
                body.lines().next().unwrap_or("")
            };
            format!("{method} {path} {what}").trim_end().to_owned()
        })
        .collect()
}

/// The writes of a start on the digest issue, by [`digest_writes`]: the
/// job plan, its dispatch record, the dispatch and the record's edit.
const DIGEST_START: &[&str] = &[
    "POST issues/9/comments job plan",
    "POST issues/9/comments dispatch record",
    "POST dispatches",
    "PATCH issues/comments/9000000002 dispatch record",
];

/// The placeholder the pass adds its plan comment with, by [`digest_writes`].
const PLACEHOLDER: &str =
    "POST issues/9/comments The board pass is recording its maintain plan here.";

/// A start and the record after it are taken together within the caps: one
/// write left holds the start whole, as `--max-dispatch 0` does, so no
/// placeholder is added and nothing dispatched; two let the pass post and
/// dispatch its plan, which count as the one dispatch, and add the
/// placeholder and edit the plan in, the two writes. One whose plan comment
/// exists needs one. A preview counts the same and writes nothing.
#[test]
fn maintain_holds_its_start_and_record_together() {
    let yesterdays = || vec![on_digest(5, Some("2026-10-22T09:00:00Z"), json!({}))];
    let added = "PATCH issues/comments/9000000005 marked plan";
    let edited = "PATCH issues/comments/5 marked plan";
    // Its comments, `--max-dispatch` and `--max-writes`, the flag that
    // holds the start, if one does, the record's writes where it starts,
    // and the summary.
    let cases = [
        (
            vec![],
            Some(0),
            Some(2),
            Some("--max-dispatch"),
            vec![],
            "dispatched 0/0, writes 0/2, held 1",
        ),
        (
            vec![],
            None,
            Some(1),
            Some("--max-writes"),
            vec![],
            "dispatched 0, writes 0/1, held 1",
        ),
        (
            vec![],
            Some(1),
            Some(2),
            None,
            vec![PLACEHOLDER, added],
            "dispatched 1/1, writes 2/2, held 0",
        ),
        (
            yesterdays(),
            None,
            Some(0),
            Some("--max-writes"),
            vec![],
            "dispatched 0, writes 0/0, held 1",
        ),
        (
            yesterdays(),
            None,
            Some(1),
            None,
            vec![edited],
            "dispatched 1, writes 1/1, held 0",
        ),
    ];
    let start = "maintain: start research cgwalters-forge/bot-ops#9 pool=claude";
    for (comments, dispatch, writes, held, record, summary) in cases {
        let limits = Limits {
            dispatch,
            writes,
            ..Limits::default()
        };
        for applies in [false, true] {
            let name = format!("{} comment(s), {limits:?}, apply {applies}", comments.len());
            let (config, forge) = two_days(maintained(9, comments.clone(), 40.0));
            let own = own();
            let mode = if applies {
                Mode::Actions(&own)
            } else {
                Mode::Preview
            };
            let report = block(run(
                &forge,
                &forge,
                &config,
                State::default(),
                (mode, limits),
                forge.replay.now,
            ))
            .unwrap();
            assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
            assert_eq!(report.summary, format!("summary: {summary}"), "{name}");
            let lines = maintain_lines(&report);
            let held_line = held.map(|flag| format!("held by {flag}: {start}"));
            assert_eq!(
                held_line.as_ref().is_some_and(|l| report.lines.contains(l)),
                held.is_some(),
                "{name}: {:#?}",
                report.lines
            );
            let mut expected: Vec<String> = vec![];
            if held.is_none() && applies {
                expected = DIGEST_START.iter().map(|w| w.to_string()).collect();
                expected.extend(record.iter().map(|w| w.to_string()));
            }
            assert_eq!(digest_writes(&forge, 0), expected, "{name}");
            let dispatched = usize::from(held.is_none() && applies);
            assert_eq!(forge.dispatches().len(), dispatched, "{name}");
            // Started or previewed, the plan's record follows: the day ends
            // only where it was recorded.
            assert_eq!(lines.len(), if held.is_some() { 2 } else { 4 }, "{name}");
            if held.is_none() {
                assert!(lines[2].starts_with(start), "{name}: {lines:#?}");
            }
            let done = report.maintain.unwrap().done;
            assert_eq!(done, held.is_none() && applies, "{name}");
        }
    }
}

/// A pass that dispatched the digest, added the placeholder, and failed to
/// edit the plan into it has spent the two writes and ended its day. A pass
/// that lost its state reads no heads from the placeholder, which carries no
/// marker, but today's research plan holds the digest: nothing is written or
/// dispatched again. The next day, still with no plan recorded, the pull
/// requests the first run was given are due again, and a preview of that
/// writes and starts nothing before an applying pass starts them.
#[test]
fn maintain_recovers_from_an_interrupted_record() {
    let (config, mut forge) = two_days(maintained(9, vec![], 40.0));
    let placeholder = format!("{API_ROOT}repos/cgwalters-forge/bot-ops/issues/comments/9000000005");
    *forge.refuse.borrow_mut() = Some(format!("PATCH {placeholder}"));
    let limits = Limits {
        writes: Some(2),
        ..Limits::default()
    };
    let pass = |forge: &Forge, state: State, mode, at| {
        let report = block(run(forge, forge, &config, state, (mode, limits), at)).unwrap();
        let lines: Vec<String> = (maintain_lines(&report).into_iter())
            .map(str::to_owned)
            .collect();
        (report, lines)
    };
    let own = own();
    let today = forge.replay.now;
    let (report, lines) = pass(&forge, State::default(), Mode::Actions(&own), today);
    assert_eq!(report.failures, 1, "{:#?}", report.lines);
    assert_eq!(report.summary, "summary: dispatched 1, writes 2/2, held 0");
    let refused = format!(
        "maintain error: maintain: record the plan on cgwalters-forge/bot-ops#9: \
         connection refused during PATCH {placeholder}"
    );
    assert_eq!(lines[3], refused, "{lines:#?}");
    let mut expected = DIGEST_START.to_vec();
    expected.push(PLACEHOLDER);
    assert_eq!(digest_writes(&forge, 0), expected);
    let comments = forge.comments_on("cgwalters-forge/bot-ops", 9);
    assert!(
        !comments
            .iter()
            .any(|c| c.starts_with(maintain::PLAN_MARKER))
    );
    // The dispatch may have started a run: the day ends all the same.
    let state = report.state.unwrap();
    assert!(state.maintain.as_ref().unwrap().done);
    let written = forge.writes.borrow().len();
    let (report, lines) = pass(&forge, state, Mode::Actions(&own), today);
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    assert_eq!(
        lines,
        [format!(
            "maintain: done for today; the next is due from {NEXT}"
        )]
    );
    // A fresh state finds no plan comment of the pass's, and the plan vetoes.
    let (report, lines) = pass(&forge, State::default(), Mode::Actions(&own), today);
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    assert_eq!(
        lines[1..],
        [
            "maintain: digest due: 2 of 2 incoming pull request(s) changed since no plan; \
             reviews up/a#1, up/a#2",
            "maintain: a research plan already exists today; the digest waits",
        ]
    );
    assert_eq!(report.summary, "summary: dispatched 0, writes 0/2, held 0");
    assert!(!report.maintain.unwrap().done);
    assert_eq!(forge.writes.borrow().len(), written);
    // The next day, with no state and that day's usage, previews the same
    // pull requests' start.
    let tomorrow = today + time::Duration::days(1);
    let mut snapshot = usage(&mut forge.replay);
    let updated = at(text(&snapshot, "/updated_at")) + time::Duration::days(1);
    snapshot["updated_at"] = updated.format(&Rfc3339).unwrap().into();
    set_usage(&mut forge.replay, &snapshot);
    let (report, lines) = pass(&forge, State::default(), Mode::Preview, tomorrow);
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    assert_eq!(
        lines[1..],
        [
            "maintain: digest due: 2 of 2 incoming pull request(s) changed since no plan; \
             reviews up/a#1, up/a#2",
            "maintain: start research cgwalters-forge/bot-ops#9 pool=claude",
            "maintain: record the plan on cgwalters-forge/bot-ops#9; a preview records nothing",
        ]
    );
    assert_eq!(report.summary, "summary: dispatched 1, writes 2/2, held 0");
    assert_eq!(forge.writes.borrow().len(), written);
    assert_eq!(forge.dispatches().len(), 1);
    // And an applying pass starts them, adding a plan comment of its own.
    let (report, lines) = pass(&forge, State::default(), Mode::Actions(&own), tomorrow);
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    assert!(
        lines[3].starts_with("maintain: recorded the plan in "),
        "{lines:#?}"
    );
    assert_eq!(forge.dispatches().len(), 2);
    assert_eq!(report.summary, "summary: dispatched 1, writes 2/2, held 0");
    let record = (forge.comments_on("cgwalters-forge/bot-ops", 9).iter())
        .find_map(|c| plan::fenced_json(c, maintain::PLAN_BLOCK).unwrap())
        .unwrap();
    // None acknowledged; both pending on the new plan, not the first day's.
    assert_eq!(record["heads"], json!({}));
    assert_eq!(
        record["pending"]["heads"],
        json!({"up/a#1": head("aa"), "up/a#2": head("bb")})
    );
    let id = text(&record, "/pending/plan");
    assert!(!id.is_empty() && !id.starts_with("9000000001@"), "{id}");
    assert!(report.maintain.unwrap().done);
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
            let variables = json!({"owner": "up", "name": "digest", "number": 9, "after": null});
            replay.requests.push(json!({
                "request": graphql_key(plan::COMMENTS_QUERY, &variables),
                "answer": {"data": {"repository": {"issue": {"comments": {"nodes": [],
                    "pageInfo": {"hasNextPage": false, "endCursor": null}}}}}},
            }));
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

/// A pre-due pass and a search failure keep the cached day, even though
/// neither produces a new daily decision.
#[test]
fn maintain_keeps_its_day_without_a_new_one() {
    for hour in [9, 16] {
        let (config, mut replay) = maintained(hour, vec![], 40.0);
        if hour == 9 {
            answer(&mut replay, &operator_search("author"))["status"] = 503.into();
        }
        let state = yesterdays_day();
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
            State::default(),
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
            170,
            "budget: 61 requests left, fewer than a start may take (68); 1 more candidate(s) wait",
            0,
        ),
        (
            84,
            "budget: 62 requests left, fewer than a start may take (68); 9 more candidate(s) wait",
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

/// A `promote` section with one fork, `edit`'s keys replacing its own and
/// a key set to `"-"` left out.
fn promote(edit: Value) -> Value {
    let mut section = json!({
        "forks": {"o/fork": {"upstream": "up/project", "base": "main"}},
        "signoff": {"name": "Op Erator", "email": "op@example.com"},
        "policyRepository": "o/policy",
        "policyPath": "upstream-policy",
    });
    let keys = section.as_object_mut().unwrap();
    keys.extend(edit.as_object().unwrap().clone());
    keys.retain(|_, value| *value != json!("-"));
    json!({ "promote": section })
}

#[test]
fn v1_representative_config() {
    // Synthetic deployment shape: public placeholders, no live endpoint or credentials.
    let text = include_str!("../../../../fixtures/pass/config-v1.json");
    let cfg = Config::parse(text).unwrap();
    assert!(cfg.work());
    assert_eq!(cfg.triage, Some(TriageScope::Epics));
    assert_eq!(cfg.triage_pools(), &cfg.plan_pools);
    assert_eq!(cfg.github_board().unwrap().priority_field, "Priority");
    assert_eq!(cfg.epics.as_ref().unwrap().max_applies_per_pass, 1);
    assert_eq!(cfg.overlap.as_ref().unwrap().stale_draft_hours, 4);
    assert_eq!(cfg.overlap.as_ref().unwrap().stale_hours, 24);
    assert_eq!(cfg.pr.merge_queue.as_ref().unwrap().tries, 3);
    assert!(cfg.grant().is_some());
    assert!(!cfg.leases && !cfg.triage_close && !cfg.body_plans);
    for (pointer, value) in [
        ("/wip", json!(101)),
        ("/usage/issue", json!(0)),
        ("/budget/pools/claude", Value::Null),
        ("/pools/claude", json!({"target": 0.95})),
        (
            "/epics",
            json!({"repository": "example/control", "unknown": true}),
        ),
    ] {
        let mut value_config: Value = serde_json::from_str(text).unwrap();
        *value_config.pointer_mut(pointer).unwrap() = value;
        assert!(
            Config::parse(&value_config.to_string()).is_err(),
            "{pointer}"
        );
    }
}

#[test]
fn config_table() {
    let minimal = json!({"operator": "op", "bot": "bot", "dispatchRepo": "o/caller", "dispatchWorkflow": "dispatch.yml"});
    let parsed = Config::parse(&minimal.to_string()).unwrap();
    assert!(!parsed.work() && parsed.triage.is_none() && parsed.follow.is_empty());
    assert!(parsed.pools.0.is_empty() && parsed.dispatch_ref.is_none());
    assert_eq!(parsed.wip, 0);
    assert!(!parsed.reruns && !parsed.leases && !parsed.body_plans && !parsed.triage_close);
    assert!(parsed.starts.is_none() && parsed.holds.is_none() && parsed.direction.is_none());
    assert!(parsed.revisit.is_none() && parsed.sweep.is_none() && parsed.maintain.is_none());
    assert!(parsed.cost.is_none() && parsed.budget.is_none() && parsed.overlap.is_none());
    // Pools keep the file's order, which breaks ties.
    let ordered = Config::parse(
        r#"{"operator": "op", "bot": "bot", "dispatchRepo": "o/caller", "dispatchWorkflow": "dispatch.yml",
            "pools": {"zeta": {}, "alpha": {"target": 0.5, "burst": 10}}, "usage": {"repository": "o/ops", "issue": 1}}"#,
    )
    .unwrap();
    let names: Vec<&str> = ordered.pools.0.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["zeta", "alpha"]);
    assert_eq!(ordered.pools.0[0].1.target, DEFAULT_TARGET);
    // Stage 2 starts reviews on the pools `pr.reviewPools` names.
    let mut reviewed = minimal.clone();
    reviewed["pr"] = json!({"reviewWorkflow": "review.yml", "reviewPools": ["b", "a"]});
    let reviewed = Config::parse(&reviewed.to_string()).unwrap();
    assert_eq!(reviewed.poll(None).policy.review_pools, ["b", "a"]);
    assert!(parsed.poll(None).policy.review_pools.is_empty());
    let usage = json!({"url": "http://b.example/usage", "repository": "o/ops", "issue": 1});
    for (edit, error) in [
        (json!({"extra": 1}), "unknown field `extra`"),
        (json!({"wip": null}), "config.wip is null"),
        (
            json!({"pr": {"fixPools": [null]}}),
            "config.pr.fixPools[0] is null",
        ),
        (json!({"pr": {"other": 1}}), "unknown field `other`"),
        (
            json!({"pr": {"reviewPools": ["a"]}}),
            "pr reviewPools needs pr reviewWorkflow, the workflow it starts",
        ),
        (
            json!({"pr": {"reviewWorkflow": "review.yml", "reviewPools": ["tier-1"]}}),
            "pr pool \"tier-1\" is not a pool name",
        ),
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
            json!({"direction": {"repository": "o/ops", "issue": 0}}),
            "direction issue is not a positive number",
        ),
        (
            json!({"direction": {"repository": "O/Caller", "issue": 3}}),
            "direction repository O/Caller is one the pass starts runs on",
        ),
        (
            json!({"planPools": ["a"]}),
            "planPools names a, which is not in pools",
        ),
        (
            json!({"triage": true}),
            "triage needs triagePools or planPools",
        ),
        (
            json!({"triage": {"scope": "some"}}),
            "triage: unknown variant `some`, expected `epics` or `all`",
        ),
        (
            json!({"triage": {"scopes": "all"}}),
            "triage: unknown field `scopes`",
        ),
        (
            json!({"triage": "all"}),
            "triage is \"all\", not true, false or an object",
        ),
        (
            json!({"triage": {}, "planPools": ["a"], "pools": {"a": {}},
                "usage": {"repository": "o/ops", "issue": 1}}),
            "triage scope epics needs epics",
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
            json!({"triageClose": true}),
            "triageClose needs triageFollowUpDays",
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
            json!({"overlap": {"staleDraftHours": 169}}),
            "overlap staleDraftHours is not 0 to 168",
        ),
        (
            json!({"overlap": {"staleHours": 169}}),
            "overlap staleHours is not 0 to 168",
        ),
        (
            json!({"overlap": {"staleDraftHour": 4}}),
            "unknown field `staleDraftHour`",
        ),
        (json!({"starts": {}}), "starts needs epics"),
        (
            json!({"starts": {}, "epics": {"repository": "o/epics"}}),
            "starts needs epics.repository to be dispatchRepo",
        ),
        (
            json!({"starts": {"breakerRuns": 1}, "epics": {"repository": "O/Caller"}}),
            "starts breakerRuns is not 2 to 10",
        ),
        (
            json!({"starts": {"stallAfterMinutes": 5}, "epics": {"repository": "O/Caller"}}),
            "starts stallAfterMinutes is not 10 to 10080",
        ),
        (json!({"starts": {"stall": 60}}), "unknown field `stall`"),
        (json!({"stall": {}}), "stall needs holds"),
        (
            json!({"stall": {"holdHours": 0}, "holds": {"repository": "o/ops", "issue": 1}}),
            "stall holdHours is not 1 to 168",
        ),
        (
            json!({"stall": {"idleHours": 169}, "holds": {"repository": "o/ops", "issue": 1}}),
            "stall idleHours is not 1 to 168",
        ),
        (
            json!({"stall": {"holdHour": 6}, "holds": {"repository": "o/ops", "issue": 1}}),
            "unknown field `holdHour`",
        ),
        (
            json!({"holds": {"repository": "o/ops", "issue": 1,
                "proposals": {"repository": "o/ops", "label": "act", "repositories": ["o/r"]}}}),
            "holds proposals needs holds needsYou",
        ),
        (
            json!({"holds": {"repository": "o/ops", "issue": 1, "needsYou": true,
                "proposals": {"repository": "o/ops", "label": "a,b", "repositories": ["o/r"]}}}),
            "holds proposals label is not 1 to 50 characters without a comma",
        ),
        (
            json!({"holds": {"repository": "o/ops", "issue": 1, "needsYou": true,
                "proposals": {"repository": "o/ops", "label": "act", "repositories": ["bad"]}}}),
            "holds proposals",
        ),
        (
            json!({"holds": {"repository": "o/ops", "issue": 1, "needsYou": true,
                "proposals": {"repository": "o/ops", "label": "act", "repositories": []}}}),
            "holds proposals",
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
            json!({"pr": {"headOwners": ["cgwalters-forge", "not a login"]}}),
            "pr headOwners names \"not a login\", not a login",
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
        (
            json!({"follow": ["o/r"], "pr": {"fixPools": ["mistral"]},
                "usage": {"url": "http://broker.example/usage", "repository": "o/ops", "issue": 1}}),
            "pr pool mistral is not in pools",
        ),
        (
            json!({"budget": {"weeklyUsd": 1000}}),
            "budget needs usage, the snapshot",
        ),
        (
            json!({"epics": {"repository": "o/tracker", "maxAppliesPerPass": 0}}),
            "epics maxAppliesPerPass is not from 1 to 10",
        ),
        (
            json!({"epics": {"repository": "o/tracker", "maxAppliesPerPass": 11}}),
            "epics maxAppliesPerPass is not from 1 to 10",
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
            "budget source broker needs usage.url",
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
            "budget needs usage, the snapshot",
        ),
        (
            json!({"budget": {"source": "ledger"}}),
            "unknown variant `ledger`",
        ),
        (promote(json!({"draft": true})), "unknown field `draft`"),
        (
            promote(
                json!({"forks": {"o/fork": {"upstream": "up/project", "base": "main",
                "draft": true}}}),
            ),
            "unknown field `draft`",
        ),
        (
            promote(json!({"signoff": {"name": "Op", "email": "op@example.com", "key": "k"}})),
            "unknown field `key`",
        ),
        (
            promote(json!({"signoff": null})),
            "config.promote.signoff is null",
        ),
        (
            promote(json!({"forks": {"fork": {"upstream": "up/project", "base": "main"}}})),
            "promote forks: invalid repository \"fork\": expected OWNER/REPO",
        ),
        (
            promote(json!({"forks": {"o/fork": {"upstream": "up", "base": "main"}}})),
            "promote forks o/fork upstream: invalid repository \"up\": expected OWNER/REPO",
        ),
        (
            promote(
                json!({"forks": {"o/fork": {"upstream": "up/a", "base": "main"},
                "O/Fork": {"upstream": "up/b", "base": "main"}}}),
            ),
            "promote forks names o/fork twice",
        ),
        (
            promote(json!({"forks": {}, "policyRepository": "policy"})),
            "promote policyRepository: invalid repository \"policy\": expected OWNER/REPO",
        ),
        (
            promote(json!({"forks": {"o/fork": {"upstream": "O/Fork", "base": "main"}}})),
            "promote fork o/fork is its own upstream",
        ),
        (
            promote(json!({"forks": {"o/fork": {"upstream": "up/project", "base": " "}}})),
            "promote forks o/fork base is empty",
        ),
        (
            promote(json!({"forks": {"o/fork": {"upstream": "up/project"}}})),
            "missing field `base`",
        ),
        (
            {
                let mut both = promote(json!({}));
                both["follow"] = json!(["O/FORK"]);
                both
            },
            "promote fork o/fork is also in follow; a fork's pull requests are promoted, never merged",
        ),
        (
            promote(json!({"signoff": "-"})),
            "promote signoff is required with forks: the operator's Signed-off-by",
        ),
        (
            promote(json!({"signoff": {"name": "Op", "email": ""}})),
            "promote signoff name and email must not be empty",
        ),
        (
            promote(json!({"signoff": {"name": " ", "email": "op@example.com"}})),
            "promote signoff name and email must not be empty",
        ),
        (
            promote(json!({"policyRepository": "-"})),
            "promote policyRepository is required with forks: where contribution-policy records are read",
        ),
        (
            promote(json!({"policyRepository": "policy"})),
            "promote policyRepository: invalid repository \"policy\": expected OWNER/REPO",
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
    // `triage` true is the scope all, as before scopes; an object's scope
    // is epics unless it says all.
    for (setting, scope) in [
        (json!(false), None),
        (json!(true), Some(TriageScope::All)),
        (json!({"scope": "all"}), Some(TriageScope::All)),
        (json!({}), Some(TriageScope::Epics)),
        (json!({"scope": "epics"}), Some(TriageScope::Epics)),
    ] {
        assert_eq!(
            config(json!({"triage": setting})).triage,
            scope,
            "{setting}"
        );
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
    // Promote is off when absent and with empty `forks`, which need nothing
    // else; with forks it says how many, and that nothing evaluates them.
    for (edit, line) in [
        (json!({}), "promote: off"),
        (json!({"promote": {}}), "promote: off"),
        (json!({"promote": {"forks": {}}}), "promote: off"),
        (
            promote(json!({})),
            "promote: 1 fork(s) configured; evaluation is not built",
        ),
        (
            promote(json!({"policyPath": "-"})),
            "promote: 1 fork(s) configured; evaluation is not built",
        ),
    ] {
        let parsed = config(edit.clone());
        assert_eq!(promote::line(parsed.promote.as_ref()), line, "{edit}");
    }
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

pub(super) fn facts(repo: &str, number: u64, created: &str) -> Facts {
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
            Some((HoldKind::Closed, "closed", false)),
        ),
        (
            "needs-human",
            |f| f.labels = vec!["Needs-Human".into()],
            Some((HoldKind::NeedsHuman, "labelled needs-human", true)),
        ),
        (
            "needs-design",
            |f| f.labels = vec!["needs-design".into()],
            Some((HoldKind::NeedsDesign, "labelled needs-design", false)),
        ),
        (
            "unread labels",
            |f| f.labels_complete = false,
            Some((
                HoldKind::Unreadable,
                "its labels could not all be read",
                false,
            )),
        ),
        (
            "board Blocked",
            |f| f.status = Some("Blocked".into()),
            Some((HoldKind::Blocked, "board Status is Blocked", false)),
        ),
        (
            "dependency",
            |f| f.blocked_by = 2,
            Some((HoldKind::WaitingOn, "2 open dependencies", false)),
        ),
        (
            "bot pull request",
            |f| f.pulls = vec![pull_by("bot")],
            Some((HoldKind::PrOpen, "pull request o/r#9 is open", false)),
        ),
        (
            "bot pull request awaiting approval",
            |f| {
                f.labels = vec![OPERATOR_APPROVAL_LABEL.into()];
                f.pulls = vec![pull_by("bot")];
            },
            Some((
                HoldKind::PrOpen,
                "approve pull request o/r#9 to merge",
                true,
            )),
        ),
        (
            "approval label in another case",
            |f| {
                f.labels = vec!["Needs-Operator-Approval".into()];
                f.pulls = vec![pull_by("bot")];
            },
            Some((
                HoldKind::PrOpen,
                "approve pull request o/r#9 to merge",
                true,
            )),
        ),
        (
            "visitor pull request",
            |f| f.pulls = vec![pull_by("visitor")],
            None,
        ),
        (
            "unread pull requests",
            |f| f.pulls_complete = false,
            Some((
                HoldKind::Unreadable,
                "its pull requests could not all be read",
                false,
            )),
        ),
        (
            "a trusted pull request among those read",
            |f| {
                f.pulls = vec![pull_by("op")];
                f.pulls_complete = false;
            },
            Some((HoldKind::PrOpen, "pull request o/r#9 is open", false)),
        ),
    ] {
        let mut f = base.clone();
        change(&mut f);
        let held = hold(&f, &BTreeMap::new(), &people);
        let held = held
            .as_ref()
            .map(|h| (h.kind, h.reason.as_str(), h.operator));
        assert_eq!(held, expected, "{name}");
    }
    // With labels not all read, `under_way` (which the revisit also uses)
    // does not guess at the approval label.
    let mut f = base.clone();
    f.labels = vec![OPERATOR_APPROVAL_LABEL.into()];
    f.labels_complete = false;
    f.pulls = vec![pull_by("bot")];
    let open = Hold::new(HoldKind::PrOpen, "pull request o/r#9 is open");
    assert_eq!(under_way(&f, &BTreeMap::new(), &people), Some(open));
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
    let checkpoint =
        json!({"reason": "budget", "done": "half", "left": "half", "next": "the rest"});
    let draft = json!({"isDraft": true,
        "body": format!("Closes #8.\n\n```checkpoint\n{checkpoint}\n```\n")});
    followed(replay, ("cgwalters-forge/agentic-job", 9), draft, json!([]));
    settings["follow"] = json!(["cgwalters-forge/agentic-job"]);
    settings["pr"] = json!({"fixPools": ["claude"]});
}

/// The bot's open pull request `number` of `repo` from its fork, the one
/// open pull request there, as stage 2 reads it once `repo` is followed:
/// mergeable, unreviewed, with no checks, committed when the recording was
/// made and with its fields replaced by `change`, and `comments` on it.
fn followed(replay: &mut Replay, (repo, number): (&str, u64), change: Value, comments: Value) {
    let committed = replay.now.format(&Rfc3339).unwrap();
    let mut pull = json!({
        "id": format!("PR_{number}"), "number": number,
        "url": format!("https://github.com/{repo}/pull/{number}"),
        "title": "Do the thing", "state": "OPEN", "isDraft": false, "mergeable": "MERGEABLE",
        "body": "Closes #8.", "headRefOid": "abc", "headRefName": "dispatch/implement/agent-run-1",
        "baseRefName": "main", "isCrossRepository": true,
        "baseRepository": {"nameWithOwner": repo, "isPrivate": false, "defaultBranchRef": {"name": "main"}},
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
        "commits": {"nodes": [{"commit": {"oid": "abc", "committedDate": committed,
            "message": "Do half the thing", "statusCheckRollup": null}}]},
    });
    for (key, value) in change.as_object().unwrap() {
        pull[key] = value.clone();
    }
    let (owner, name) = repo.split_once('/').unwrap();
    let variables = json!({"owner": owner, "name": name, "number": number});
    replay.requests.push(json!({
        "request": graphql_key(poll::PULL_QUERY, &variables),
        "answer": {"data": {"repository": {"pullRequest": pull}}},
    }));
    for (path, body) in [
        ("events?per_page=100".into(), json!([])),
        (
            "pulls?state=open&per_page=100".into(),
            json!([{"number": number, "user": {"login": "cgwalters-bot"}}]),
        ),
        (format!("issues/{number}/comments?per_page=100"), comments),
        ("rules/branches/main".into(), json!([])),
        (
            "branches/main".into(),
            json!({"name": "main", "protected": false}),
        ),
    ] {
        replay.requests.push(json!({
            "request": {"get": format!("{API_ROOT}repos/{repo}/{path}")},
            "answer": {"status": 200, "link": null, "body": body},
        }));
    }
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

/// The recording's open tracker#889, which tracker#468's plan overlaps,
/// made to change `README.md` as well and left the only open pull request
/// there, so that it holds tracker#527 too, which is at P1 so that the
/// `overlap` setting's documentation waiver does not let it start. #468's
/// run failed, so that with `reruns` its rerun is checked for overlap.
fn blocking_889(replay: &mut Replay) {
    only(
        replay,
        &["cgwalters-forge/tracker#468", "cgwalters-forge/tracker#527"],
    );
    for entry in &mut replay.requests {
        let pulls = entry["answer"].pointer_mut("/data/repository/pullRequests/nodes");
        if let Some(nodes) = pulls.and_then(Value::as_array_mut) {
            nodes.retain(|n| n["number"] == 889);
            for node in nodes.iter_mut() {
                let files = node["files"]["nodes"].as_array_mut().unwrap();
                files.push(json!({"path": "README.md"}));
            }
        }
        let items = entry["answer"].pointer_mut("/data/repositoryOwner/projectV2/items/nodes");
        for node in items.and_then(Value::as_array_mut).into_iter().flatten() {
            if node["content"]["number"] == 527 {
                node["priority"] = json!({"name": "P1"});
            }
        }
    }
    let run = json!({"id": 39718949900_u64, "display_title": "implement cgwalters-forge/tracker#468 (claude)",
        "conclusion": "failure", "run_attempt": 1,
        "html_url": "https://github.com/cgwalters-forge/tracker/actions/runs/39718949900"});
    failed(replay, vec![run]);
    text_468(replay, "Work.");
    // Not a draft, which `overlap.staleDraftHours` would waive.
    let variables = json!({"owner": "cgwalters-forge", "name": "tracker", "number": 889});
    let pull = json!({"isDraft": false, "createdAt": "2026-10-20T00:00:00Z",
        "commits": {"nodes": [{"commit": {"committedDate": "2026-10-20T00:00:00Z"}}]}});
    replay.requests.push(json!({
        "request": graphql_key(overlap::DRAFT_QUERY, &variables),
        "answer": {"data": {"repository": {"pullRequest": pull}}},
    }));
}

/// Blockers (agent-board#482): the pull request an overlap holds two items
/// on is moved on once, by stage 2's own step where the pass follows its
/// repository, or is one needs-you card where only a person moves it, in
/// place of a stall alarm per item. Each case's pass applies after one
/// that published the same holds seven hours before, so that #527's hold
/// would stall. Its settings, the open pull request's fields and comments
/// where tracker is followed, else the blocker's own read, the cap on
/// dispatches; then the lines the pass prints (each the start of one), the
/// runs it dispatches, and whether it publishes a card.
#[test]
fn blockers_move_or_are_carded() {
    const REPO: &str = "cgwalters-forge/tracker";
    let now = replay().now;
    let ago = |hours: i64| {
        (now - time::Duration::hours(hours))
            .format(&Rfc3339)
            .unwrap()
    };
    let marker = json!([{"id": 1, "user": {"login": "cgwalters-bot"},
        "body": "<!-- tracker-poll: {\"rounds\":1,\"sha\":\"abc\",\"run\":7} -->"}]);
    let fix_run = format!("{API_ROOT}repos/{REPO}/actions/runs/7");
    let reviews = format!(
        "{API_ROOT}repos/{REPO}/actions/workflows/review.yml/runs?event=workflow_dispatch&created=%3E%3D{}&per_page=100",
        (now - time::Duration::days(1)).date()
    );
    let unread = |state: &str, mergeable: &str, hours: i64| {
        json!({"state": state, "isDraft": false, "mergeable": mergeable, "headRefOid": "abc",
            "createdAt": ago(hours + 1), "reviewDecision": null,
            "commits": {"nodes": [{"commit": {"committedDate": ago(hours)}}]},
            "reviews": {"nodes": []}})
    };
    let follow = json!({"follow": [REPO], "pr": {"fixPools": ["claude"]}});
    let reviewed = json!({"follow": [REPO], "pr": {"fixPools": ["claude"],
        "reviewPools": ["claude"], "reviewWorkflow": "review.yml"}});
    let conflicting = json!({"mergeable": "CONFLICTING"});
    let still = json!({"commits": {"nodes": [{"commit": {"oid": "abc", "committedDate": ago(30),
        "message": "Do the thing", "statusCheckRollup": null}}]}});
    let mut stuck = still.clone();
    stuck["mergeable"] = conflicting["mergeable"].clone();
    let holds = "it holds cgwalters-forge/tracker#468, cgwalters-forge/tracker#527";
    let moves = |how: &str| format!("blocker {REPO}#889: stage 2 moves it ({how}); {holds}");
    let card = |why: &str| {
        format!("blocker {REPO}#889: only a person moves it at abc ({why}); {holds} by overlap")
    };
    let outside =
        "it conflicts with its base, and the pass does not follow cgwalters-forge/tracker";
    // Followed: the open pull request's change and comments, and a read
    // besides; or the blocker read outside `follow`.
    enum Read {
        Followed(Value, Value, Option<(String, Value)>),
        Outside(Value),
    }
    type Case = (
        &'static str,
        Value,
        Read,
        Option<usize>,
        Vec<String>,
        usize,
        bool,
    );
    let cases: Vec<Case> = vec![
        (
            "a conflicting followed blocker gets one fix round for both its items",
            follow.clone(),
            Read::Followed(conflicting.clone(), json!([]), None),
            None,
            vec![
                format!("{REPO}#889 abc fix: conflicts with main -> "),
                moves("fix: conflicts with main"),
                format!("hold {REPO}#468: Waiting: open pull request {REPO}#889 changes "),
                format!(
                    "hold {REPO}#527: Waiting: open pull request {REPO}#889 changes `README.md`."
                ),
            ],
            1,
            false,
        ),
        (
            "the same head again: its round runs, and nothing more starts",
            follow.clone(),
            Read::Followed(
                conflicting.clone(),
                marker.clone(),
                Some((fix_run.clone(), json!({"id": 7, "status": "in_progress"}))),
            ),
            None,
            vec![moves("wait: fix round 1 is running for abc")],
            0,
            false,
        ),
        (
            "but a wait as long as staleHours is not moving it",
            follow.clone(),
            Read::Followed(
                stuck,
                marker.clone(),
                Some((fix_run.clone(), json!({"id": 7, "status": "in_progress"}))),
            ),
            None,
            vec![
                format!(
                    "blocker {REPO}#889: stage 2's step has not moved it in 30 hours (wait: fix round 1 is running for abc); {holds}"
                ),
                format!("stall hold {REPO}#527: "),
            ],
            0,
            false,
        ),
        (
            "with the dispatch cap spent its round waits, and nothing is written",
            follow.clone(),
            Read::Followed(conflicting, json!([]), None),
            Some(0),
            vec![
                format!("{REPO}#889 abc fix: conflicts with main -> held by --max-dispatch"),
                format!(
                    "blocker {REPO}#889: stage 2's step did not run this pass (fix: conflicts with main); {holds}"
                ),
                // Not moved, so its items' holds stall as before.
                format!("stall hold {REPO}#527: "),
            ],
            0,
            false,
        ),
        (
            "an unreviewed followed blocker gets the review job started",
            reviewed,
            Read::Followed(
                still.clone(),
                json!([]),
                Some((reviews, json!({"total_count": 0, "workflow_runs": []}))),
            ),
            None,
            vec![
                format!(
                    "review {REPO}#889 abc: no run of the review job for the head, so it starts -> "
                ),
                moves("review: no run of the review job for the head, so it starts"),
            ],
            1,
            false,
        ),
        (
            "one whose review nothing starts is a card after a day",
            follow.clone(),
            Read::Followed(still, json!([]), None),
            None,
            vec![card(
                "wait: no approving review of the head yet, and no review of it has come",
            )],
            0,
            true,
        ),
        (
            "a blocker outside follow is one card",
            json!({}),
            Read::Outside(unread("OPEN", "CONFLICTING", 30)),
            None,
            vec![card(outside)],
            0,
            true,
        ),
        (
            "but not before it has stood still for staleHours",
            json!({}),
            Read::Outside(unread("OPEN", "CONFLICTING", 5)),
            None,
            vec![format!(
                "blocker {REPO}#889: {outside}; it has stood still for 5 hour(s) of the 24 before a card; {holds}"
            )],
            0,
            false,
        ),
        (
            "nor once it is merged",
            json!({}),
            Read::Outside(unread("MERGED", "UNKNOWN", 30)),
            None,
            vec![
                format!("blocker {REPO}#889: it is merged now; {holds}"),
                format!("stall hold {REPO}#527: "),
            ],
            0,
            false,
        ),
        (
            "staleHours 0 leaves the overlap holds to stall as before",
            json!({"overlap": {"staleHours": 0}}),
            Read::Outside(unread("OPEN", "CONFLICTING", 30)),
            None,
            vec![format!("stall hold {REPO}#527: ")],
            0,
            false,
        ),
    ];
    for (name, edit, read, cap, lines, dispatched, carded) in cases {
        let mut forge = Forge::new(None, false);
        let mut settings = json!({"reruns": true, "overlap": {},
            "holds": {"repository": "cgwalters-forge/bot-ops", "issue": 1, "needsYou": true}});
        with_broker(
            &mut forge.replay,
            &mut settings,
            json!({"body": usage::tests::broker()}),
        );
        for (key, value) in edit.as_object().unwrap() {
            settings[key] = value.clone();
        }
        blocking_889(&mut forge.replay);
        // A card's pull request has the bot's summary, which only a pull
        // request's read finds; a followed one's comments are stage 2's.
        if carded {
            let (owner, name) = REPO.split_once('/').unwrap();
            let variables = json!({"owner": owner, "name": name, "number": 889, "after": null});
            let summary = comment_node(9, "cgwalters-bot", "**Decision needed:** Close #889?");
            let page =
                json!({"nodes": [summary], "pageInfo": {"hasNextPage": false, "endCursor": null}});
            forge.replay.requests.push(json!({
                "request": graphql_key(plan::PULL_REQUEST_COMMENTS_QUERY, &variables),
                "answer": {"data": {"repository": {"pullRequest": {"comments": page}}}},
            }));
        }
        match read {
            Read::Followed(change, comments, besides) => {
                followed(&mut forge.replay, (REPO, 889), change, comments);
                if let Some((url, body)) = besides {
                    let answer = json!({"status": 200, "link": null, "body": body});
                    forge
                        .replay
                        .requests
                        .push(json!({"request": {"get": url}, "answer": answer}));
                }
            }
            Read::Outside(node) => {
                let variables =
                    json!({"owner": "cgwalters-forge", "name": "tracker", "number": 889});
                forge.replay.requests.push(json!({
                    "request": graphql_key(blockers::PULL_QUERY, &variables),
                    "answer": {"data": {"repository": {"pullRequest": node}}},
                }));
            }
        }
        for (url, body) in [
            (format!("{API_ROOT}user"), json!({"login": "cgwalters-bot"})),
            (
                format!("{API_ROOT}repos/cgwalters-forge/bot-ops"),
                json!({"private": true}),
            ),
        ] {
            let answer = json!({"status": 200, "link": null, "body": body});
            forge
                .replay
                .requests
                .push(json!({"request": {"get": url}, "answer": answer}));
        }
        let config = config(settings);
        // The holds as a pass seven hours before published them.
        let before = preview(&config, &forge.replay);
        let mut published = holds::snapshot(&before.holds, ago(7), before.tally.clone());
        published.pass.progress_at = ago(0);
        let body = holds::render(&published).unwrap();
        let comments = &mut answer(&mut forge.replay, "bot-ops/issues/1/comments")["body"];
        let comment = json!({"id": 7, "user": {"login": "cgwalters-bot"}, "body": body});
        comments.as_array_mut().unwrap().push(comment);
        let limits = Limits {
            dispatch: cap,
            ..Limits::default()
        };
        let mode = (Mode::Actions(&own()), limits);
        let report = block(run(&forge, &forge, &config, State::default(), mode, now)).unwrap();
        let said = &report.lines;
        assert_eq!(report.failures, 0, "{name}: {said:#?}");
        for line in &lines {
            assert!(
                said.iter().any(|l| l.starts_with(line.as_str())),
                "{name}: no {line:?} in {said:#?}"
            );
        }
        // One line a blocker, and a stall only where the case says so.
        let blocker = said.iter().filter(|l| l.starts_with("blocker ")).count();
        let off = name.starts_with("staleHours 0");
        assert_eq!(blocker, usize::from(!off), "{name}: {said:#?}");
        let stalls: Vec<&String> = said
            .iter()
            .filter(|l| l.starts_with("stall hold "))
            .collect();
        let stalled = lines.iter().any(|l| l.starts_with("stall hold "));
        assert_eq!(!stalls.is_empty(), stalled, "{name}: {stalls:#?}");
        let writes = forge.writes.borrow();
        let sent = (writes.iter())
            .filter(|(_, url, _)| url.ends_with("/dispatches"))
            .count();
        assert_eq!(sent, dispatched, "{name}: {writes:#?}");
        if cap.is_some() {
            let on_889 = (writes.iter()).filter(|(_, url, _)| url.contains("/issues/889/"));
            assert_eq!(on_889.count(), 0, "{name}: {writes:#?}");
        }
        let snapshot = (writes.iter())
            .map(|(_, _, body)| text(body, "/body"))
            .find(|b| b.starts_with(holds::HOLDS_MARKER))
            .map(|b| holds::parse(b).unwrap())
            .unwrap_or_else(|| panic!("{name}: no holds published: {said:#?}"));
        // Every hold's url is its item's issue address, a pull request's
        // too, as the review app requires of the whole snapshot.
        for h in snapshot["holds"].as_array().unwrap() {
            let issue = (h["item"].as_str())
                .map(|i| format!("https://github.com/{}", i.replace('#', "/issues/")));
            assert_eq!(h["url"].as_str(), issue.as_deref(), "{name}: {h}");
        }
        let pull = format!("{REPO}#889");
        let cards: Vec<Value> = (snapshot["holds"].as_array().unwrap().iter())
            .filter(|h| h["kind"] == "blocking")
            .map(|h| json!([h["item"], h["url"], h["operator"], h["priority"]]))
            .collect();
        let asks: Vec<Value> = (snapshot["needs_you"]["items"].as_array().unwrap().iter())
            .filter(|a| a["kind"] == "blocking")
            .map(|a| {
                json!([
                    a["key"],
                    a["action"]["type"],
                    a["target"],
                    a["priority"],
                    a["ask"]
                ])
            })
            .collect();
        match carded {
            true => {
                let url = format!("https://github.com/{REPO}/issues/889");
                assert_eq!(cards, [json!([pull, url, true, "P0"])], "{name}");
                let target = format!("https://github.com/{REPO}/pull/889");
                assert_eq!(
                    asks,
                    [json!([
                        pull.to_lowercase(),
                        "pull_request",
                        target,
                        "P0",
                        "Close #889?"
                    ])],
                    "{name}"
                );
            }
            false => assert!(cards.is_empty() && asks.is_empty(), "{name}: {cards:?}"),
        }
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

/// [`CLOSE_QUERY`] reads the newest event of each kind on its own, so no
/// number of other events hides a rename or reopen; an answer missing a
/// kind is an error, never no such event.
#[test]
fn close_events_are_each_the_newest_of_their_kind() {
    let issue = ItemRef {
        host: DEFAULT_HOST.into(),
        repository: "o/r".into(),
        number: 1,
    };
    let event = |at: &str| json!({"nodes": [{"createdAt": at}]});
    let node = json!({
        "locks": event("2026-10-01T00:00:00Z"), "unlocks": {"nodes": []},
        "renames": event("2026-10-20T00:00:00Z"), "reopens": event("2026-10-21T00:00:00Z"),
    });
    let read = IssueEvents::read(&issue, &node).unwrap();
    assert_eq!(read.locked, Some(at("2026-10-01T00:00:00Z")));
    assert_eq!(read.unlocked, None);
    assert_eq!(read.renamed, Some(at("2026-10-20T00:00:00Z")));
    assert_eq!(read.reopened, Some(at("2026-10-21T00:00:00Z")));
    for missing in ["locks", "unlocks", "renames", "reopens"] {
        let mut node = node.clone();
        node.as_object_mut().unwrap().remove(missing);
        let error = IssueEvents::read(&issue, &node).err().unwrap();
        assert!(
            format!("{error:#}").contains(&format!("no {missing} events")),
            "{missing}"
        );
    }
    for alias in ["locks:", "unlocks:", "renames:", "reopens:"] {
        assert!(
            CLOSE_QUERY.contains(&format!("{alias} timelineItems(")),
            "{alias}"
        );
    }
    assert_eq!(CLOSE_QUERY.matches("last: 1)").count(), 4);
}

/// An epic's planning request edits the bot's last one in place rather
/// than adding a comment (agent-board#429): tracker#596, given an open
/// sub-issue, is planned in the bot's earlier request comment, whose new
/// change time makes it a new plan; only its dispatch record is added, as
/// the claim protocol needs. Only the bot's own is reused, not one the
/// operator wrote; and one the operator edits after the pass read it is
/// neither overwritten nor planned past, but left to the next pass.
#[test]
fn apply_edits_an_epics_planning_request_in_place() {
    let cases = [
        ("cgwalters-bot", false, Some("PATCH")),
        ("cgwalters", false, Some("POST")),
        ("cgwalters-bot", true, None),
    ];
    for (author, raced, posted) in cases {
        let mut forge = Forge::new(None, false);
        let mut replay = replay();
        only(&mut replay, FIRST);
        unfinished(&mut replay);
        forge.replay = replay;
        let at = (forge.replay.now - time::Duration::minutes(1))
            .format(&Rfc3339)
            .unwrap();
        let old = json!({"schema": JOB_PLAN_SCHEMA, "repo": "cgwalters-forge/tracker", "item": 596,
            "kind": "triage", "runner": TRIAGE_RUNNER, "brief": "Plan the epic."});
        let body = plan_body("The board pass asks a triage run to plan this epic.", &old);
        let id = 8_000_000_001_u64;
        let url =
            format!("https://github.com/cgwalters-forge/tracker/issues/596#issuecomment-{id}");
        forge.added.borrow_mut().insert(
            ("cgwalters-forge/tracker".into(), 596),
            vec![json!({"id": id, "url": url,
                "body": body, "createdAt": at, "lastEditedAt": null,
                "author": {"login": author}, "editor": null})],
        );
        if raced {
            // Every read of it after the first: the operator's edit.
            let reads = Cell::new(0);
            forge.racing = Some(Box::new(move |query, data| {
                let nodes = (query == plan::COMMENTS_QUERY)
                    .then(|| data.pointer_mut("/repository/issue/comments/nodes"))
                    .flatten()
                    .and_then(Value::as_array_mut);
                for node in nodes.into_iter().flatten().filter(|n| n["id"] == id) {
                    reads.set(reads.get() + 1);
                    if reads.get() > 1 {
                        node["body"] = "Plan only the docs.".into();
                        node["lastEditedAt"] = "2026-10-23T21:05:00Z".into();
                        node["editor"] = json!({"login": "cgwalters"});
                    }
                }
            }));
        }
        let report = apply(&forge, &config(json!({})));
        assert_eq!(report.failures, 0, "{:#?}", report.lines);
        if posted.is_none() {
            let held = format!(
                "hold epic cgwalters-forge/tracker#596: its planning request {url} changed since the pass read it; the next pass reads it again"
            );
            assert!(report.lines.contains(&held), "{held}: {:#?}", report.lines);
            let dispatched = forge.dispatches();
            assert!(
                !dispatched.contains(&"cgwalters-forge/tracker#596".to_owned()),
                "{dispatched:?}"
            );
            let writes = forge.writes.borrow();
            assert!(
                !(writes.iter()).any(|(_, url, _)| url.contains("/tracker/issues/596/comments")
                    || url.ends_with(&format!("/comments/{id}"))),
                "{writes:#?}"
            );
            continue;
        }
        let start = "start planning cgwalters-forge/tracker#596 pool=claude: ";
        assert!(
            report.lines.iter().any(|l| l.starts_with(start)),
            "{:#?}",
            report.lines
        );
        assert!(
            forge
                .dispatches()
                .contains(&"cgwalters-forge/tracker#596".to_owned())
        );
        let writes = forge.writes.borrow();
        let on_596: Vec<(&str, &str)> = (writes.iter())
            .filter(|(_, url, body)| {
                url.contains("/tracker/issues/596/comments")
                    || (url.ends_with(&format!("/comments/{id}")))
                    || text(body, "/body").contains("\"item\": 596")
            })
            .map(|(method, url, body)| {
                let first = text(body, "/body").lines().next().unwrap_or_default();
                (
                    method.as_str(),
                    if url.ends_with(&id.to_string()) {
                        "ours"
                    } else {
                        first
                    },
                )
            })
            .collect();
        let plan = "The board pass asks a triage run to plan this epic.";
        let reused = posted == Some("PATCH");
        let first = match reused {
            true => ("PATCH", "ours"),
            false => ("POST", plan),
        };
        assert_eq!(on_596[0], first, "{author}: {on_596:#?}");
        let plans = (on_596.iter())
            .filter(|(m, f)| *m == "POST" && *f == plan)
            .count();
        assert_eq!(plans, usize::from(!reused), "{author}: {on_596:#?}");
        assert!(
            (on_596.iter())
                .any(|(m, f)| *m == "POST" && f.starts_with("<!-- agent-board-dispatch")),
            "{on_596:#?}"
        );
    }
}

/// Page `page` of the bot's issues in tracker in `state`, newest first, as
/// the sweep lists them.
fn sweep_page(replay: &mut Replay, state: &str, page: usize, issues: Value) {
    let url = format!(
        "{API_ROOT}repos/cgwalters-forge/tracker/issues?state={state}&creator=cgwalters-bot&sort=created&direction=desc&per_page=100&page={page}"
    );
    replay
        .requests
        .push(json!({"request": {"get": url}, "answer": {"status": 200, "body": issues}}));
}

/// The bot's issues in tracker, as the sweep lists them: its open ones,
/// then any.
fn sweep_listing(replay: &mut Replay, issues: Value) {
    for state in ["open", "all"] {
        sweep_page(replay, state, 1, issues.clone());
    }
}

/// tracker#9001, an open sweep issue of the bot's on the board, filed on
/// the recording's day, whose body lists `issues`; no comment recorded.
fn sweep_issue(replay: &mut Replay, issues: &[&str]) {
    let url = "https://github.com/cgwalters-forge/tracker/issues/9001";
    let list =
        json!({"schema": "agent-board/sweep/v1", "due": "2026-10-23T00:00:00Z", "issues": issues});
    let body = format!(
        "{}\nListed.\n\n```agent-board-sweep\n{list}\n```\n",
        sweep::SWEEP_MARKER
    );
    sweep_listing(
        replay,
        json!([{"number": 9001, "html_url": url, "state": "open",
            "created_at": "2026-10-23T00:00:00Z", "body": body}]),
    );
    let variables = json!({"owner": "cgwalters-forge", "name": "tracker", "number": 9001});
    replay.requests.push(json!({
        "request": graphql_key(BODY_QUERY, &variables),
        "answer": {"data": {"repository": {"issue": {"url": url, "title": "Weekly sweep",
            "body": body, "createdAt": "2026-10-23T00:00:00Z", "lastEditedAt": null,
            "author": {"login": "cgwalters-bot"}, "editor": null}}}},
    }));
    let variables =
        json!({"owner": "cgwalters-forge", "name": "tracker", "number": 9001, "after": null});
    replay.requests.push(json!({
        "request": graphql_key(plan::COMMENTS_QUERY, &variables),
        "answer": {"data": {"repository": {"issue": {"comments": {"nodes": [],
            "pageInfo": {"hasNextPage": false, "endCursor": null}}}}}},
    }));
    for entry in &mut replay.requests {
        let nodes = entry["answer"]
            .pointer_mut("/data/repositoryOwner/projectV2/items/nodes")
            .and_then(Value::as_array_mut);
        if let Some(nodes) = nodes
            && let Some(node) = nodes.iter().find(|n| {
                n["content"]["number"] == 459
                    && text(&n["content"], "/repository/nameWithOwner") == "cgwalters-forge/tracker"
            })
        {
            let mut node = node.clone();
            node["content"]["number"] = 9001.into();
            nodes.push(node);
        }
    }
}

/// The weekly sweep (agent-board#431) on the recording, a Friday, with
/// tracker#446 in a live epic and tracker#459 and #533 outside every one.
/// Due and not yet filed, a preview says it files one issue listing the
/// two, and an apply files it, with the bot's token only, and an issue of
/// another repository only where it is read as public. Once filed, its one
/// triage run starts with a slot spare and claude under its pace, but not
/// while openai, over its pace, is a triage pool. The bot's reply then
/// attaches tracker#459 to tracker#446's live epic and leaves tracker#533's
/// question and the verdict on tracker#9999, off the board but open, for
/// the operator, leaving out tracker#9998, closed since, and labelling the
/// sweep issue; the next pass applies nothing.
#[test]
fn sweep_files_runs_and_applies_once_a_week() {
    let settings = json!({"triage": {"scope": "epics"}, "triagePools": ["claude"], "wip": 12,
        "sweep": {"weekday": "friday", "hourUtc": 0}});
    let issues = [
        "cgwalters-forge/tracker#446",
        "cgwalters-forge/tracker#459",
        "cgwalters-forge/tracker#533",
    ];
    let board = |forge: &mut Forge| {
        only(&mut forge.replay, &issues);
        silent(&mut forge.replay, &[], &[]);
    };
    let sweeps = |report: &Report| -> Vec<String> {
        (report.lines.iter())
            .filter(|l| l.starts_with("sweep") || l.starts_with("start sweep"))
            .cloned()
            .collect()
    };

    // Due, and filed by an apply.
    let mut forge = Forge::new(None, false);
    board(&mut forge);
    sweep_listing(&mut forge.replay, json!([]));
    let filed_line =
        "sweep: file Weekly sweep: 2 issues outside the epics in cgwalters-forge/tracker";
    let report = preview(&config(settings.clone()), &forge.replay);
    assert_eq!(
        sweeps(&report),
        [format!("{filed_line}; a preview files nothing")]
    );
    let report = apply(&forge, &config(settings.clone()));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    let posted: Vec<_> = (forge.writes.borrow().iter())
        .filter(|(m, u, _)| m == "POST" && u.ends_with("/tracker/issues"))
        .map(|(_, _, body)| body.clone())
        .collect();
    assert_eq!(posted.len(), 1, "{:#?}", report.lines);
    assert_eq!(
        posted[0]["title"],
        "Weekly sweep: 2 issues outside the epics"
    );
    let body = text(&posted[0], "/body");
    assert!(body.starts_with(sweep::SWEEP_MARKER), "{body}");
    assert!(
        body.contains("\"cgwalters-forge/tracker#459\",\n    \"cgwalters-forge/tracker#533\""),
        "{body}"
    );

    // Not with the operator's token: the next pass would not find it.
    let mut operator = Forge::new(None, false);
    operator.login = "cgwalters";
    board(&mut operator);
    sweep_listing(&mut operator.replay, json!([]));
    let report = apply(&operator, &config(settings.clone()));
    assert_eq!(
        sweeps(&report),
        [
            "sweep error: the token is cgwalters's, not cgwalters-bot's; only the bot files and applies a sweep, which the pass would not find again"
        ]
    );
    assert!(
        !(operator.writes.borrow().iter()).any(|(_, u, _)| u.ends_with("/tracker/issues")),
        "{:#?}",
        report.lines
    );

    // An issue of another repository only where it is read as public: not
    // one read as private, nor one whose visibility is not recorded.
    let hidden = "sweep: 1 issue(s) outside the epics are in repositories not read as public, and not listed in cgwalters-forge/tracker";
    for (visibility, expected) in [
        (
            Some(json!({"private": false, "visibility": "public"})),
            vec!["sweep: file Weekly sweep: 3 issues outside the epics in cgwalters-forge/tracker; a preview files nothing".to_owned()],
        ),
        (
            Some(json!({"private": true, "visibility": "private"})),
            vec![hidden.to_owned(), format!("{filed_line}; a preview files nothing")],
        ),
        (
            None,
            vec![hidden.to_owned(), format!("{filed_line}; a preview files nothing")],
        ),
    ] {
        let mut fresh = Forge::new(None, false);
        let mut more = issues.to_vec();
        more.push("cgwalters-forge/agent-board#40");
        only(&mut fresh.replay, &more);
        silent(&mut fresh.replay, &[], &[]);
        sweep_listing(&mut fresh.replay, json!([]));
        if let Some(body) = &visibility {
            let url = format!("{API_ROOT}repos/cgwalters-forge/agent-board");
            fresh
                .replay
                .requests
                .push(json!({"request": {"get": url}, "answer": {"status": 200, "body": body}}));
        }
        let report = preview(&config(settings.clone()), &fresh.replay);
        assert_eq!(sweeps(&report), expected, "{visibility:?}");
    }

    // A closed one filed since this week's due time ends the week, past a
    // full page of the bot's newer issues too; one from the week before
    // does not. More than three full pages of open issues could hide an
    // open one, and file none.
    let url = "https://github.com/cgwalters-forge/tracker/issues/8000";
    let closed = |created: &str| {
        json!([{"number": 8000, "html_url": url, "state": "closed",
            "created_at": created, "body": format!("{}\nListed.", sweep::SWEEP_MARKER)}])
    };
    let full: Value = (1..=100)
        .map(|n| json!({"number": n, "state": "open", "created_at": "2026-10-23T00:00:01Z", "body": "Other."}))
        .collect();
    let this_week =
        format!("sweep: this week's, {url}, is closed; the next is due from 2026-10-30T00:00:00Z");
    let none = json!([]);
    for (open, all, expected) in [
        (
            vec![none.clone()],
            vec![closed("2026-10-23T00:00:00Z")],
            this_week.clone(),
        ),
        (
            vec![none.clone()],
            vec![full.clone(), closed("2026-10-23T00:00:00Z")],
            this_week,
        ),
        (
            vec![none.clone()],
            vec![closed("2026-10-16T09:00:00Z")],
            format!("{filed_line}; a preview files nothing"),
        ),
        (
            vec![full.clone(); 3],
            vec![],
            "sweep error: the bot's open issues in cgwalters-forge/tracker are more than 3 pages; no sweep is filed or started".to_owned(),
        ),
    ] {
        let mut fresh = Forge::new(None, false);
        board(&mut fresh);
        let pages = (open.len(), all.len());
        for (state, bodies) in [("open", open), ("all", all)] {
            for (n, body) in bodies.into_iter().enumerate() {
                sweep_page(&mut fresh.replay, state, n + 1, body);
            }
        }
        let report = preview(&config(settings.clone()), &fresh.replay);
        assert_eq!(sweeps(&report), [expected], "{pages:?}");
    }

    // Filed: started only with every triage pool under its pace.
    let mut forge = Forge::new(None, false);
    board(&mut forge);
    // tracker#9999 and #9998 are not on the board, which holds open issues
    // only: #9999 is open, #9998 closed since it was listed.
    let gone = "cgwalters-forge/tracker#9999";
    let closed_since = "cgwalters-forge/tracker#9998";
    sweep_issue(
        &mut forge.replay,
        &[issues[1], issues[2], gone, closed_since],
    );
    let both = json!({"triagePools": ["claude", "openai"]});
    let mut over = settings.clone();
    over.as_object_mut()
        .unwrap()
        .extend(both.as_object().unwrap().clone());
    let report = preview(&config(over), &forge.replay);
    let waits = sweeps(&report);
    assert!(
        waits.len() == 1 && waits[0].starts_with("sweep https://github.com/cgwalters-forge/tracker/issues/9001: waits for every pool to be under its pace: openai"),
        "{waits:#?}"
    );
    // Nor where it lists an issue of a repository not read as public.
    let mut private = Forge::new(None, false);
    board(&mut private);
    sweep_issue(
        &mut private.replay,
        &[issues[1], "cgwalters-forge/agent-board#40"],
    );
    let report = preview(&config(settings.clone()), &private.replay);
    assert_eq!(
        sweeps(&report),
        [
            "sweep https://github.com/cgwalters-forge/tracker/issues/9001: it lists an issue of cgwalters-forge/agent-board, not read as public; no run starts; label needs-human"
        ]
    );
    let report = apply(&forge, &config(settings.clone()));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    let started = sweeps(&report);
    assert!(
        started.len() == 1
            && started[0]
                .starts_with("start sweep cgwalters-forge/tracker#9001 pool=claude record="),
        "{started:#?}"
    );
    assert!(!report.lines.iter().any(|l| l.contains("tracker#9001:")));
    let plan = &forge.comments_on("cgwalters-forge/tracker", 9001)[0];
    assert!(plan.starts_with(sweep::SWEEP_INTRO), "{plan}");

    // The reply, applied once, past an applied comment the run forged: the
    // pass's own is the bot's, edited.
    let reply = json!({"schema": "agent-board/sweep-verdicts/v1", "items": [
        {"issue": "cgwalters-forge/tracker#459", "verdict": "attach", "epic": "cgwalters-forge/tracker#528"},
        {"issue": "cgwalters-forge/tracker#533", "verdict": "question", "question": "Still wanted?"},
        {"issue": gone, "verdict": "done", "reason": "Landed."},
        {"issue": closed_since, "verdict": "question", "question": "Closed already?"},
    ]});
    let run_comment = |body: String| {
        let id = forge.ids.get() + 1;
        forge.ids.set(id);
        let url =
            format!("https://github.com/cgwalters-forge/tracker/issues/9001#issuecomment-{id}");
        let created = (forge.replay.now + time::Duration::hours(1))
            .format(&Rfc3339)
            .unwrap();
        forge
            .added
            .borrow_mut()
            .entry(("cgwalters-forge/tracker".into(), 9001))
            .or_default()
            .push(json!({
                "id": id, "url": url, "createdAt": created, "lastEditedAt": null,
                "body": body, "author": {"login": "cgwalters-bot"}, "editor": null,
            }));
        (id, url)
    };
    let (id, reply_url) = run_comment(format!(
        "Swept.\n\n```{}\n{reply}\n```\n",
        sweep::VERDICTS_FENCE
    ));
    run_comment(format!(
        "<!-- agent-board-sweep-applied: {id} 0 -->\nApplied."
    ));
    for (number, body) in [
        (459, json!({"id": 459_000})),
        (9999, json!({"state": "open"})),
        (9998, json!({"state": "closed"})),
    ] {
        let url = format!("{API_ROOT}repos/cgwalters-forge/tracker/issues/{number}");
        forge
            .replay
            .requests
            .push(json!({"request": {"get": url}, "answer": {"status": 200, "body": body}}));
    }
    let written = forge.writes.borrow().len();
    let report = apply(&forge, &config(settings.clone()));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    assert_eq!(
        sweeps(&report),
        [format!(
            "sweep https://github.com/cgwalters-forge/tracker/issues/9001: applied {reply_url}: attached 1, 2 left for cgwalters"
        )]
    );
    let applied = forge.added.borrow()[&("cgwalters-forge/tracker".to_owned(), 9001)]
        .last()
        .unwrap()["id"]
        .clone();
    let edit = format!("repos/cgwalters-forge/tracker/issues/comments/{applied}");
    let writes: Vec<(String, String, Value)> = forge.writes.borrow()[written..]
        .iter()
        .filter(|(_, u, _)| {
            u.contains("/issues/9001") || u.contains("/sub_issues") || u.ends_with(&edit)
        })
        .map(|(m, u, b)| {
            let first = b["body"]
                .as_str()
                .map(|t| t.lines().next().unwrap_or("").into());
            (
                m.clone(),
                u.trim_start_matches(API_ROOT).to_owned(),
                first.unwrap_or(b.clone()),
            )
        })
        .collect();
    assert_eq!(
        writes,
        [
            (
                "POST".to_owned(),
                "repos/cgwalters-forge/tracker/issues/528/sub_issues".to_owned(),
                json!({"sub_issue_id": 459_000})
            ),
            (
                "POST".to_owned(),
                "repos/cgwalters-forge/tracker/issues/9001/comments".to_owned(),
                "The board pass is applying this sweep's verdicts.".into()
            ),
            (
                "PATCH".to_owned(),
                edit.clone(),
                format!("<!-- agent-board-sweep-applied: {id} 2 -->").into()
            ),
            (
                "POST".to_owned(),
                "repos/cgwalters-forge/tracker/issues/9001/labels".to_owned(),
                json!({"labels": ["needs-human"]})
            ),
        ]
    );
    let summary = forge.comments_on("cgwalters-forge/tracker", 9001);
    let summary = summary.last().unwrap();
    assert!(
        summary.contains("- Attached cgwalters-forge/tracker#459 to cgwalters-forge/tracker#528"),
        "{summary}"
    );
    assert!(
        summary.contains("- cgwalters-forge/tracker#533: Still wanted?"),
        "{summary}"
    );
    assert!(
        summary
            .contains("- cgwalters-forge/tracker#9999, not on the board: done, to close: Landed."),
        "{summary}"
    );
    assert!(!summary.contains(closed_since), "{summary}");
    // A pass that stopped after its comment: the label again, then nothing.
    let labelled = |forge: &Forge, from: usize| -> usize {
        forge.writes.borrow()[from..]
            .iter()
            .filter(|(_, u, _)| u.contains("/issues/9001") || u.contains("/sub_issues"))
            .count()
    };
    let written = forge.writes.borrow().len();
    let report = apply(&forge, &config(settings.clone()));
    assert_eq!(
        sweeps(&report),
        [
            "sweep https://github.com/cgwalters-forge/tracker/issues/9001: applied, with something left for the operator; label needs-human"
        ]
    );
    assert_eq!(labelled(&forge, written), 1);
    for entry in &mut forge.replay.requests {
        if entry["request"]["get"]
            .as_str()
            .is_some_and(|u| u.contains("creator=cgwalters-bot"))
        {
            entry["answer"]["body"][0]["labels"] = json!([{"name": "needs-human"}]);
        }
    }
    let written = forge.writes.borrow().len();
    let report = apply(&forge, &config(settings));
    let waiting = sweeps(&report);
    assert!(
        waiting.len() == 1
            && waiting[0].contains(
                ": applied, https://github.com/cgwalters-forge/tracker/issues/9001#issuecomment-"
            ),
        "{waiting:#?}"
    );
    assert_eq!(labelled(&forge, written), 0);
}

/// The settings of the sweep tests: tracker#446 in a live epic, tracker#459
/// and #533 outside every one.
fn sweep_settings() -> Value {
    json!({"triage": {"scope": "epics"}, "triagePools": ["claude"], "wip": 12,
        "sweep": {"weekday": "friday", "hourUtc": 0}})
}

/// The sweep issue tracker#9001 listing tracker#459 and #533, its run
/// started by an applying pass.
fn started_sweep(settings: &Value) -> Forge {
    let mut forge = Forge::new(None, false);
    only(
        &mut forge.replay,
        &[
            "cgwalters-forge/tracker#446",
            "cgwalters-forge/tracker#459",
            "cgwalters-forge/tracker#533",
        ],
    );
    silent(&mut forge.replay, &[], &[]);
    sweep_issue(
        &mut forge.replay,
        &["cgwalters-forge/tracker#459", "cgwalters-forge/tracker#533"],
    );
    let issue_459 = format!("{API_ROOT}repos/cgwalters-forge/tracker/issues/459");
    forge.replay.requests.push(
        json!({"request": {"get": issue_459}, "answer": {"status": 200, "body": {"id": 459_000}}}),
    );
    let report = apply(&forge, &config(settings.clone()));
    let start = "start sweep cgwalters-forge/tracker#9001 pool=claude record=";
    assert!(
        report.lines.iter().any(|l| l.starts_with(start)),
        "{:#?}",
        report.lines
    );
    forge
}

/// The sweep run's reply on tracker#9001, the bot's, with `items`.
fn sweep_reply(forge: &Forge, items: Value) -> String {
    let id = forge.ids.get() + 1;
    forge.ids.set(id);
    let url = format!("https://github.com/cgwalters-forge/tracker/issues/9001#issuecomment-{id}");
    let created = (forge.replay.now + time::Duration::hours(1))
        .format(&Rfc3339)
        .unwrap();
    let reply = json!({"schema": "agent-board/sweep-verdicts/v1", "items": items});
    forge
        .added
        .borrow_mut()
        .entry(("cgwalters-forge/tracker".into(), 9001))
        .or_default()
        .push(json!({
            "id": id, "url": url, "createdAt": created, "lastEditedAt": null,
            "body": format!("Swept.\n\n```{}\n{reply}\n```\n", sweep::VERDICTS_FENCE),
            "author": {"login": "cgwalters-bot"}, "editor": null,
        }));
    url
}

/// The pass's sweep lines and its hold of every new run.
fn sweep_lines(report: &Report) -> Vec<&str> {
    (report.lines.iter())
        .map(String::as_str)
        .filter(|l| {
            l.starts_with("sweep") || l.starts_with("hold wip") || l.starts_with("hold budget")
        })
        .collect()
}

/// Applying a sweep's reply, and finishing an apply a pass stopped in,
/// take no run: they go on while every slot is taken or the weekly budget
/// is reached, which only holds new runs. A reply that leaves listed
/// issues out (here, every one) keeps them pending as questions, and the
/// sweep issue open; a label lost after the applied comment is put on by
/// the next pass.
#[test]
fn sweep_finishes_without_capacity() {
    let mut settings = sweep_settings();
    let mut forge = started_sweep(&settings);
    let url = "https://github.com/cgwalters-forge/tracker/issues/9001";
    // Every slot taken: the reply, attaching #459 and silent on #533.
    let reply = sweep_reply(
        &forge,
        json!([{"issue": "cgwalters-forge/tracker#459", "verdict": "attach",
            "epic": "cgwalters-forge/tracker#528"}]),
    );
    in_flight(&mut forge.replay, runs(12));
    let report = apply(&forge, &config(settings.clone()));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    let lines = sweep_lines(&report);
    assert!(
        lines.len() == 2
            && lines[0].starts_with("hold wip: ")
            && lines[1]
                == format!("sweep {url}: applied {reply}: attached 1, 1 left for cgwalters"),
        "{lines:#?}"
    );
    let summary = forge.comments_on("cgwalters-forge/tracker", 9001);
    let summary = summary.last().unwrap();
    assert!(
        summary.contains("- cgwalters-forge/tracker#533: the sweep gave it no verdict"),
        "{summary}"
    );

    // An empty reply on a second sweep, whose label is lost after its
    // comment; the budget reached, the next pass labels it.
    let mut forge = started_sweep(&settings);
    let reply = sweep_reply(&forge, json!([]));
    let labels = format!("POST {API_ROOT}repos/cgwalters-forge/tracker/issues/9001/labels");
    *forge.refuse.borrow_mut() = Some(labels);
    let report = apply(&forge, &config(settings.clone()));
    let lines = sweep_lines(&report);
    assert!(
        lines.len() == 1
            && lines[0].starts_with("sweep error: ")
            && lines[0].contains("connection refused"),
        "{lines:#?}"
    );
    let summary = forge.comments_on("cgwalters-forge/tracker", 9001);
    let summary = summary.last().unwrap();
    for issue in ["459", "533"] {
        let pending = format!("- cgwalters-forge/tracker#{issue}: the sweep gave it no verdict");
        assert!(summary.contains(&pending), "{summary}");
    }
    assert!(summary.contains(&reply), "{summary}");
    with_spend(&mut forge.replay, &mut settings, spend(1000.0));
    // Where an applying pass publishes the usage it read from the broker.
    let ops = format!("{API_ROOT}repos/cgwalters-forge/bot-ops");
    forge.replay.requests.push(json!({"request": {"get": ops},
        "answer": {"status": 200, "body": {"private": true, "visibility": "private"}}}));
    let written = forge.writes.borrow().len();
    let report = apply(&forge, &config(settings.clone()));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    let lines = sweep_lines(&report);
    assert!(
        lines.len() == 2
            && lines[0].starts_with("hold budget: ")
            && lines[1]
                == format!(
                    "sweep {url}: applied, with something left for the operator; label needs-human"
                ),
        "{lines:#?}"
    );
    let writes: Vec<String> = (forge.writes.borrow()[written..].iter())
        .filter(|(_, u, _)| !u.contains("/bot-ops/"))
        .map(|(m, u, _)| format!("{m} {u}"))
        .collect();
    assert_eq!(
        writes,
        [format!(
            "POST {API_ROOT}repos/cgwalters-forge/tracker/issues/9001/labels"
        )]
    );
}

/// A sweep whose plan cannot be used, here edited by a visitor, is stuck:
/// labelled for a person, with no run started and no reply read.
#[test]
fn sweep_with_an_unusable_plan_is_stuck() {
    let settings = sweep_settings();
    let forge = started_sweep(&settings);
    {
        let mut added = forge.added.borrow_mut();
        let plan = &mut added
            .get_mut(&("cgwalters-forge/tracker".to_owned(), 9001))
            .unwrap()[0];
        plan["lastEditedAt"] = "2026-10-23T16:00:00Z".into();
        plan["editor"] = json!({"login": "visitor"});
    }
    sweep_reply(&forge, json!([]));
    let dispatched = forge.dispatches().len();
    let written = forge.writes.borrow().len();
    let report = apply(&forge, &config(settings));
    let lines = sweep_lines(&report);
    assert!(
        lines.len() == 1
            && lines[0].starts_with("sweep https://github.com/cgwalters-forge/tracker/issues/9001: its plan cannot be used: ")
            && lines[0].ends_with("; a person replaces it; label needs-human"),
        "{lines:#?}"
    );
    let sweep_writes: Vec<String> = (forge.writes.borrow()[written..].iter())
        .filter(|(_, u, _)| u.contains("/issues/9001"))
        .map(|(m, u, _)| format!("{m} {u}"))
        .collect();
    assert_eq!(
        sweep_writes,
        [format!(
            "POST {API_ROOT}repos/cgwalters-forge/tracker/issues/9001/labels"
        )]
    );
    let dispatches = forge.dispatches();
    assert!(
        !dispatches[dispatched..]
            .iter()
            .any(|d| d.ends_with("#9001")),
        "{dispatches:#?}"
    );
}

/// A sweep listing an issue of a repository that is no longer a work
/// repository: no run starts for it, and where its reply came, that issue
/// is left for the operator, not attached, though the board still holds it.
#[test]
fn sweep_keeps_to_the_work_repositories() {
    let settings = sweep_settings();
    let mut revoked = settings.clone();
    let config_value: Value = serde_json::from_str(CONFIG).unwrap();
    let work: Vec<Value> = (config_value["workRepositories"].as_array().unwrap().iter())
        .filter(|r| *r != "cgwalters-forge/agent-board")
        .cloned()
        .collect();
    revoked["workRepositories"] = json!(work);
    let url = "https://github.com/cgwalters-forge/tracker/issues/9001";
    let removed = "cgwalters-forge/agent-board#40";
    let listed = ["cgwalters-forge/tracker#459", removed];
    let fresh = |listed: &[&str]| {
        let mut forge = Forge::new(None, false);
        only(
            &mut forge.replay,
            &["cgwalters-forge/tracker#446", listed[0], listed[1]],
        );
        silent(&mut forge.replay, &[], &[]);
        sweep_issue(&mut forge.replay, listed);
        let public = format!("{API_ROOT}repos/cgwalters-forge/agent-board");
        let issue_459 = format!("{API_ROOT}repos/cgwalters-forge/tracker/issues/459");
        forge.replay.requests.extend([
            json!({"request": {"get": public},
                "answer": {"status": 200, "body": {"private": false, "visibility": "public"}}}),
            json!({"request": {"get": issue_459}, "answer": {"status": 200, "body": {"id": 459_000}}}),
        ]);
        forge
    };

    // Removed before its run: none starts.
    let report = preview(&config(revoked.clone()), &fresh(&listed).replay);
    assert_eq!(
        sweep_lines(&report),
        [format!(
            "sweep {url}: it lists an issue of cgwalters-forge/agent-board, not a work repository; no run starts; label needs-human"
        )]
    );

    // Removed after its run started: only the work repository's is attached.
    let forge = fresh(&listed);
    let report = apply(&forge, &config(settings));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    let reply = sweep_reply(
        &forge,
        json!([
            {"issue": listed[0], "verdict": "attach", "epic": "cgwalters-forge/tracker#528"},
            {"issue": removed, "verdict": "attach", "epic": "cgwalters-forge/tracker#528"},
        ]),
    );
    let written = forge.writes.borrow().len();
    let report = apply(&forge, &config(revoked));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    assert_eq!(
        sweep_lines(&report),
        [format!(
            "sweep {url}: applied {reply}: attached 1, 1 left for cgwalters"
        )]
    );
    let attached: Vec<Value> = (forge.writes.borrow()[written..].iter())
        .filter(|(_, u, _)| u.contains("/sub_issues"))
        .map(|(_, _, b)| b.clone())
        .collect();
    assert_eq!(attached, [json!({"sub_issue_id": 459_000})]);
    let summary = forge.comments_on("cgwalters-forge/tracker", 9001);
    let summary = summary.last().unwrap();
    assert!(
        summary.contains(&format!(
            "- {removed}, not in a work repository: belongs to cgwalters-forge/tracker#528"
        )),
        "{summary}"
    );

    // Made private after its run started: its verdict is not copied.
    let mut forge = fresh(&listed);
    let report = apply(&forge, &config(sweep_settings()));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    let reply = sweep_reply(
        &forge,
        json!([
            {"issue": listed[0], "verdict": "attach", "epic": "cgwalters-forge/tracker#528"},
            {"issue": removed, "verdict": "question", "question": "Read while private?"},
        ]),
    );
    for entry in &mut forge.replay.requests {
        if entry["request"]["get"] == format!("{API_ROOT}repos/cgwalters-forge/agent-board") {
            entry["answer"]["body"] = json!({"private": true, "visibility": "private"});
        }
    }
    let report = apply(&forge, &config(sweep_settings()));
    assert_eq!(report.failures, 0, "{:#?}", report.lines);
    assert_eq!(
        sweep_lines(&report),
        [format!(
            "sweep {url}: applied {reply}: attached 1, 1 left for cgwalters"
        )]
    );
    let summary = forge.comments_on("cgwalters-forge/tracker", 9001);
    let summary = summary.last().unwrap();
    let hidden = format!("- {removed}, not read as public: its verdict is left out");
    assert!(summary.contains(&hidden), "{summary}");
    assert!(!summary.contains("Read while private?"), "{summary}");
}

/// A sweep whose comments cannot be read, here more pages than the bound,
/// is stuck: labelled for a person rather than failing every later pass.
#[test]
fn sweep_with_unreadable_comments_is_stuck() {
    let mut forge = Forge::new(None, false);
    only(
        &mut forge.replay,
        &["cgwalters-forge/tracker#446", "cgwalters-forge/tracker#459"],
    );
    silent(&mut forge.replay, &[], &[]);
    sweep_issue(&mut forge.replay, &["cgwalters-forge/tracker#459"]);
    // Every page, the first and each after it, says there is another.
    let page = |after: Value| {
        let variables =
            json!({"owner": "cgwalters-forge", "name": "tracker", "number": 9001, "after": after});
        graphql_key(plan::COMMENTS_QUERY, &variables)
    };
    let more = json!({"data": {"repository": {"issue": {"comments": {"nodes": [],
        "pageInfo": {"hasNextPage": true, "endCursor": "c"}}}}}});
    for entry in &mut forge.replay.requests {
        if entry["request"] == page(Value::Null) {
            entry["answer"] = more.clone();
        }
    }
    forge
        .replay
        .requests
        .push(json!({"request": page(json!("c")), "answer": more}));
    let report = preview(&config(sweep_settings()), &forge.replay);
    assert_eq!(
        sweep_lines(&report),
        [
            "sweep https://github.com/cgwalters-forge/tracker/issues/9001: its comments cannot be read: cgwalters-forge/tracker#9001 has more than 1000 comments; the newest could be unread; a person closes it; label needs-human"
        ]
    );
}

/// `sweep` is checked with the rest of the configuration.
#[test]
fn sweep_config_table() {
    let epics = json!({"scope": "epics"});
    let sweep = |edit: Value| {
        let mut section = json!({"weekday": "friday", "hourUtc": 9});
        section
            .as_object_mut()
            .unwrap()
            .extend(edit.as_object().unwrap().clone());
        section
    };
    for (triage, section, error) in [
        (
            json!(true),
            sweep(json!({})),
            Some("sweep needs triage scope epics"),
        ),
        (epics.clone(), sweep(json!({})), None),
        (
            epics.clone(),
            sweep(json!({"weekday": "fri"})),
            Some("sweep weekday \"fri\" is not a day of the week"),
        ),
        (
            epics.clone(),
            sweep(json!({"hourUtc": 24})),
            Some("sweep hourUtc is not 0 to 23"),
        ),
        (
            epics.clone(),
            sweep(json!({"max": 0})),
            Some("sweep max is not 1 to 100"),
        ),
        (
            epics.clone(),
            sweep(json!({"max": 101})),
            Some("sweep max is not 1 to 100"),
        ),
        (
            epics.clone(),
            sweep(json!({"repository": "cgwalters-forge/elsewhere"})),
            Some("sweep repository cgwalters-forge/elsewhere is not a work repository"),
        ),
        (
            epics.clone(),
            sweep(json!({"every": "week"})),
            Some("unknown field `every`"),
        ),
    ] {
        let mut value: Value = serde_json::from_str(CONFIG).unwrap();
        value["triage"] = triage;
        value["sweep"] = section.clone();
        let parsed = Config::parse(&value.to_string());
        match error {
            None => {
                let parsed = parsed.unwrap();
                let sweep = parsed.sweep.as_ref().unwrap();
                assert_eq!(sweep.max, 40);
                assert_eq!(
                    sweep.repository(&parsed).unwrap(),
                    "cgwalters-forge/tracker"
                );
            }
            Some(error) => {
                let got = format!("{:#}", parsed.unwrap_err());
                assert!(got.contains(error), "{section}: {got}");
            }
        }
    }
}

/// tracker#889 closes tracker#468, at P0, and agentic-job#9 closes
/// tracker#527, at P2: the bot's pull requests from its fork, followed with
/// `pr.reviewPools`, whose head `abc` has no review yet. Repository and
/// number order would take agentic-job#9 first. Each repository's review
/// job runs on `workflow_dispatch` are `runs`; tracker#889 has `comments`.
fn reviews_due(wip: u64, runs: [Vec<Value>; 2], comments: Value) -> (Forge, Config) {
    let mut forge = Forge::new(None, false);
    let replay = &mut forge.replay;
    let mut settings = json!({"wip": wip, "reruns": true,
        "follow": [REVIEWED[0].0.0, REVIEWED[1].0.0],
        "pr": {"fixPools": ["claude"], "reviewWorkflow": "review.yml", "reviewPools": ["claude"]}});
    with_broker(
        replay,
        &mut settings,
        json!({"body": usage::tests::broker()}),
    );
    failed(replay, vec![]);
    for (((repo, number), (closes, priority)), runs) in REVIEWED.into_iter().zip(runs) {
        let closing = json!({"closingIssuesReferences": {"nodes": [{"number": closes,
            "repository": {"nameWithOwner": "cgwalters-forge/tracker"},
            "labels": {"nodes": [], "pageInfo": {"hasNextPage": false}}}],
            "pageInfo": {"hasNextPage": false}}});
        let on = if number == 889 {
            comments.clone()
        } else {
            json!([])
        };
        followed(replay, (repo, number), closing, on);
        for entry in &mut replay.requests {
            let items = entry["answer"].pointer_mut("/data/repositoryOwner/projectV2/items/nodes");
            for node in items.and_then(Value::as_array_mut).into_iter().flatten() {
                if node["content"]["number"] == closes
                    && text(&node["content"], "/repository/nameWithOwner")
                        == "cgwalters-forge/tracker"
                {
                    node["priority"] = json!({"name": priority});
                }
            }
        }
        let url = format!(
            "{API_ROOT}repos/{repo}/actions/workflows/review.yml/runs?event=workflow_dispatch&created=%3E%3D{}&per_page=100",
            (replay.now - time::Duration::days(1)).date()
        );
        let answer = json!({"status": 200, "link": null, "body": {"workflow_runs": runs}});
        replay
            .requests
            .push(json!({"request": {"get": url}, "answer": answer}));
    }
    for (url, body) in [
        (format!("{API_ROOT}user"), json!({"login": "cgwalters-bot"})),
        (
            format!("{API_ROOT}repos/cgwalters-forge/bot-ops"),
            json!({"private": true}),
        ),
    ] {
        let answer = json!({"status": 200, "link": null, "body": body});
        replay
            .requests
            .push(json!({"request": {"get": url}, "answer": answer}));
    }
    (forge, config(settings))
}

/// A pull request, by repository and number.
type Pull = (&'static str, u64);

/// The pull requests of [`reviews_due`], each with the issue it closes and
/// that issue's Priority.
const REVIEWED: [(Pull, (u64, &str)); 2] = [
    (("cgwalters-forge/agentic-job", 9), (527, "P2")),
    (("cgwalters-forge/tracker", 889), (468, "P0")),
];

/// The review job's run on `workflow_dispatch` in `repo` for pull request
/// `number`'s head `head`, created an hour before the recording, as
/// `fields` change it.
fn review_run(replay: &Replay, (repo, number, head): (&str, u64, &str), fields: Value) -> Value {
    let created = (replay.now - time::Duration::hours(1))
        .format(&Rfc3339)
        .unwrap();
    let mut run = json!({"id": 77, "path": ".github/workflows/review.yml",
        "event": "workflow_dispatch", "head_sha": "f".repeat(40), "head_branch": "main",
        "display_title": format!("review {repo}#{number} {head} (claude)"),
        "status": "completed", "conclusion": "failure", "run_attempt": 1,
        "created_at": created, "run_started_at": created, "updated_at": created,
        "repository": {"full_name": repo}});
    for (key, value) in fields.as_object().unwrap() {
        run[key] = value.clone();
    }
    run
}

/// What an applying pass did of stage 2's review steps: their lines, and
/// its review job writes, the dispatches by pull request and the reruns.
fn review_steps(forge: &Forge, report: &Report) -> (Vec<String>, Vec<String>) {
    let lines = (report.lines.iter())
        .filter(|l| {
            l.starts_with("review ")
                || l.contains(" abc wait: ")
                || l.contains(" abc needs-human: ")
        })
        .cloned()
        .collect();
    let writes = (forge.writes.borrow().iter())
        .filter_map(|(_, url, body)| {
            let path = url.trim_start_matches(API_ROOT);
            if path.ends_with("review.yml/dispatches") {
                let repo = path
                    .split('/')
                    .take(3)
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join("/");
                Some(format!("start {repo}#{}", text(body, "/inputs/pr")))
            } else {
                path.ends_with("/rerun-failed-jobs")
                    .then(|| format!("rerun {path}"))
            }
        })
        .collect();
    (lines, writes)
}

/// Stage 2's review starts and reruns with `pr.reviewPools`
/// (agent-board#439), each case one applying pass over [`reviews_due`]:
/// taken by the board's Priority, not repository and number, each admitted
/// within `wip` and `--max-dispatch` before anything is written, a rerun as
/// a start, and a review run still going for a head pushed over since
/// holding its slot. The case's `wip`, `--max-dispatch`, the review runs of
/// tracker and agentic-job, then each review line (its start) and the
/// review job writes in order.
#[test]
fn apply_starts_reviews_by_priority_within_wip() {
    let replay = replay();
    let ran = |fields: Value| review_run(&replay, ("cgwalters-forge/tracker", 889, "abc"), fields);
    let superseded = review_run(
        &replay,
        ("cgwalters-forge/tracker", 889, &"e".repeat(40)),
        json!({"status": "in_progress", "conclusion": null}),
    );
    let start = |repo: &str, done: &str| {
        format!("review {repo} abc: no run of the review job for the head, so it starts -> {done}")
    };
    let (tracker, agentic) = (
        "cgwalters-forge/tracker#889",
        "cgwalters-forge/agentic-job#9",
    );
    type Case = (
        &'static str,
        u64,
        Option<usize>,
        [Vec<Value>; 2],
        Vec<String>,
        Vec<&'static str>,
    );
    let cases: Vec<Case> = vec![
        (
            "with room both start, the P0 issue's pull request first",
            8,
            None,
            [vec![], vec![]],
            vec![
                start(agentic, "started on claude"),
                start(tracker, "started on claude"),
            ],
            vec![
                "start cgwalters-forge/tracker#889",
                "start cgwalters-forge/agentic-job#9",
            ],
        ),
        (
            "--max-dispatch takes the P0 one",
            8,
            Some(1),
            [vec![], vec![]],
            vec![
                start(agentic, "held by --max-dispatch"),
                start(tracker, "started on claude"),
            ],
            vec!["start cgwalters-forge/tracker#889"],
        ),
        (
            "the last slot goes to the P0 one, and its start fills it",
            WIP_BASE + 1,
            None,
            [vec![], vec![]],
            vec![
                start(
                    agentic,
                    &format!("not started: wip: {0} of {0} runs active", WIP_BASE + 1),
                ),
                start(tracker, "started on claude"),
            ],
            vec!["start cgwalters-forge/tracker#889"],
        ),
        (
            "a full wip starts neither",
            WIP_BASE,
            None,
            [vec![], vec![]],
            vec![
                start(
                    agentic,
                    &format!("not started: wip: {0} of {0} runs active", WIP_BASE),
                ),
                start(
                    tracker,
                    &format!("not started: wip: {0} of {0} runs active", WIP_BASE),
                ),
            ],
            vec![],
        ),
        (
            "a review still running for a head pushed over since holds its slot",
            WIP_BASE + 1,
            None,
            [vec![], vec![superseded]],
            vec![
                start(
                    agentic,
                    &format!("not started: wip: {0} of {0} runs active", WIP_BASE + 1),
                ),
                start(
                    tracker,
                    &format!("not started: wip: {0} of {0} runs active", WIP_BASE + 1),
                ),
            ],
            vec![],
        ),
        (
            "a rerun is a dispatch: it takes --max-dispatch from the start after it",
            8,
            Some(1),
            [vec![], vec![ran(json!({}))]],
            vec![
                start(agentic, "held by --max-dispatch"),
                format!(
                    "review rerun {tracker} abc: the review job's run https://github.com/cgwalters-forge/tracker/actions/runs/77 ended (failure) with no review of the head, so its failed jobs run once more -> re-run"
                ),
            ],
            vec!["rerun repos/cgwalters-forge/tracker/actions/runs/77/rerun-failed-jobs"],
        ),
        (
            "and a slot of wip",
            WIP_BASE + 1,
            None,
            [vec![], vec![ran(json!({}))]],
            vec![
                start(
                    agentic,
                    &format!("not started: wip: {0} of {0} runs active", WIP_BASE + 1),
                ),
                format!(
                    "review rerun {tracker} abc: the review job's run https://github.com/cgwalters-forge/tracker/actions/runs/77 ended (failure) with no review of the head, so its failed jobs run once more -> re-run"
                ),
            ],
            vec!["rerun repos/cgwalters-forge/tracker/actions/runs/77/rerun-failed-jobs"],
        ),
    ];
    for (name, wip, cap, runs, lines, writes) in cases {
        let (forge, config) = reviews_due(wip, runs, json!([]));
        let limits = Limits {
            dispatch: cap,
            ..Limits::default()
        };
        let mode = (Mode::Actions(&own()), limits);
        let now = forge.replay.now;
        let report = block(run(&forge, &forge, &config, State::default(), mode, now)).unwrap();
        assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
        let (said, wrote) = review_steps(&forge, &report);
        assert_eq!(said, lines, "{name}: {:#?}", report.lines);
        assert_eq!(wrote, writes, "{name}");
    }
}

/// The runs active before stage 2 in [`reviews_due`]'s recording: the
/// caller's listed runs, the stuck one included.
const WIP_BASE: u64 = 2;

/// A review start is sent once a head, whatever happens to its answer or
/// whichever passes overlap. One whose answer is lost may have started: the
/// pass that sent it says the error; the next one finds its reservation on
/// the pull request and no run listed, so it waits for the run, counted
/// toward `wip`, and once the run should be listed asks a person. A pass
/// that overlaps another in progress starts none, and one whose claim lost
/// to an overlapping pass's sends none.
#[test]
fn apply_sends_a_review_start_once() {
    let tracker = "cgwalters-forge/tracker#889";
    let start = |repo: &str, done: &str| {
        format!("review {repo} abc: no run of the review job for the head, so it starts -> {done}")
    };
    let applied = |forge: &Forge, config: &Config| {
        let mode = (Mode::Actions(&own()), Limits::default());
        let now = forge.replay.now;
        block(run(forge, forge, config, State::default(), mode, now)).unwrap()
    };
    // The answer to tracker#889's start is lost, after it was sent.
    let (forge, config) = reviews_due(8, [vec![], vec![]], json!([]));
    *forge.fail.borrow_mut() = Some("tracker/actions/workflows/review.yml/dispatches".into());
    let report = applied(&forge, &config);
    assert_eq!(report.failures, 1, "{:#?}", report.lines);
    let error = format!("{tracker} error: ");
    assert!(
        report.lines.iter().any(|l| l.starts_with(&error)),
        "{:#?}",
        report.lines
    );
    let (_, wrote) = review_steps(&forge, &report);
    assert_eq!(
        wrote,
        [
            format!("start {tracker}"),
            "start cgwalters-forge/agentic-job#9".into()
        ]
    );
    // Its claim, rewritten as the reservation, as the next pass reads it.
    let reserved: Vec<Value> = (forge.added.borrow()[&("cgwalters-forge/tracker".into(), 889)]
        .iter())
    .map(|c| {
        json!({"id": c["id"], "user": {"login": "cgwalters-bot"},
            "created_at": c["createdAt"], "body": c["body"]})
    })
    .collect();
    assert_eq!(reserved.len(), 1);
    assert!(
        text(&reserved[0], "/body").contains(r#""review_head":"abc""#),
        "{reserved:?}"
    );
    let wait = format!(
        "{tracker} abc wait: the review job's start for the head is reserved, its run not listed yet"
    );
    let person = format!(
        "{tracker} abc needs-human: the review job was started for abc and no run of it for the head is listed; inspect its runs, or review it -> labelled needs-human"
    );
    let full = format!("not started: wip: {0} of {0} runs active", WIP_BASE + 1);
    // The reservation as a pass `minutes` later reads it: made that long
    // before the recording's clock.
    let aged = |minutes: i64| {
        let at = (forge.replay.now - time::Duration::minutes(minutes))
            .format(&Rfc3339)
            .unwrap();
        let mut comment = reserved[0].clone();
        let body = text(&comment, "/body").to_owned();
        let (state, rest) = body
            .strip_prefix("<!-- tracker-poll: ")
            .and_then(|b| b.split_once(" -->"))
            .unwrap();
        let mut state: Value = serde_json::from_str(state).unwrap();
        state["review_at"] = at.clone().into();
        comment["body"] = format!("<!-- tracker-poll: {state} -->{rest}").into();
        comment["created_at"] = at.into();
        json!([comment])
    };
    // The case, its `wip`, minutes after the lost answer, then the review
    // lines and the review job writes.
    for (name, wip, minutes, lines, writes) in [
        (
            "while it may yet be listed, it waits, and nothing is sent again",
            8,
            2,
            vec![
                start("cgwalters-forge/agentic-job#9", "started on claude"),
                wait.clone(),
            ],
            vec!["start cgwalters-forge/agentic-job#9"],
        ),
        (
            "and it holds a slot of wip",
            WIP_BASE + 1,
            2,
            vec![start("cgwalters-forge/agentic-job#9", &full), wait],
            vec![],
        ),
        (
            "then it is a person's",
            8,
            15,
            vec![
                start("cgwalters-forge/agentic-job#9", "started on claude"),
                person,
            ],
            vec!["start cgwalters-forge/agentic-job#9"],
        ),
    ] {
        let (forge, config) = reviews_due(wip, [vec![], vec![]], aged(minutes));
        let report = applied(&forge, &config);
        assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
        let (said, wrote) = review_steps(&forge, &report);
        assert_eq!(said, lines, "{name}: {:#?}", report.lines);
        assert_eq!(wrote, writes, "{name}");
    }
    // Overlapping passes: one in progress already, or one whose claim
    // comes first, posted between this pass's read and its read-back.
    let other = "https://github.com/cgwalters-forge/tracker/actions/runs/4243";
    let claim = json!({"id": 1, "user": {"login": "cgwalters-bot"}, "created_at": "2026-10-23T15:00:00Z",
        "body": "<!-- agent-board-fix-claim: {\"from\":\"review:abc\"} -->\nReview start: claimed."});
    for (name, passes, rival, done) in [
        (
            "another pass in progress takes the slots",
            vec![json!({"id": 4243, "html_url": other})],
            None,
            format!("not started: another pass is in progress, {other}"),
        ),
        (
            "a claim of another pass first wins",
            vec![],
            Some(claim),
            "not started: another pass claimed it in comment 1".into(),
        ),
    ] {
        let (mut forge, config) = reviews_due(8, [vec![], vec![]], json!([]));
        forge.passes = passes;
        forge.rival = rival;
        let report = applied(&forge, &config);
        assert_eq!(report.failures, 0, "{name}: {:#?}", report.lines);
        let (said, wrote) = review_steps(&forge, &report);
        let lines = [
            start("cgwalters-forge/agentic-job#9", &done),
            start(tracker, &done),
        ];
        assert_eq!(said, lines, "{name}: {:#?}", report.lines);
        assert!(wrote.is_empty(), "{name}: {wrote:?}");
    }
}

/// An open pull request of `author`'s, o/r#9, that closes the issue.
fn pull_by(author: &str) -> ClosingPull {
    ClosingPull {
        number: 9,
        repository: "o/r".into(),
        author: Some(author.into()),
        updated: None,
    }
}
