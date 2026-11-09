use super::*;
use crate::epic::{Epic, IssueRef};

const AT: &str = "2026-11-08T12:00:00Z";

fn time(at: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(at, &Rfc3339).ok()
}

fn issue(number: u64) -> ItemRef {
    ItemRef {
        host: DEFAULT_HOST.into(),
        repository: "o/t".into(),
        number,
    }
}

/// The P0 epic `number` with `open` and `closed` sub-issues of `o/t`, each
/// titled and last changed at `changed` past 10:00, and `body`.
fn epic(number: u64, open: &[u64], closed: &[u64], body: &str, changed: u64) -> EpicFacts {
    let sub = |n: &u64| IssueRef {
        repo: "o/t".into(),
        number: *n,
    };
    let detail = |n: u64| Detail {
        title: Some(format!("Issue {n}")),
        updated: time(&format!("2026-11-08T10:{changed:02}:00Z")),
    };
    EpicFacts {
        issue: issue(number),
        labels: vec![EPIC_LABEL.into()],
        labels_complete: true,
        priority: Some(PRIORITY.into()),
        status: None,
        why: None,
        body: body.into(),
        census: Epic {
            issue: sub(&number),
            id: String::new(),
            designated: true,
            open: true,
            sub_issues: open.iter().map(sub).collect(),
            closed: closed.iter().map(sub).collect(),
        },
        priorities: BTreeMap::new(),
        detail: detail(number),
        sub_details: (open.iter().chain(closed))
            .map(|&n| (format!("o/t#{n}"), detail(n)))
            .collect(),
    }
}

fn hold(n: u64, kind: HoldKind, reason: &str, operator: bool) -> Published {
    Published {
        item: Some(format!("o/t#{n}")),
        url: None,
        epic: false,
        kind,
        reason: reason.into(),
        operator,
        priority: Some(PRIORITY.into()),
        question: operator.then(|| "Ship it?".into()),
        since: "2026-11-07T00:00:00Z".into(),
        carried: false,
        watched: false,
        releases: 1,
    }
}

fn step(number: u64, reason: &str) -> poll::Stepped {
    poll::Stepped {
        repository: "o/w".into(),
        number,
        head: "abc".into(),
        action: "fix",
        reason: reason.into(),
        ran: false,
        moved_at: None,
        waiting: poll::WaitReason::Person,
    }
}

fn run(item: u64, id: u64) -> view::Run {
    let started = Some("2026-11-08T11:00:00Z");
    view::Run::new(
        ("o/t", item),
        "implement",
        Some("claude"),
        ("o/runner", id),
        started,
    )
    .unwrap()
}

/// Mixed children: #2 waits on the operator, #3 has a run, #4 is done, #5's
/// pull request fails a check, #6's waits on review, #7 is board Blocked,
/// #8 is on the board with nothing under way, #9 the pass knows nothing
/// of; the body's tasks name #4 and #2 again, #10 apart, and a plain task.
/// Epic #20 shares #8, has an older change and nothing on the operator; a
/// P1 epic is no focus, and one epic could not be read.
struct World {
    read: Read,
    holds: Vec<Published>,
    steps: Vec<poll::Stepped>,
    flights: Vec<view::Run>,
    quiet: BTreeSet<String>,
}

fn world() -> World {
    let body = "## Goal\n\nShip the thing to users. Then more.\n\n- [x] #4\n- [ ] o/t#10 Write docs\n- [x] Plain task\n- [ ] https://github.com/o/t/issues/2\n```\n- [ ] #11 in a fence\n```\n";
    let first = epic(1, &[2, 3, 5, 6, 7, 8, 9], &[4], body, 30);
    let second = epic(20, &[8], &[], "Shared work.", 5);
    let mut other = epic(30, &[31], &[], "Later.", 0);
    other.priority = Some("P1".into());
    let listed: Listing = vec![
        Ok(first),
        Ok(second),
        Ok(other),
        Err((
            issue(40),
            anyhow::anyhow!("not all of its boards could be read"),
        )),
    ];
    World {
        read: Read::new(&listed),
        holds: vec![
            hold(2, HoldKind::NeedsHuman, "labelled needs-human", true),
            hold(2, HoldKind::Stalled, "held for 3 days", true),
            hold(5, HoldKind::PrOpen, "pull request o/w#50 is open", false),
            hold(6, HoldKind::PrOpen, "pull request o/w#60 is open", false),
            hold(7, HoldKind::Blocked, "board Status is Blocked", false),
        ],
        steps: vec![
            step(50, "check build failed, conflicts with main; not approved"),
            step(60, "review requested changes"),
        ],
        flights: vec![run(3, 41)],
        quiet: ["o/t#8".to_owned()].into(),
    }
}

fn sources<'w>(w: &'w World, flights: bool, board: bool) -> Sources<'w> {
    Sources {
        read: Some(&w.read),
        holds: &w.holds,
        flights: flights.then_some(w.flights.as_slice()),
        started: &[],
        steps: &w.steps,
        quiet: board.then_some(&w.quiet),
    }
}

fn blockers(card: &Value) -> Vec<(String, Value)> {
    (card["children"].as_array().unwrap().iter())
        .map(|c| {
            let name = c["item"]
                .as_str()
                .or(c["title"].as_str())
                .unwrap()
                .to_owned();
            (name, c["blocker"]["type"].clone())
        })
        .collect()
}

/// The focus of [`world`] is the synthetic wire fixture review's Focus view
/// reads (`fixtures/epic-focus.json`).
#[test]
fn focus_matches_the_wire_fixture() {
    let w = world();
    let focus = snapshot(AT, &sources(&w, true, true));
    let fixture: Value =
        serde_json::from_str(include_str!("../../../../../fixtures/epic-focus.json")).unwrap();
    assert_eq!(focus, fixture, "{focus:#}");
}

#[test]
fn children_are_typed_deduplicated_and_ordered() {
    let w = world();
    let focus = snapshot(AT, &sources(&w, true, true));
    assert_eq!(
        (
            &focus["complete"],
            &focus["unread_epics"],
            &focus["sources"]["epics"]
        ),
        (&json!(false), &json!(1), &json!("partial"))
    );
    let cards = focus["epics"].as_array().unwrap();
    // The P1 epic is no focus; the operator's epic first.
    let items: Vec<_> = cards.iter().map(|c| c["item"].as_str().unwrap()).collect();
    assert_eq!(items, ["o/t#1", "o/t#20"]);
    let first = &cards[0];
    assert_eq!(first["goal"], "Ship the thing to users.");
    assert_eq!(first["changed_at"], "2026-11-08T10:30:00Z");
    // Seven open and one closed sub-issue, #10 and the plain task; #2 and
    // #4 once each, the fenced #11 not at all.
    assert_eq!(first["progress"], json!({"done": 2, "total": 10}));
    assert_eq!(first["operator_blocked"], 1);
    let typed = |pairs: &[(&str, Value)]| -> Vec<(String, Value)> {
        pairs
            .iter()
            .map(|(n, t)| ((*n).to_owned(), t.clone()))
            .collect()
    };
    assert_eq!(
        blockers(first),
        typed(&[
            ("o/t#2", json!("operator")),
            ("o/t#3", Value::Null),
            ("o/t#5", json!("ci")),
            ("o/t#6", json!("review")),
            ("o/t#7", json!("hold")),
            ("o/t#8", json!("not-started")),
            ("o/t#9", json!("unknown")),
            ("o/t#10", json!("unknown")),
            ("o/t#4", Value::Null),
            ("Plain task", Value::Null),
        ])
    );
    let child = |n: usize| &first["children"][n];
    assert_eq!(
        (&child(0)["source"], &child(0)["blocker"]["reason"]),
        (&json!("both"), &json!("Ship it?"))
    );
    assert_eq!(child(2)["blocker"]["pull"], "o/w#50");
    assert_eq!(
        (&child(7)["source"], &child(7)["title"]),
        (&json!("task"), &json!("Write docs"))
    );
    assert_eq!(
        first["runs"][0]["run"],
        json!({"repository": "o/runner", "id": 41})
    );
    // Overlapping membership: #8 is on both cards, as itself on each.
    assert_eq!(
        blockers(&cards[1]),
        typed(&[("o/t#8", json!("not-started"))])
    );
    assert_eq!(cards[1]["runs"], json!([]));
}

/// Where the runs or the board were not read, nothing is said of a child
/// they would prove moving or not started: it is unknown, and the focus is
/// not complete.
#[test]
fn partial_coverage_is_unknown_not_quiet() {
    let w = world();
    let focus = snapshot(AT, &sources(&w, false, false));
    assert_eq!(
        focus["sources"],
        json!({"epics": "partial", "runs": "unread", "board": "unread"})
    );
    let types: BTreeMap<_, _> = blockers(&focus["epics"][0]).into_iter().collect();
    assert_eq!(types["o/t#3"], "unknown");
    assert_eq!(types["o/t#8"], "unknown");
    assert_eq!(types["o/t#2"], "operator");
    let unlisted = Sources {
        read: None,
        ..sources(&w, true, true)
    };
    let focus = snapshot(AT, &unlisted);
    assert_eq!(
        (
            &focus["sources"]["epics"],
            &focus["epics"],
            &focus["complete"]
        ),
        (&json!("unread"), &json!([]), &json!(false))
    );
}

/// Children and epics past their bounds are counted, and an operator's
/// blocker past the cut still orders the card.
#[test]
fn cards_are_bounded_and_counted() {
    let open: Vec<u64> = (100..100 + MAX_CHILDREN as u64 + 5).collect();
    let mut listed: Listing = (1..=MAX_EPICS as u64 + 2)
        .map(|n| Ok(epic(n, &[], &[], "", 0)))
        .collect();
    listed.push(Ok(epic(99, &open, &[], "", 0)));
    let read = Read::new(&listed);
    let last = *open.last().unwrap();
    let holds = [hold(
        last,
        HoldKind::NeedsHuman,
        "labelled needs-human",
        true,
    )];
    let focus = snapshot(
        AT,
        &Sources {
            read: Some(&read),
            holds: &holds,
            flights: Some(&[]),
            started: &[],
            steps: &[],
            quiet: None,
        },
    );
    assert_eq!(focus["omitted"], 3);
    let card = &focus["epics"][0];
    assert_eq!(
        (&card["item"], &card["operator_blocked"], &card["omitted"]),
        (&json!("o/t#99"), &json!(1), &json!(5))
    );
    assert_eq!(card["children"].as_array().unwrap().len(), MAX_CHILDREN);
    assert_eq!(card["progress"]["total"], MAX_CHILDREN + 5);
}

#[test]
fn goals_and_task_items_are_read_conservatively() {
    for (body, expected) in [
        ("Ship it. Then more.", Some("Ship it.")),
        (
            "# Title\n\n<!-- hidden. -->\nFirst line\nwraps on. Second.",
            Some("First line wraps on."),
        ),
        ("Version 1.2 ships now", Some("Version 1.2 ships now")),
        ("- [ ] #1\n", None),
        ("", None),
    ] {
        assert_eq!(goal(body).as_deref(), expected, "{body}");
    }
    for (text, expected) in [
        ("#12 Docs", Some(("o/t#12", "Docs"))),
        ("x/y#3: fix it", Some(("x/y#3", "fix it"))),
        ("https://github.com/x/y/pull/7.", Some(("x/y#7", ""))),
        ("https://github.com/x/y/blob/7", None),
        ("../y#3 path", None),
        ("#012 leading zero", None),
        ("Plain words", None),
    ] {
        let found = task_item(text, "o/t");
        let found = found.as_ref().map(|(i, r)| (i.as_str(), *r));
        assert_eq!(found, expected, "{text}");
    }
}

/// Published with the holds, its time alone is no change, and a focus too
/// big for the comments goes whole before any hold.
#[test]
fn focus_is_published_with_the_holds() {
    let w = world();
    let at = |at: &str| {
        let mut document = holds::snapshot(&[], at.into(), Default::default());
        document.pass.progress_at = AT.into();
        document.epic_focus = Some(snapshot(at, &sources(&w, true, true)));
        document
    };
    let (fitted, body) = holds::fit(at(AT)).unwrap();
    let published = holds::published(&body).unwrap();
    assert_eq!(Some(&published["epic_focus"]), fitted.epic_focus.as_ref());
    assert!(!holds::changed(Some(&body), &at("2026-11-08T12:05:00Z")).unwrap());
    let mut moved = at("2026-11-08T12:05:00Z");
    moved.epic_focus.as_mut().unwrap()["epics"][0]["operator_blocked"] = 0.into();
    assert!(holds::changed(Some(&body), &moved).unwrap());
    let mut oversized = at(AT);
    oversized.epic_focus = Some(json!({"oversized": "x".repeat(holds::MAX_COMMENT_CHARS)}));
    assert!(holds::fit(oversized).unwrap().0.epic_focus.is_none());
}
