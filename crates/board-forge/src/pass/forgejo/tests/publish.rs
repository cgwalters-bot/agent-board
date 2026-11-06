//! What a Forgejo pass writes back: stage 5's `status/` labels and the
//! usage and hold snapshots.
use super::*;

/// The publication repository the snapshot tests name.
const PRIVATE: &str = "example/private";

/// A board whose labels and publication issues an applying pass writes.
struct Board {
    reads: Fake,
    /// example/work's issues, open and closed.
    issues: RefCell<Vec<Value>>,
    /// Each comment by issue number, on the publication repository.
    comments: RefCell<BTreeMap<u64, Vec<Value>>>,
    private: bool,
    /// An issue a person relabels between the board's read and the write.
    relabelled: Option<u64>,
    writes: RefCell<Vec<String>>,
}

impl Board {
    fn new(issues: Vec<Value>) -> Self {
        Self {
            reads: Fake::default(),
            issues: RefCell::new(issues),
            comments: RefCell::default(),
            private: true,
            relabelled: None,
            writes: RefCell::default(),
        }
    }

    fn issue(&self, n: u64) -> Value {
        let issues = self.issues.borrow();
        issues.iter().find(|i| i["number"] == n).unwrap().clone()
    }

    fn labels(&self, n: u64) -> Vec<String> {
        let issue = self.issue(n);
        let labels = issue["labels"].as_array().unwrap().iter();
        labels.map(|l| text(l, "/name").to_owned()).collect()
    }

    fn posted(&self, n: u64) -> Vec<String> {
        let comments = self.comments.borrow();
        let posted = comments.get(&n).into_iter().flatten();
        posted.map(|c| text(c, "/body").to_owned()).collect()
    }
}

/// Every label example/work has, exclusive where scoped.
fn repository_labels() -> Vec<Value> {
    let statuses = [
        "todo",
        "backlog",
        "blocked",
        "in-progress",
        "needs-human",
        "done",
    ];
    let names = ["agent-board".to_owned(), "needs-human".to_owned()]
        .into_iter()
        .chain(statuses.map(|s| format!("status/{s}")));
    (1..)
        .zip(names)
        .map(|(id, name)| json!({"id": id, "exclusive": name.contains('/'), "name": name}))
        .collect()
}

impl ForgejoTransport for Board {
    fn get(&self, url: &url::Url) -> Result<Response> {
        assert!(url.as_str().starts_with(&format!("{URL}/api/v1/")), "{url}");
        let path = url.path().strip_prefix("/subpath/api/v1/").unwrap();
        let listed = |items: Vec<Value>| {
            let total = Some(items.len() as u64);
            Ok(Response {
                body: page(url, &items).into(),
                total,
            })
        };
        let body = match path.split('/').collect::<Vec<_>>()[..] {
            ["orgs", "example"] => return Err(crate::forgejo::NotFound.into()),
            ["repos", "example", "work", "labels"] => return listed(repository_labels()),
            ["repos", "example", "work", "issues"] => {
                let state = url.query_pairs().find(|(k, _)| k == "state").unwrap().1;
                let issues = self.issues.borrow();
                let issues = issues.iter().filter(|i| i["state"] == *state);
                return listed(issues.cloned().collect());
            }
            ["repos", "example", "work", "issues", n] => {
                let n: u64 = n.parse()?;
                let mut issue = self.issue(n);
                if self.relabelled == Some(n) {
                    let labels = issue["labels"].as_array_mut().unwrap();
                    labels.push(json!({"id": 99, "name": "priority/P1"}));
                }
                issue
            }
            ["repos", "example", "private"] => {
                json!({"id": 9, "full_name": PRIVATE, "private": self.private})
            }
            ["repos", "example", "private", "issues", n] => {
                let n: u64 = n.parse()?;
                let comments = self.comments.borrow().get(&n).map_or(0, Vec::len);
                json!({"number": n, "pull_request": null, "comments": comments,
                    "repository": {"full_name": PRIVATE}})
            }
            ["repos", "example", "private", "issues", n, "comments"] => {
                json!(self.posted_raw(n.parse()?))
            }
            ["repos", "example", "private", "issues", "comments", id] => {
                let id: u64 = id.parse()?;
                let comments = self.comments.borrow();
                let mut all = comments.values().flatten();
                all.find(|c| c["id"] == id).unwrap().clone()
            }
            _ => return self.reads.get(url),
        };
        Ok(Response { body, total: None })
    }

    fn send(&self, method: &str, url: &url::Url, body: &Value) -> Result<(u16, Value)> {
        assert!(url.as_str().starts_with(&format!("{URL}/api/v1/")), "{url}");
        self.writes
            .borrow_mut()
            .push(format!("{method} {}", url.path()));
        let path = url.path().strip_prefix("/subpath/api/v1/").unwrap();
        assert_eq!(method, "POST");
        match path.split('/').collect::<Vec<_>>()[..] {
            ["repos", "example", "work", "issues", n, "labels"] => {
                let n: u64 = n.parse()?;
                let [id] = &body["labels"].as_array().unwrap()[..] else {
                    panic!("{body}");
                };
                let labels = repository_labels();
                let label = labels.iter().find(|l| l["id"] == *id).unwrap();
                let scope = text(label, "/name").split_once('/').map(|(s, _)| s);
                let mut issues = self.issues.borrow_mut();
                let issue = issues.iter_mut().find(|i| i["number"] == n).unwrap();
                // Forgejo drops the other labels of an exclusive scope.
                let old = issue["labels"].as_array_mut().unwrap();
                old.retain(|l| {
                    scope.is_none_or(|s| !text(l, "/name").starts_with(&format!("{s}/")))
                });
                old.push(label.clone());
                Ok((200, json!([])))
            }
            ["repos", "example", "private", "issues", n, "comments"] => {
                let n: u64 = n.parse()?;
                let mut comments = self.comments.borrow_mut();
                let id = 700 + comments.values().map(Vec::len).sum::<usize>() as u64;
                let at = "2026-10-23T15:15:10Z";
                comments.entry(n).or_default().push(json!({"id": id,
                    "user": {"id": BOT}, "body": body["body"], "created_at": at, "updated_at": at}));
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

impl Board {
    fn posted_raw(&self, n: u64) -> Vec<Value> {
        self.comments.borrow().get(&n).cloned().unwrap_or_default()
    }
}

/// A labelled board with `statusLabels`, scheduling nothing.
fn status_file() -> Value {
    let mut value = file();
    value["board"]["url"] = URL.into();
    value["board"]["statusLabels"] = true.into();
    value
}

fn closed(mut issue: Value) -> Value {
    issue["state"] = "closed".into();
    issue
}

/// One issue each Status derives to, and those it keeps.
fn derived_board() -> Board {
    let mut board = Board::new(vec![
        issue(1, OPERATOR, &["agent-board"]),
        issue(2, OPERATOR, &["agent-board", "status/todo"]),
        closed(issue(3, OPERATOR, &["agent-board", "status/in-progress"])),
        issue(4, OPERATOR, &["agent-board", "status/todo", "needs-human"]),
        issue(5, OPERATOR, &["agent-board", "status/blocked"]),
        closed(issue(6, OPERATOR, &["agent-board", "status/backlog"])),
        issue(7, OPERATOR, &["agent-board", "status/todo", "status/done"]),
        issue(8, OPERATOR, &["agent-board", "status/todo"]),
        // Closed off the board: not read.
        closed(issue(9, OPERATOR, &["status/todo"])),
    ]);
    board.reads.listed.runs = (
        [
            "implement example/work#2 (claude)",
            "implement example/work#5 (claude)",
        ]
        .iter()
        .zip(43..)
        .map(|(title, id)| caller_run(id, title))
        .collect(),
        0,
    );
    board
}

fn pass(board: &Board, value: &Value, mode: Mode<'_>, limits: Limits) -> Report {
    run(
        board,
        &BrokerFake::default(),
        &config(value),
        (mode, limits),
        usage::tests::now(),
    )
    .unwrap()
}

fn status_lines(report: &Report) -> Vec<&str> {
    let lines = report.lines.iter().map(String::as_str);
    lines.filter(|l| l.starts_with("status")).collect()
}

#[test]
fn derived_status_is_written_once_and_read_back() {
    let board = derived_board();
    let report = pass(&board, &status_file(), Mode::Local, Limits::default());
    assert_eq!(report.failures, 0, "{:?}", report.lines);
    assert_eq!(
        status_lines(&report),
        [
            "status: 4 change(s) of 8 item(s) read; 1 kept, their own facts not read whole",
            "status example/work#1: unset -> Todo (not labelled needs-human, no run in flight or pull request of ours moving)",
            &format!(
                "status example/work#2: Todo -> In Progress (run in flight, {URL}/example/control/actions/runs/3)"
            ),
            "status example/work#3: In Progress -> Done (closed)",
            "status example/work#4: Todo -> Needs human (labelled needs-human)",
        ]
    );
    for (n, label) in [
        (1, "status/todo"),
        (2, "status/in-progress"),
        (3, "status/done"),
        (4, "status/needs-human"),
    ] {
        let labels = board.labels(n);
        let scoped: Vec<_> = labels.iter().filter(|l| l.starts_with("status/")).collect();
        assert_eq!(scoped, [label], "{n}");
    }
    // A person's holds, two labels and one already right are left alone.
    for (n, labels) in [
        (5, vec!["agent-board", "status/blocked"]),
        (6, vec!["agent-board", "status/backlog"]),
        (7, vec!["agent-board", "status/todo", "status/done"]),
        (8, vec!["agent-board", "status/todo"]),
    ] {
        assert_eq!(board.labels(n), labels, "{n}");
    }
    let writes = board.writes.borrow().clone();
    assert_eq!(writes.len(), 4, "{writes:?}");
    assert!(writes.iter().all(|w| w.ends_with("/labels")), "{writes:?}");
    // Read back: the next pass has nothing to change.
    let again = pass(&board, &status_file(), Mode::Local, Limits::default());
    assert_eq!(again.failures, 0, "{:?}", again.lines);
    assert_eq!(
        status_lines(&again),
        ["status: 0 change(s) of 8 item(s) read; 1 kept, their own facts not read whole"]
    );
    assert_eq!(board.writes.borrow().len(), 4);
}

#[test]
fn status_caps_preview_and_a_persons_change() {
    type Case = (
        &'static str,
        Option<u64>,
        Limits,
        usize,
        usize,
        &'static str,
    );
    let caps = |status_changes, writes| Limits {
        status_changes,
        writes,
        ..Limits::default()
    };
    let cases: [Case; 5] = [
        (
            "more changes than --max-status-changes",
            None,
            caps(Some(3), None),
            1,
            0,
            "status: refused, 4 changes are more than --max-status-changes 3; nothing is written. Check them, then pass a higher cap once",
        ),
        (
            "exactly --max-status-changes",
            None,
            caps(Some(4), None),
            0,
            4,
            "",
        ),
        (
            "--max-writes",
            None,
            caps(None, Some(2)),
            0,
            2,
            "held by --max-writes: status example/work#3: In Progress -> Done (closed)",
        ),
        (
            "a person's change",
            Some(1),
            Limits::default(),
            0,
            3,
            "status example/work#1: its labels changed since the board was read; left for the next pass",
        ),
        (
            "off",
            None,
            Limits::default(),
            0,
            0,
            "stage 5 status: off (no board.statusLabels)",
        ),
    ];
    for (name, relabelled, limits, failures, writes, said) in cases {
        let mut board = derived_board();
        board.relabelled = relabelled;
        let mut value = status_file();
        if name == "off" {
            value["board"]["statusLabels"] = false.into();
        }
        let report = pass(&board, &value, Mode::Local, limits);
        assert_eq!(report.failures, failures, "{name}: {:?}", report.lines);
        assert_eq!(board.writes.borrow().len(), writes, "{name}");
        assert!(
            said.is_empty() || report.lines.iter().any(|l| l == said),
            "{name}: {:?}",
            report.lines
        );
    }
    // A preview writes nothing, and counts this pass's own start as a run.
    let mut value = status_file();
    value["wip"] = 2.into();
    value["workRepositories"] = json!(["example/work"]);
    value["pools"] = json!({"claude": {}});
    let fake = Fake {
        listed: Listed {
            issues: vec![issue(7, OPERATOR, &["agent-board", "status/todo"])],
            ..Listed::default()
        },
        ..Fake::default()
    };
    let report = preview(&fake, &BrokerFake::default(), &config(&value));
    assert_eq!(report.failures, 0, "{:?}", report.lines);
    assert_eq!(
        status_lines(&report),
        [
            "status: 1 change(s) of 1 item(s) read",
            "status example/work#7: Todo -> In Progress (run in flight, started in this pass)",
        ]
    );
}

/// Usage and holds published to [`PRIVATE`], with the needs-you list.
fn snapshot_file() -> Value {
    let mut value = file();
    value["board"]["url"] = URL.into();
    value["usage"]["repository"] = PRIVATE.into();
    value["usage"]["issue"] = 1.into();
    value["holds"] = json!({"repository": PRIVATE, "issue": 2, "needsYou": true});
    value
}

/// The pass view is built from GitHub's runs and dispatches alone, which
/// the Forgejo contract says.
#[test]
fn pass_view_is_refused() {
    let mut value = snapshot_file();
    value["holds"]["passView"] = true.into();
    let error = format!("{:#}", Config::parse(&value.to_string()).unwrap_err());
    assert!(
        error.contains("unsupported_capability: holds.passView"),
        "{error}"
    );
}

fn snapshot_lines(report: &Report) -> Vec<&str> {
    let lines = report.lines.iter().map(String::as_str);
    lines
        .filter(|l| l.starts_with("usage: publish") || l.starts_with("usage: published"))
        .chain(
            (report.lines.iter().map(String::as_str))
                .filter(|l| l.starts_with("holds") || l.starts_with("usage: unchanged")),
        )
        .collect()
}

#[test]
fn snapshots_are_appended_only_when_changed() {
    let board = Board::new(Vec::new());
    let value = snapshot_file();
    let report = pass(&board, &value, Mode::Local, Limits::default());
    assert_eq!(report.failures, 0, "{:?}", report.lines);
    assert_eq!(
        snapshot_lines(&report),
        [
            format!("usage: published {URL}/{PRIVATE}/issues/1#issuecomment-700"),
            format!("holds: published {URL}/{PRIVATE}/issues/2#issuecomment-701"),
        ]
    );
    let [usage] = &board.posted(1)[..] else {
        panic!("{:?}", board.posted(1));
    };
    assert!(usage.starts_with(USAGE_MARKER), "{usage}");
    let [held] = &board.posted(2)[..] else {
        panic!("{:?}", board.posted(2));
    };
    assert!(held.starts_with(holds::HOLDS_MARKER), "{held}");
    // Only the contract's needs-you sources: GitHub's are off.
    assert_eq!(
        holds::published(held).unwrap()["needs_you"]["sources"],
        json!({"epics": "off", "items": "unread", "reviews": "off", "proposals": "off", "digest": "off"}),
        "{held}"
    );
    // A minute on, only the clock moved: nothing is posted.
    let later = usage::tests::now() + time::Duration::minutes(1);
    let config = config(&value);
    let limits = (Mode::Local, Limits::default());
    let again = run(&board, &BrokerFake::default(), &config, limits, later).unwrap();
    assert_eq!(again.failures, 0, "{:?}", again.lines);
    assert_eq!(
        snapshot_lines(&again),
        ["usage: unchanged", "holds: unchanged"]
    );
    // A newer snapshot someone edited, or anyone else's, is passed over for
    // the one before it, which is unchanged.
    let at = "2026-10-23T15:15:10Z";
    for (id, user, updated) in [(800, BOT, "2026-10-23T15:16:00Z"), (801, OPERATOR, at)] {
        let body = format!("{}\n```json\n{{}}\n```", holds::HOLDS_MARKER);
        board
            .comments
            .borrow_mut()
            .get_mut(&2)
            .unwrap()
            .push(json!({"id": id,
            "user": {"id": user}, "body": body, "created_at": at, "updated_at": updated}));
    }
    let edited = pass(&board, &value, Mode::Local, Limits::default());
    assert_eq!(
        snapshot_lines(&edited),
        ["usage: unchanged", "holds: unchanged"]
    );
    // Where the only snapshot was edited, there is none: a new one is posted.
    board.comments.borrow_mut().get_mut(&1).unwrap()[0]["updated_at"] =
        "2026-10-23T15:16:00Z".into();
    let replaced = pass(&board, &value, Mode::Local, Limits::default());
    assert!(
        replaced
            .lines
            .iter()
            .any(|l| l.starts_with("usage: published")),
        "{:?}",
        replaced.lines
    );
    assert_eq!(board.posted(1).len(), 2);
    // Nothing is written outside the instance: [`Board`] refuses it.
    let writes = board.writes.borrow();
    assert!(
        writes
            .iter()
            .all(|w| w.starts_with("POST /subpath/api/v1/repos/example/private/")),
        "{writes:?}"
    );
}

#[test]
fn snapshots_need_a_private_repository_and_an_apply() {
    // A preview says what it would post, and posts nothing.
    let board = Board::new(Vec::new());
    let report = pass(&board, &snapshot_file(), Mode::Preview, Limits::default());
    assert_eq!(report.failures, 0, "{:?}", report.lines);
    assert_eq!(
        snapshot_lines(&report),
        [
            format!("usage: publish to {PRIVATE}#1; a preview does not write it"),
            format!("holds: publish to {PRIVATE}#2; a preview does not write it"),
        ]
    );
    assert!(board.writes.borrow().is_empty());
    // A repository that is not private refuses the pass before stage 1, for
    // either snapshot, in a preview too.
    for (option, mode) in [("usage", Mode::Preview), ("holds", Mode::Local)] {
        let mut board = Board::new(Vec::new());
        board.private = false;
        let mut value = snapshot_file();
        match option {
            "usage" => value.as_object_mut().unwrap().remove("holds"),
            _ => value["usage"].as_object_mut().unwrap().remove("repository"),
        };
        if option == "holds" {
            value["usage"].as_object_mut().unwrap().remove("issue");
        }
        let report = pass(&board, &value, mode, Limits::default());
        assert_eq!(report.failures, 1, "{option}");
        assert_eq!(report.lines.len(), 2, "{option}: {:?}", report.lines);
        assert_eq!(
            report.lines[1],
            format!(
                "config error: {option}.repository {PRIVATE} is not private on this instance; its snapshot is not published there"
            )
        );
        assert!(board.writes.borrow().is_empty());
        assert!(board.posted(1).is_empty() && board.posted(2).is_empty());
    }
}
