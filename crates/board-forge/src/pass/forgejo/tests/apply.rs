use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};

struct Applying {
    reads: Fake,
    comments: RefCell<BTreeMap<u64, Vec<Value>>>,
    slots: RefCell<Vec<(u64, Value)>>,
    runs: RefCell<Vec<Value>>,
    jobs: Cell<u64>,
    writes: RefCell<Vec<String>>,
    fault: &'static str,
    boundary: &'static str,
    change: &'static str,
    changed: Cell<bool>,
}

impl Applying {
    fn new() -> Self {
        let mut fake = Fake::default();
        fake.listed.issues = vec![issue(7, OPERATOR, &["agent-board"])];
        let this = Self {
            reads: fake,
            comments: RefCell::default(),
            slots: RefCell::new(vec![(0, json!({"free":"initial"})); 2]),
            runs: RefCell::default(),
            jobs: Cell::new(1),
            writes: RefCell::default(),
            fault: "",
            boundary: "",
            change: "",
            changed: Cell::new(false),
        };
        this.add(
            7,
            400,
            OPERATOR,
            &format!(
                "```agent-board-plan\n{}\n```",
                json!({
            "schema": JOB_PLAN_SCHEMA, "repo":"example/work", "item":7,
            "kind":"implement", "runner":"hosted", "brief":"Implement the task.", "pool":"claude"})
            ),
        );
        this
    }

    fn add(&self, item: u64, id: u64, user: u64, body: &str) {
        let at = "2026-10-23T15:15:10Z";
        self.comments
            .borrow_mut()
            .entry(item)
            .or_default()
            .push(json!({
            "id":id, "user":{"id":user}, "body":body, "created_at":at, "updated_at":at}));
    }

    fn posts(&self, suffix: &str) -> usize {
        self.writes
            .borrow()
            .iter()
            .filter(|p| p.ends_with(suffix))
            .count()
    }

    fn check_url(&self, url: &url::Url) {
        assert!(
            url.as_str().starts_with(&format!("{URL}/api/v1/")),
            "foreign request: {url}"
        );
    }

    fn change_after(&self, boundary: &str) {
        if self.boundary != boundary || self.changed.replace(true) {
            return;
        }
        match self.change {
            "wip" => {
                for id in [80, 81] {
                    self.runs
                        .borrow_mut()
                        .push(caller_run(id, "research example/work#8 (claude)"));
                }
            }
            "owner" => self.slots.borrow_mut()[0].1["claim"] = "replacement-owner".into(),
            "new-plan" => {
                let body = self.comments.borrow()[&7][0]["body"]
                    .as_str()
                    .unwrap()
                    .replace("Implement the task.", "A different task.");
                self.add(7, 900, OPERATOR, &body);
            }
            _ => {}
        }
    }
}

impl ForgejoTransport for Applying {
    fn get(&self, url: &url::Url) -> Result<Response> {
        self.check_url(url);
        let path = url.path().strip_prefix("/subpath/api/v1/").unwrap();
        let segments: Vec<_> = path.split('/').collect();
        let body = match segments[..] {
            ["repos", "example", "work", "issues", n] => {
                let n: u64 = n.parse()?;
                let mut issue = self
                    .reads
                    .listed
                    .issues
                    .iter()
                    .find(|i| i["number"] == n)
                    .unwrap()
                    .clone();
                issue["comments"] = self.comments.borrow().get(&n).map_or(0, Vec::len).into();
                if self.changed.get() {
                    match self.change {
                        "membership" => issue["labels"] = json!([]),
                        "question" | "epic" | "needs-human" => issue["labels"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!({"name":self.change})),
                        "closed" => issue["state"] = "closed".into(),
                        "identity" => issue["number"] = 8.into(),
                        _ => {}
                    }
                }
                issue
            }
            ["repos", "example", "work", "issues", n, "comments"] => {
                let n: u64 = n.parse()?;
                json!(self.comments.borrow().get(&n).cloned().unwrap_or_default())
            }
            ["repos", "example", "work", "issues", "comments", id] => {
                let id: u64 = id.parse()?;
                self.comments
                    .borrow()
                    .values()
                    .flatten()
                    .find(|c| c["id"] == id)
                    .unwrap()
                    .clone()
            }
            ["repos", "example", "control"] => json!({"private":true}),
            ["repos", "example", "control", "contents", "leases", file] => {
                assert!(
                    url.query_pairs()
                        .any(|(k, v)| k == "ref" && v == LEASE_BRANCH)
                );
                let n: usize = file.trim_end_matches(".json").parse()?;
                let slots = self.slots.borrow();
                let (version, body) = &slots[n];
                json!({"type":"file", "sha":format!("{version:040x}"), "encoding":"base64", "content":STANDARD.encode(body.to_string())})
            }
            ["repos", "example", "control", "actions", "runs"] => {
                let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
                let workflow = &query["workflow_id"];
                let found: Vec<_> = self
                    .runs
                    .borrow()
                    .iter()
                    .filter(|r| {
                        r["workflow_id"] == workflow.as_str()
                            && query
                                .get("status")
                                .is_none_or(|s| r["status"] == s.as_str())
                    })
                    .cloned()
                    .collect();
                json!({"total_count":found.len(), "workflow_runs":page(url, &found)})
            }
            ["repos", "example", "control", "actions", "runs", id] => {
                let id: u64 = id.parse()?;
                self.runs
                    .borrow()
                    .iter()
                    .find(|r| r["id"] == id)
                    .cloned()
                    .context("run missing")?
            }
            ["repos", "example", "control", "actions", "runs", id, "jobs"] => {
                json!([{"id":1,"run_id":id.parse::<u64>()?,"attempt":self.jobs.get(),"status":"failure"}])
            }
            ["repos", "example", "work", "issues", _, "dependencies"]
                if self.changed.get() && self.change == "dependency" =>
            {
                return Ok(Response {
                    body: json!(page(url, &[issue(3, OPERATOR, &[])])),
                    total: None,
                });
            }
            ["repos", "example", "work", "pulls"]
                if self.changed.get() && self.change == "pull" =>
            {
                return Ok(Response {
                    body: json!([pull(12, BOT, "Closes #7")]),
                    total: Some(1),
                });
            }
            _ => return self.reads.get(url),
        };
        Ok(Response { body, total: None })
    }

    fn send(&self, method: &str, url: &url::Url, body: &Value) -> Result<(u16, Value)> {
        self.check_url(url);
        self.writes.borrow_mut().push(url.path().into());
        let path = url.path();
        if method == "PUT" {
            assert_eq!(body["branch"], LEASE_BRANCH);
            let n: usize = path
                .rsplit('/')
                .next()
                .unwrap()
                .trim_end_matches(".json")
                .parse()?;
            let mut slots = self.slots.borrow_mut();
            let proposed: Value =
                serde_json::from_slice(&STANDARD.decode(body["content"].as_str().unwrap())?)?;
            if self.fault == "bind409" && proposed["run"].as_u64().is_some_and(|n| n > 0) {
                return Ok((409, Value::Null));
            }
            if self.fault == "409" || body["sha"] != format!("{:040x}", slots[n].0) {
                return Ok((409, Value::Null));
            }
            slots[n].0 += 1;
            slots[n].1 =
                serde_json::from_slice(&STANDARD.decode(body["content"].as_str().unwrap())?)?;
            return Ok((200, json!({})));
        }
        if path.ends_with("/comments") {
            let item: u64 = path.split('/').rev().nth(1).unwrap().parse()?;
            let id = 500 + self.comments.borrow().values().map(Vec::len).sum::<usize>() as u64;
            let comment = body["body"].as_str().unwrap();
            let receipt = comment.starts_with("<!-- agent-board-receipt:");
            if self.fault == "receipt-before" && receipt {
                bail!("receipt write failed");
            }
            self.add(item, id, BOT, comment);
            if self.fault == "receipt-lost" && receipt {
                bail!("receipt reply lost");
            }
            if comment.starts_with("<!-- agent-board-dispatch:") {
                self.change_after("claim");
            }
            if receipt && comment.contains("\"event\":\"rerun\"") {
                self.change_after("rerun-journal");
            }
            if self.fault == "claim-reply-lost"
                && body["body"]
                    .as_str()
                    .unwrap()
                    .starts_with("<!-- agent-board-dispatch:")
            {
                bail!("claim reply lost");
            }
            if self.fault == "edited-after-claim"
                && body["body"]
                    .as_str()
                    .unwrap()
                    .starts_with("<!-- agent-board-dispatch:")
            {
                self.comments.borrow_mut().get_mut(&7).unwrap()[0]["updated_at"] =
                    "2026-10-23T15:15:11Z".into();
            }
            return Ok((201, json!({"id":id})));
        }
        if path.ends_with("/dispatches") {
            let inputs = &body["inputs"];
            let id = 41 + self.runs.borrow().len() as u64;
            let mut run = caller_run(
                id,
                &format!(
                    "{} {}#{} ({})",
                    inputs["kind"].as_str().unwrap(),
                    inputs["repo"].as_str().unwrap(),
                    inputs["item"].as_str().unwrap(),
                    inputs["pool"].as_str().unwrap()
                ),
            );
            run["created"] = "2026-10-23T15:15:11Z".into();
            run["trigger_event"] = "workflow_dispatch".into();
            run["event_payload"] = json!({"inputs":inputs}).to_string().into();
            if self.fault == "wrong-run" {
                run["trigger_user"]["id"] = 999.into();
            }
            self.runs.borrow_mut().push(run);
            if self.fault == "lost-reply" {
                bail!("start reply lost");
            }
            return Ok((
                if self.fault == "204" { 204 } else { 201 },
                json!({"id":id}),
            ));
        }
        if path.ends_with("/cancel") {
            self.runs.borrow_mut()[0]["status"] = "cancelled".into();
            return Ok((204, Value::Null));
        }
        if path.ends_with("/rerun") {
            if self.fault == "rerun-lost" {
                bail!("rerun reply lost");
            }
            self.jobs.set(self.jobs.get() + 1);
            self.runs.borrow_mut()[0]["status"] = "waiting".into();
            return Ok((204, Value::Null));
        }
        panic!("unexpected write {url}");
    }

    fn write(&self, method: &str, url: &url::Url, body: &Value) -> Result<Value> {
        let (status, answer) = self.send(method, url, body)?;
        ensure!((200..300).contains(&status), "write refused {status}");
        Ok(answer)
    }
}

fn applying_config() -> Config {
    let mut value = file();
    value["board"]["url"] = URL.into();
    value["wip"] = 2.into();
    value["workRepositories"] = json!(["example/work"]);
    value["pools"] = json!({"claude":{}});
    value["leases"] = true.into();
    value["reruns"] = true.into();
    config(&value)
}

fn applying(fake: &Applying, limits: Limits) -> Report {
    run(
        fake,
        &BrokerFake::default(),
        &applying_config(),
        (Mode::Local, limits),
        usage::tests::now(),
    )
    .unwrap()
}

#[test]
fn apply_start_bind_receipt_and_second_pass_is_idle() {
    let fake = Applying::new();
    let report = applying(&fake, Limits::default());
    assert_eq!(report.failures, 0, "{:?}", report.lines);
    assert_eq!(fake.posts("/dispatches"), 1);
    assert_eq!(report.tally.1.started, 1);
    assert!(
        report.summary.contains("dispatched 1, writes 4"),
        "{}",
        report.summary
    );
    let writes = fake.writes.borrow();
    assert!(writes[0].ends_with("leases/0.json"));
    assert!(writes[1].ends_with("/comments"));
    assert!(writes[2].ends_with("/dispatches"));
    assert!(writes[3].ends_with("leases/0.json"));
    assert!(writes[4].ends_with("/comments"));
    drop(writes);
    assert_eq!(fake.slots.borrow()[0].1["run"], 41);
    assert!(
        fake.comments.borrow()[&7].last().unwrap()["body"]
            .as_str()
            .unwrap()
            .contains("actions/runs/1")
    );
    let report = applying(&fake, Limits::default());
    assert_eq!(report.failures, 0, "{:?}", report.lines);
    assert_eq!(fake.posts("/dispatches"), 1);
    assert_eq!(fake.posts("/comments"), 2);
}

#[test]
fn uncertain_or_edited_starts_never_get_a_second_claim_or_post() {
    for fault in [
        "204",
        "wrong-run",
        "lost-reply",
        "claim-reply-lost",
        "edited-after-claim",
    ] {
        let mut fake = Applying::new();
        fake.fault = fault;
        applying(&fake, Limits::default());
        let writes = fake.writes.borrow().len();
        applying(&fake, Limits::default());
        assert_eq!(
            fake.posts("/dispatches"),
            usize::from(!matches!(fault, "claim-reply-lost" | "edited-after-claim")),
            "{fault}"
        );
        assert_eq!(fake.writes.borrow().len(), writes, "{fault}");
        if fault == "wrong-run" {
            assert_eq!(fake.posts("/cancel"), 1);
        }
    }
}

#[test]
fn unreceipted_claim_recovers_one_match_and_otherwise_holds() {
    for matches in 0..=2 {
        let fake = Applying::new();
        applying(&fake, Limits::default());
        fake.comments.borrow_mut().get_mut(&7).unwrap().pop();
        // Model a pass interrupted just before bind/receipt.
        fake.slots.borrow_mut()[0].1["run"] = 0.into();
        if matches == 0 {
            fake.runs.borrow_mut().clear();
        }
        if matches == 2 {
            let mut other = fake.runs.borrow()[0].clone();
            other["id"] = 42.into();
            other["index_in_repo"] = 2.into();
            fake.runs.borrow_mut().push(other);
        }
        applying(&fake, Limits::default());
        let comments = fake.comments.borrow();
        assert_eq!(comments[&7].len(), if matches == 1 { 3 } else { 2 });
        if matches == 1 {
            assert!(comments[&7][2]["body"].as_str().unwrap().contains("found"));
        }
        assert_eq!(fake.posts("/dispatches"), 1);
    }
}

#[test]
fn admission_caps_races_and_untrusted_plans() {
    for case in [
        "409",
        "full",
        "writes",
        "dispatch",
        "non-recorder",
        "existing-claim",
        "no-leases",
    ] {
        let mut fake = Applying::new();
        let mut limits = Limits::default();
        let mut config = applying_config();
        match case {
            "409" => fake.fault = "409",
            "full" => {
                for id in [41, 42] {
                    fake.runs
                        .borrow_mut()
                        .push(caller_run(id, "implement example/work#8 (claude)"));
                }
            }
            "writes" => limits.writes = Some(3),
            "dispatch" => limits.dispatch = Some(0),
            "non-recorder" => {
                fake.comments.borrow_mut().get_mut(&7).unwrap()[0]["user"]["id"] = 999.into()
            }
            "existing-claim" => fake.add(
                7,
                401,
                OPERATOR,
                "<!-- agent-board-dispatch: 400@2026-10-23T15:15:10Z -->\nClaim",
            ),
            "no-leases" => config.leases = false,
            _ => unreachable!(),
        }
        let report = run(
            &fake,
            &BrokerFake::default(),
            &config,
            (Mode::Local, limits),
            usage::tests::now(),
        )
        .unwrap();
        assert_eq!(fake.posts("/dispatches"), 0, "{case}: {:?}", report.lines);
        assert_eq!(fake.posts("/comments"), 0, "{case}");
        assert_eq!(fake.writes.borrow().len(), usize::from(case == "409"));
    }
}

#[test]
fn caps_reached_between_candidates_do_not_claim_the_next_plan() {
    for limits in [
        Limits {
            dispatch: Some(1),
            ..Limits::default()
        },
        Limits {
            writes: Some(4),
            ..Limits::default()
        },
    ] {
        let mut fake = Applying::new();
        fake.reads
            .listed
            .issues
            .push(issue(8, OPERATOR, &["agent-board"]));
        let body = fake.comments.borrow()[&7][0]["body"]
            .as_str()
            .unwrap()
            .replace("\"item\":7", "\"item\":8");
        fake.add(8, 401, OPERATOR, &body);
        let report = applying(&fake, limits);
        assert_eq!(report.failures, 0, "{:?}", report.lines);
        assert_eq!(fake.posts("/dispatches"), 1);
        assert_eq!(fake.comments.borrow()[&8].len(), 1);
        assert!(
            report.lines.iter().any(|l| l.starts_with("held by --max-")),
            "{:?}",
            report.lines
        );
    }
}

#[test]
fn failed_run_gets_one_attempt_guarded_rerun_even_if_reply_is_lost() {
    for fault in ["", "rerun-lost", "attempt2"] {
        let mut fake = Applying::new();
        applying(&fake, Limits::default());
        fake.runs.borrow_mut()[0]["status"] = "failure".into();
        if fault == "attempt2" {
            fake.jobs.set(2);
        }
        fake.fault = fault;
        applying(&fake, Limits::default());
        fake.runs.borrow_mut()[0]["status"] = "failure".into();
        applying(&fake, Limits::default());
        assert_eq!(
            fake.posts("/rerun"),
            usize::from(fault != "attempt2"),
            "{fault}"
        );
        assert_eq!(fake.posts("/dispatches"), 1, "{fault}");
        if fault != "attempt2" {
            assert!(
                fake.comments.borrow()[&7]
                    .iter()
                    .any(|c| c["body"].as_str().unwrap().contains("\"attempt\":1"))
            );
        }
    }
}

#[test]
fn terminal_and_expired_leases_are_released_without_starting_again() {
    for expiry in [false, true] {
        let fake = Applying::new();
        applying(&fake, Limits::default());
        if expiry {
            fake.slots.borrow_mut()[0].1["expires_at"] = "2026-10-22T15:15:10Z".into();
        } else {
            fake.runs.borrow_mut()[0]["status"] = "success".into();
        }
        applying(&fake, Limits::default());
        assert!(fake.slots.borrow()[0].1["free"].is_string());
        assert_eq!(fake.posts("/dispatches"), 1);
    }
}

#[test]
fn forgejo_actions_uses_the_pass_workflows_own_runs() {
    for other in [false, true] {
        let fake = Applying::new();
        let mut own = caller_run(50, "Board pass");
        own["workflow_id"] = "pass.yml".into();
        fake.runs.borrow_mut().push(own.clone());
        if other {
            own["id"] = 51.into();
            fake.runs.borrow_mut().push(own);
        }
        let own = PassRun {
            repository: "example/control".into(),
            id: 50,
        };
        let report = run(
            &fake,
            &BrokerFake::default(),
            &applying_config(),
            (Mode::Actions(&own), Limits::default()),
            usage::tests::now(),
        )
        .unwrap();
        assert_eq!(
            fake.posts("/dispatches"),
            usize::from(!other),
            "{:?}",
            report.lines
        );
        if other {
            assert!(fake.writes.borrow().is_empty());
        }
    }
}

#[test]
fn newest_recorder_plan_replaces_older_invalid_plans_but_never_falls_back() {
    for case in [
        "older-invalid",
        "newest-invalid",
        "visitor-invalid",
        "replacement",
        "edited-claim",
        "edited-receipt",
    ] {
        let fake = Applying::new();
        match case {
            "older-invalid" => fake.add(7, 399, OPERATOR, "```agent-board-plan\nnot JSON\n```"),
            "newest-invalid" => fake.add(7, 401, OPERATOR, "```agent-board-plan\nnot JSON\n```"),
            "visitor-invalid" => fake.add(7, 401, 999, "```agent-board-plan\nnot JSON\n```"),
            "replacement" => {
                let body = fake.comments.borrow()[&7][0]["body"]
                    .as_str()
                    .unwrap()
                    .replace("Implement the task.", "Replacement task.");
                fake.add(7, 401, BOT, &body);
            }
            "edited-claim" | "edited-receipt" => {
                applying(&fake, Limits::default());
                let n = if case == "edited-claim" { 1 } else { 2 };
                fake.comments.borrow_mut().get_mut(&7).unwrap()[n]["updated_at"] =
                    "2026-10-23T15:15:12Z".into();
            }
            _ => unreachable!(),
        }
        applying(&fake, Limits::default());
        assert_eq!(
            fake.posts("/dispatches"),
            usize::from(case != "newest-invalid"),
            "{case}"
        );
        if case == "replacement" {
            assert!(
                fake.runs.borrow()[0]["event_payload"]
                    .as_str()
                    .unwrap()
                    .contains("Replacement task.")
            );
            assert!(
                fake.comments.borrow()[&7][2]["body"]
                    .as_str()
                    .unwrap()
                    .contains("401@")
            );
        }
    }
}

#[test]
fn final_admission_rechecks_changed_facts_for_starts_and_journaled_reruns() {
    for boundary in ["claim", "rerun-journal"] {
        for change in [
            "question",
            "epic",
            "needs-human",
            "membership",
            "closed",
            "identity",
            "dependency",
            "pull",
            "wip",
            "owner",
            "new-plan",
        ] {
            let mut fake = Applying::new();
            if boundary == "rerun-journal" {
                applying(&fake, Limits::default());
                fake.runs.borrow_mut()[0]["status"] = "failure".into();
            }
            fake.boundary = boundary;
            fake.change = change;
            applying(&fake, Limits::default());
            assert!(fake.changed.get(), "{boundary}/{change}");
            assert_eq!(
                fake.posts("/dispatches"),
                usize::from(boundary == "rerun-journal"),
                "{boundary}/{change}"
            );
            assert_eq!(fake.posts("/rerun"), 0, "{boundary}/{change}");
            // A final hold after the durable journal may consume its one chance,
            // but can never turn into a second POST on a following pass.
            if boundary == "rerun-journal" && change != "new-plan" {
                applying(&fake, Limits::default());
                assert_eq!(fake.posts("/rerun"), 0, "{change}");
            }
        }
    }
}

#[test]
fn confirmed_start_survives_bind_and_receipt_failures_without_redispatch() {
    for fault in ["bind409", "receipt-before", "receipt-lost"] {
        let mut fake = Applying::new();
        fake.fault = fault;
        applying(&fake, Limits::default());
        fake.fault = "";
        applying(&fake, Limits::default());
        assert_eq!(fake.posts("/dispatches"), 1, "{fault}");
        assert_eq!(
            fake.comments.borrow()[&7]
                .iter()
                .filter(|c| c["body"]
                    .as_str()
                    .unwrap()
                    .starts_with("<!-- agent-board-dispatch:"))
                .count(),
            1
        );
        assert!(fake.comments.borrow()[&7].iter().any(|c| {
            c["body"]
                .as_str()
                .unwrap()
                .starts_with("<!-- agent-board-receipt:")
        }));
    }
}

#[test]
fn pending_claim_recovers_even_when_full_or_unpaced() {
    for full in [false, true] {
        let fake = Applying::new();
        applying(&fake, Limits::default());
        fake.comments.borrow_mut().get_mut(&7).unwrap().pop();
        fake.slots.borrow_mut()[0].1["run"] = 0.into();
        if full {
            fake.runs
                .borrow_mut()
                .push(caller_run(80, "implement example/work#8 (claude)"));
        }
        let broker = BrokerFake {
            usage: None,
            ..BrokerFake::default()
        };
        run(
            &fake,
            &broker,
            &applying_config(),
            (Mode::Local, Limits::default()),
            usage::tests::now(),
        )
        .unwrap();
        assert!(
            fake.comments.borrow()[&7].last().unwrap()["body"]
                .as_str()
                .unwrap()
                .contains("\"event\":\"found\"")
        );
        assert_eq!(fake.posts("/dispatches"), 1);
    }
}

#[test]
fn request_deadline_and_cancellation_write_cap_never_authorize_a_retry() {
    for limit in [0, 30, 70, 100, 140] {
        let fake = Applying::new();
        let bounds = Bounds::new(limit, Instant::now() + Duration::from_secs(60));
        let report = run_within(
            &fake,
            &BrokerFake::default(),
            &applying_config(),
            (Mode::Local, Limits::default()),
            usage::tests::now(),
            &bounds,
        )
        .unwrap();
        assert!(report.spent.requests <= limit);
        applying(&fake, Limits::default());
        assert!(
            fake.posts("/dispatches") <= 1,
            "limit {limit}: {:?}",
            report.lines
        );
    }
    let fake = Applying::new();
    let bounds = Bounds::new(PASS_REQUEST_LIMIT, Instant::now() - Duration::from_secs(1));
    let report = run_within(
        &fake,
        &BrokerFake::default(),
        &applying_config(),
        (Mode::Local, Limits::default()),
        usage::tests::now(),
        &bounds,
    )
    .unwrap();
    assert_eq!(report.spent.requests, 0);
    assert!(fake.writes.borrow().is_empty());

    let mut fake = Applying::new();
    fake.fault = "wrong-run";
    let report = applying(
        &fake,
        Limits {
            writes: Some(4),
            ..Limits::default()
        },
    );
    assert!(report.summary.contains("writes 4/4"), "{}", report.summary);
    assert_eq!(fake.posts("/cancel"), 1);
    assert_eq!(fake.posts("/comments"), 2);
    applying(&fake, Limits::default());
    assert_eq!(fake.posts("/dispatches"), 1);
}
