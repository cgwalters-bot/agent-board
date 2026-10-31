//! The weekly sweep on Forgejo, over the epics' fake instance: the sweep
//! issue is filed in example/work, beside the live epic example/plans#3 and
//! its child example/work#7.
use super::*;
use crate::pass::sweep::{SWEEP_MARKER, VERDICTS_FENCE};

/// Another work repository, whose issues may be private.
const OTHER: &str = "example/other";

/// The epics' configuration with triage scope `epics` and a sweep due from
/// Friday 09:00, before the pass's clock (Friday 15:15).
fn sweep_config(wip: u64) -> Config {
    let mut value = file();
    value["board"]["url"] = URL.into();
    value["board"]["repositories"] = json!([WORK, OTHER]);
    value["wip"] = wip.into();
    value["workRepositories"] = json!([WORK, OTHER]);
    value["pools"] = json!({"claude": {}});
    value["planPools"] = json!(["claude"]);
    value["leases"] = true.into();
    value["epics"] = json!({"repository": PLANS});
    value["triage"] = json!({"scope": "epics"});
    value["sweep"] = json!({"weekday": "friday", "hourUtc": 9, "repository": WORK});
    config(&value)
}

impl Instance {
    /// The epics' instance with #8 and #9 outside every epic, and the epic
    /// and every candidate held by their label, so that stage 3 plans and
    /// stage 4 starts nothing.
    fn sweeping() -> Self {
        let instance = Self::new();
        instance.issue(
            PLANS,
            EPIC,
            OPERATOR,
            &["epic", "priority/P1", "needs-human"],
        );
        instance.issue(WORK, 8, OPERATOR, &["agent-board", "needs-human"]);
        instance.issue(WORK, 9, OPERATOR, &["agent-board", "needs-human"]);
        instance.issue(WORK, 7, BOT, &["agent-board", "needs-human"]);
        instance
    }

    /// The bot's open sweep issue `n` in example/work, filed now, with its
    /// list record of `listed` unless None.
    fn swept(&self, n: u64, listed: Option<&[&str]>) -> u64 {
        self.issue(WORK, n, BOT, &[]);
        let mut issues = self.issues.borrow_mut();
        let issue = issues.get_mut(&at(WORK, n)).unwrap();
        issue["body"] = format!("{SWEEP_MARKER}\nThe list.").into();
        issue["created_at"] = AT.into();
        drop(issues);
        if let Some(listed) = listed {
            let list = json!({"schema": "agent-board/sweep/v1", "due": "2026-10-23T09:00:00Z",
                "issues": listed});
            let body = format!(
                "<!-- agent-board-sweep-list: {n} -->\nRecord.\n\n```agent-board-sweep\n{list}\n```\n"
            );
            self.comment(WORK, n, BOT, &body);
        }
        n
    }

    /// The bot's reply on the sweep issue `n` with `items`.
    fn reply(&self, n: u64, items: Value) -> u64 {
        let reply = json!({"schema": "agent-board/sweep-verdicts/v1", "items": items});
        let body = format!("Swept.\n\n```{VERDICTS_FENCE}\n{reply}\n```\n");
        self.comment(WORK, n, BOT, &body)
    }

    /// A started run's records on the sweep issue `n`: its plan, claim and
    /// receipt, its run ended.
    fn dispatched(&self, n: u64) {
        let job = json!({"schema": "agent-board/job-plan/v1", "repo": WORK, "item": n,
            "kind": "triage", "runner": "hosted", "brief": "Sweep.", "pool": "claude"});
        let plan = self.comment(
            WORK,
            n,
            BOT,
            &format!("Sweep.\n\n```agent-board-plan\n{job}\n```\n"),
        );
        let claim = self.comment(
            WORK,
            n,
            BOT,
            &format!("<!-- agent-board-dispatch: {plan}@{AT} -->\nClaimed."),
        );
        self.comment(
            WORK,
            n,
            BOT,
            &format!("<!-- agent-board-receipt: {claim} -->\n{{\"event\":\"started\",\"run\":41}}"),
        );
    }

    fn bodies(&self, repo: &str, n: u64) -> Vec<String> {
        (self.thread(repo, n).iter())
            .map(|c| c["body"].as_str().unwrap().to_owned())
            .collect()
    }
}

fn sweep_lines(report: &Report) -> Vec<&String> {
    (report.lines.iter())
        .filter(|l| l.contains("sweep"))
        .collect()
}

#[test]
fn due_files_once_a_week_with_its_list_record() {
    let instance = Instance::sweeping();
    let config = sweep_config(2);
    let preview = pass(&instance, &config, Mode::Preview);
    assert!(
        has(
            &preview,
            "sweep: file Weekly sweep: 2 issues outside the epics in example/work; a preview files nothing"
        ),
        "{:?}",
        sweep_lines(&preview)
    );
    assert!(instance.writes.borrow().is_empty());
    let report = pass(&instance, &config, Mode::Local);
    assert_eq!(report.failures, 0, "{:?}", report.lines);
    assert_eq!(instance.writes("POST repos/example/work/issues"), 1);
    let issues = instance.issues.borrow();
    let ((_, n), filed) = (issues.iter())
        .find(|(_, i)| (i["body"].as_str().unwrap()).starts_with(SWEEP_MARKER))
        .unwrap();
    let n = *n;
    assert_eq!(filed["title"], "Weekly sweep: 2 issues outside the epics");
    // Nothing is listed in the body as a record; the comment is.
    assert!(
        !filed["body"]
            .as_str()
            .unwrap()
            .contains("agent-board/sweep/v1")
    );
    drop(issues);
    let record = &instance.bodies(WORK, n)[0];
    assert!(record.starts_with(&format!("<!-- agent-board-sweep-list: {n} -->")));
    assert!(record.contains("\"example/work#8\""), "{record}");
    assert!(record.contains("\"example/work#9\""), "{record}");
    assert!(
        record.contains("\"due\": \"2026-10-23T09:00:00Z\""),
        "{record}"
    );
    // Filed since the due time and then closed: not filed again this week.
    for (created, files) in [(AT, false), ("2026-10-23T08:59:59Z", true)] {
        {
            let mut issues = instance.issues.borrow_mut();
            let issue = issues.get_mut(&at(WORK, n)).unwrap();
            issue["state"] = "closed".into();
            issue["created_at"] = created.into();
        }
        let writes = instance.writes("POST repos/example/work/issues");
        let report = pass(&instance, &config, Mode::Preview);
        assert_eq!(
            has(&report, "sweep: file Weekly sweep"),
            files,
            "{created}: {:?}",
            sweep_lines(&report)
        );
        assert_eq!(
            has(
                &report,
                "is closed; the next is due from 2026-10-30T09:00:00Z"
            ),
            !files,
            "{created}"
        );
        assert_eq!(instance.writes("POST repos/example/work/issues"), writes);
    }
}

#[test]
fn incomplete_listing_files_and_starts_nothing() {
    let instance = Instance::sweeping();
    instance.fault.set("sweep-listing");
    let report = pass(&instance, &sweep_config(2), Mode::Local);
    assert!(
        has(
            &report,
            "sweep error: the bot's open issues in example/work are more than 4 pages"
        ),
        "{:?}",
        sweep_lines(&report)
    );
    assert!(!has(&report, "sweep: file"));
    assert_eq!(instance.writes("POST repos/example/work/issues"), 0);
}

#[test]
fn private_sources_are_never_copied() {
    let instance = Instance::sweeping();
    instance.issue(OTHER, 4, OPERATOR, &["agent-board", "needs-human"]);
    instance.private.borrow_mut().insert(OTHER.into());
    let report = pass(&instance, &sweep_config(2), Mode::Local);
    assert!(
        has(
            &report,
            "sweep: 1 issue(s) outside the epics are in repositories not read as public"
        ),
        "{:?}",
        sweep_lines(&report)
    );
    let n = (instance.issues.borrow().iter())
        .find(|(_, i)| (i["body"].as_str().unwrap()).starts_with(SWEEP_MARKER))
        .map(|((_, n), _)| *n)
        .unwrap();
    let record = &instance.bodies(WORK, n)[0];
    assert!(!record.contains(OTHER), "{record}");
    // A listed issue whose repository turned private since: no run starts,
    // and where its run had, its verdict is not copied once applied.
    for started in [false, true] {
        let instance = Instance::sweeping();
        instance.issue(OTHER, 4, OPERATOR, &["agent-board", "needs-human"]);
        let n = instance.swept(30, Some(&["example/work#8", "example/other#4"]));
        instance.private.borrow_mut().insert(OTHER.into());
        if started {
            instance.dispatched(n);
            instance.reply(
                n,
                json!([{"issue": "example/other#4", "verdict": "question", "question": "Secret?"}]),
            );
        }
        let report = pass(&instance, &sweep_config(2), Mode::Local);
        assert_eq!(instance.writes(DISPATCH), 0);
        if !started {
            assert!(
                has(
                    &report,
                    "it lists an issue of example/other, not read as public; no run starts; label needs-human"
                ),
                "{:?}",
                sweep_lines(&report)
            );
            continue;
        }
        let bodies = instance.bodies(WORK, n);
        let receipt = (bodies.iter())
            .find(|b| b.contains("agent-board-sweep-applied"))
            .unwrap_or_else(|| panic!("{:?}", sweep_lines(&report)));
        assert!(
            receipt.contains("example/other#4, not read as public: its verdict is left out"),
            "{receipt}"
        );
        assert!(!receipt.contains("Secret?"), "{receipt}");
    }
}

#[test]
fn edited_or_missing_records_are_stuck() {
    for case in ["edited-list", "no-list", "other-number", "edited-reply"] {
        let instance = Instance::sweeping();
        let n = instance.swept(30, (case != "no-list").then_some(&["example/work#8"][..]));
        match case {
            "edited-list" => {
                let mut comments = instance.comments.borrow_mut();
                let record = &mut comments.get_mut(&at(WORK, n)).unwrap()[0];
                record["updated_at"] = "2026-10-23T15:16:00Z".into();
            }
            "other-number" => {
                let mut comments = instance.comments.borrow_mut();
                let record = &mut comments.get_mut(&at(WORK, n)).unwrap()[0];
                let body = record["body"]
                    .as_str()
                    .unwrap()
                    .replace(": 30 -->", ": 31 -->");
                record["body"] = body.into();
            }
            "edited-reply" => {
                instance.dispatched(n);
                instance.reply(n, json!([{"issue": "example/work#8", "verdict": "done"}]));
                let mut comments = instance.comments.borrow_mut();
                let reply = comments.get_mut(&at(WORK, n)).unwrap().last_mut().unwrap();
                reply["updated_at"] = "2026-10-23T15:16:00Z".into();
            }
            _ => {}
        }
        let report = pass(&instance, &sweep_config(2), Mode::Local);
        let why = match case {
            "edited-list" | "edited-reply" => "edited recorder record",
            "no-list" => "it has no list record",
            _ => "its list record names another issue",
        };
        assert!(
            (report.lines.iter()).any(|l| l.starts_with("sweep ")
                && l.contains(why)
                && l.ends_with("label needs-human")),
            "{case}: {:?}",
            sweep_lines(&report)
        );
        assert!(
            instance
                .label_names(WORK, n)
                .contains(&"needs-human".to_owned()),
            "{case}"
        );
        // Nothing else: no run, no apply, no new sweep while it is open.
        assert_eq!(instance.writes(DISPATCH), 0, "{case}");
        assert_eq!(
            instance.writes("POST repos/example/work/issues"),
            0,
            "{case}"
        );
        let label = format!("POST repos/example/work/issues/{n}/labels");
        assert_eq!(
            instance.writes.borrow().len(),
            instance.writes(&label),
            "{case}"
        );
        // Labelled once: a later pass only says so.
        let again = pass(&instance, &sweep_config(2), Mode::Local);
        assert!(has(&again, why), "{case}");
        assert_eq!(instance.writes(&label), 1, "{case}");
    }
}

#[test]
fn ambiguous_create_is_never_filed_again() {
    let instance = Instance::sweeping();
    instance.fault.set("create-lost");
    let report = pass(&instance, &sweep_config(2), Mode::Local);
    assert!(
        has(
            &report,
            "sweep error: filing the sweep issue; never filed again"
        ),
        "{:?}",
        sweep_lines(&report)
    );
    instance.fault.set("");
    // It was made: found open with no list, stuck, and never made again.
    let n = (instance.issues.borrow().iter())
        .find(|(_, i)| (i["body"].as_str().unwrap()).starts_with(SWEEP_MARKER))
        .map(|((_, n), _)| *n)
        .unwrap();
    instance.issues.borrow_mut().get_mut(&at(WORK, n)).unwrap()["created_at"] = AT.into();
    for _ in 0..2 {
        let report = pass(&instance, &sweep_config(2), Mode::Local);
        assert!(
            has(&report, "it has no list record"),
            "{:?}",
            sweep_lines(&report)
        );
    }
    assert_eq!(instance.writes("POST repos/example/work/issues"), 1);
}

#[test]
fn run_starts_with_spare_capacity_through_the_claim_path() {
    // No slot: the run waits, and nothing is written.
    let instance = Instance::sweeping();
    let n = instance.swept(30, Some(&["example/work#8"]));
    instance
        .runs
        .borrow_mut()
        .push(caller_run(90, "implement example/work#99 (claude)"));
    let report = pass(&instance, &sweep_config(1), Mode::Local);
    assert!(
        has(
            &report,
            &format!("sweep {URL}/example/work/issues/{n}: no slot free; it waits")
        ),
        "{:?}",
        sweep_lines(&report)
    );
    assert!(instance.writes.borrow().is_empty());
    // A slot: started once, as a triage run of the sweep issue.
    let instance = Instance::sweeping();
    let n = instance.swept(30, Some(&["example/work#8"]));
    let config = sweep_config(2);
    let preview = pass(&instance, &config, Mode::Preview);
    assert!(has(
        &preview,
        &format!("start sweep example/work#{n} pool=claude")
    ));
    assert!(instance.writes.borrow().is_empty());
    let report = pass(&instance, &config, Mode::Local);
    assert_eq!(report.failures, 0, "{:?}", report.lines);
    assert_eq!(instance.writes(DISPATCH), 1);
    assert_eq!(
        instance.runs.borrow()[0]["title"],
        format!("triage example/work#{n} (claude)")
    );
    let bodies = instance.bodies(WORK, n);
    assert!(bodies.iter().any(|b| b.contains("\"kind\": \"triage\"")
        && b.contains("attach (it is part of a live epic")
        && b.contains("example/plans#3")));
    // In flight: it waits; nothing is posted or started again.
    let report = pass(&instance, &config, Mode::Local);
    assert!(
        has(&report, "its run is in flight"),
        "{:?}",
        sweep_lines(&report)
    );
    assert_eq!(instance.writes(DISPATCH), 1);
    assert_eq!(instance.bodies(WORK, n).len(), bodies.len());
    // Ended without a reply: a person runs it again.
    instance.runs.borrow_mut()[0]["status"] = "success".into();
    let report = pass(&instance, &config, Mode::Local);
    assert!(
        has(&report, "its run gave no verdicts"),
        "{:?}",
        sweep_lines(&report)
    );
    assert_eq!(instance.writes(DISPATCH), 1);
}

#[test]
fn reply_attaches_to_a_live_epic_once_and_leaves_the_rest() {
    let instance = Instance::sweeping();
    instance.issue(WORK, 10, OPERATOR, &["agent-board", "needs-human"]);
    instance.issue(WORK, 11, OPERATOR, &["agent-board", "needs-human"]);
    // #9 names another epic already.
    let other = crate::epic::IssueRef::parse("example/plans#4").unwrap();
    instance.comment(WORK, 9, BOT, &crate::forgejo::epics::parent_comment(&other));
    let n = instance.swept(
        30,
        Some(&[
            "example/work#8",
            "example/work#9",
            "example/work#10",
            "example/work#11",
        ]),
    );
    instance.dispatched(n);
    instance.reply(
        n,
        json!([
            {"issue": "example/work#8", "verdict": "attach", "epic": "example/plans#3"},
            {"issue": "example/work#9", "verdict": "attach", "epic": "example/plans#3"},
            {"issue": "example/work#10", "verdict": "attach", "epic": "example/plans#5"},
            {"issue": "example/work#11", "verdict": "done", "reason": "Landed."}
        ]),
    );
    let config = sweep_config(2);
    let preview = pass(&instance, &config, Mode::Preview);
    assert!(
        has(
            &preview,
            "sweep: attach example/work#8 to example/plans#3; a preview writes nothing"
        ),
        "{:?}",
        sweep_lines(&preview)
    );
    assert!(instance.writes.borrow().is_empty());
    let report = pass(&instance, &config, Mode::Local);
    assert_eq!(report.failures, 0, "{:?}", report.lines);
    assert!(
        has(&report, "attached 1, 3 left for operator"),
        "{:?}",
        sweep_lines(&report)
    );
    // Both sides, never a sub-issue API.
    let names: Vec<String> = instance.listed().iter().map(ToString::to_string).collect();
    assert_eq!(names, ["example/work#7", "example/work#8"]);
    assert!(
        instance.bodies(WORK, 8)[0]
            .starts_with("<!-- agent-board-epic-parent: example/plans#3 -->")
    );
    assert!(
        !instance
            .writes
            .borrow()
            .iter()
            .any(|w| w.contains("sub_issues"))
    );
    let bodies = instance.bodies(WORK, n);
    let receipt = bodies
        .iter()
        .find(|b| b.contains("agent-board-sweep-applied"))
        .unwrap();
    assert!(
        receipt.starts_with("<!-- agent-board-receipt: "),
        "{receipt}"
    );
    assert!(receipt.contains("example/work#9: belongs to example/plans#3, but is a sub-issue of example/plans#4 already"), "{receipt}");
    assert!(receipt.contains("which is not a live epic"), "{receipt}");
    assert!(
        receipt.contains("example/work#11: done, to close: Landed."),
        "{receipt}"
    );
    // Something left: labelled, and no listed issue is closed.
    assert!(
        instance
            .label_names(WORK, n)
            .contains(&"needs-human".to_owned())
    );
    assert_eq!(instance.state(WORK, n), "open");
    assert_eq!(instance.state(WORK, 11), "open");
    // Applied once: later passes write nothing.
    let writes = instance.writes.borrow().len();
    for _ in 0..2 {
        let again = pass(&instance, &config, Mode::Local);
        assert!(
            has(&again, "applied; it waits for operator to close it"),
            "{:?}",
            sweep_lines(&again)
        );
    }
    assert_eq!(instance.writes.borrow().len(), writes);
}

#[test]
fn attachments_keep_the_epics_children_readable() {
    use crate::epic::IssueRef;
    use crate::forgejo::epics as relations;

    for (case, initial, listed, parented) in [
        ("attaches share capacity", 48, false, [false; 3]),
        ("completions share capacity", 48, false, [true; 3]),
        (
            "attaches and completions share capacity",
            48,
            false,
            [false, true, false],
        ),
        ("listed child is not duplicated", 2, true, [false; 3]),
        ("listed child needs no capacity", 50, true, [false; 3]),
    ] {
        let instance = Instance::sweeping();
        let epic = IssueRef::parse("example/plans#3").unwrap();
        let mut children = instance.listed();
        if listed {
            children.push(IssueRef::parse("example/work#8").unwrap());
        }
        while children.len() < initial {
            let number = 100 + children.len() as u64;
            instance.issue(WORK, number, OPERATOR, &["agent-board", "needs-human"]);
            children.push(IssueRef::parse(&format!("{WORK}#{number}")).unwrap());
        }
        instance.comment(
            PLANS,
            EPIC,
            BOT,
            &relations::children_comment(EPIC, &children),
        );
        instance.issue(WORK, 10, OPERATOR, &["agent-board", "needs-human"]);
        let candidates = if listed { vec![8] } else { vec![8, 9, 10] };
        let names: Vec<String> = candidates.iter().map(|n| format!("{WORK}#{n}")).collect();
        for (n, parented) in candidates.iter().zip(parented) {
            if parented {
                instance.comment(WORK, *n, BOT, &relations::parent_comment(&epic));
            }
        }
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let n = instance.swept(30, Some(&refs));
        instance.dispatched(n);
        instance.reply(
            n,
            names
                .iter()
                .map(|name| {
                    json!({
                        "issue": name, "verdict": "attach", "epic": "example/plans#3"
                    })
                })
                .collect(),
        );

        let report = pass(&instance, &sweep_config(2), Mode::Local);
        assert_eq!(report.failures, 0, "{case}: {:?}", report.lines);
        let expected = if listed {
            initial
        } else {
            relations::CHILD_LIMIT
        };
        assert_eq!(instance.listed().len(), expected, "{case}");
        // Every appended list, not just the last, must remain readable by stage 3.
        for comment in instance.thread(PLANS, EPIC) {
            relations::children(&[comment], BOT, EPIC).unwrap();
        }
        for number in candidates.iter().take(if listed { 1 } else { 2 }) {
            assert_eq!(
                relations::parent(&instance.thread(WORK, *number), BOT).unwrap(),
                Some(epic.clone()),
                "{case}"
            );
        }
        if !listed {
            assert!(
                has(&report, "attached 2, 1 left"),
                "{case}: {:?}",
                report.lines
            );
            let bodies = instance.bodies(WORK, n);
            assert!(
                bodies
                    .iter()
                    .any(|b| b.contains("which lists 50 children already")),
                "{case}"
            );
        }
    }
}

#[test]
fn nothing_left_closes_the_sweep_issue() {
    let instance = Instance::sweeping();
    let n = instance.swept(30, Some(&["example/work#8"]));
    instance.dispatched(n);
    instance.reply(
        n,
        json!([{"issue": "example/work#8", "verdict": "attach", "epic": "example/plans#3"}]),
    );
    let report = pass(&instance, &sweep_config(2), Mode::Local);
    assert_eq!(report.failures, 0, "{:?}", report.lines);
    assert_eq!(instance.state(WORK, n), "closed");
    assert!(
        !instance
            .label_names(WORK, n)
            .contains(&"needs-human".to_owned())
    );
}

#[test]
fn cut_short_apply_completes_half_made_relationships_only() {
    let instance = Instance::sweeping();
    instance.issue(WORK, 10, OPERATOR, &["agent-board", "needs-human"]);
    let n = instance.swept(30, Some(&["example/work#8", "example/work#10"]));
    instance.dispatched(n);
    let reply = instance.reply(
        n,
        json!([
            {"issue": "example/work#8", "verdict": "attach", "epic": "example/plans#3"},
            {"issue": "example/work#10", "verdict": "attach", "epic": "example/plans#3"}
        ]),
    );
    // A pass claimed the reply and wrote #8's side, then stopped.
    instance.comment(
        WORK,
        n,
        BOT,
        &format!("<!-- agent-board-sweep-apply: {reply}@{AT} -->\nClaimed this plan; its outcome follows in a later comment."),
    );
    let epic = crate::epic::IssueRef::parse("example/plans#3").unwrap();
    instance.comment(WORK, 8, BOT, &crate::forgejo::epics::parent_comment(&epic));
    let config = sweep_config(2);
    let report = pass(&instance, &config, Mode::Local);
    assert_eq!(report.failures, 0, "{:?}", report.lines);
    assert!(has(&report, "recovered"), "{:?}", sweep_lines(&report));
    // The epic's side added; #8's not again; #10 never attached.
    let names: Vec<String> = instance.listed().iter().map(ToString::to_string).collect();
    assert_eq!(names, ["example/work#7", "example/work#8"]);
    assert_eq!(instance.bodies(WORK, 8).len(), 1);
    assert!(instance.bodies(WORK, 10).is_empty());
    assert_eq!(
        instance.writes("POST repos/example/work/issues/8/comments"),
        0
    );
    assert_eq!(
        instance.writes("POST repos/example/work/issues/10/comments"),
        0
    );
    let bodies = instance.bodies(WORK, n);
    let receipt = bodies
        .iter()
        .find(|b| b.contains("agent-board-sweep-applied"))
        .unwrap();
    assert!(receipt.contains("\"outcome\": \"recovered\""), "{receipt}");
    assert!(receipt.contains("never sent again"), "{receipt}");
    // No second claim; a later pass only waits.
    let claims = bodies
        .iter()
        .filter(|b| b.starts_with("<!-- agent-board-sweep-apply:"))
        .count();
    assert_eq!(claims, 1);
    let writes = instance.writes.borrow().len();
    let again = pass(&instance, &config, Mode::Local);
    assert!(
        has(&again, "applied; it waits for operator"),
        "{:?}",
        sweep_lines(&again)
    );
    assert_eq!(instance.writes.borrow().len(), writes);
}

#[test]
fn caps_hold_an_apply_whole() {
    let instance = Instance::sweeping();
    let n = instance.swept(30, Some(&["example/work#8"]));
    instance.dispatched(n);
    instance.reply(
        n,
        json!([{"issue": "example/work#8", "verdict": "attach", "epic": "example/plans#3"}]),
    );
    let limits = Limits {
        writes: Some(3),
        ..Limits::default()
    };
    let report = run(
        &instance,
        &BrokerFake::default(),
        &sweep_config(2),
        (Mode::Local, limits),
        usage::tests::now(),
    )
    .unwrap();
    assert!(
        has(&report, "held by --max-writes"),
        "{:?}",
        sweep_lines(&report)
    );
    assert!(instance.writes.borrow().is_empty());
}
