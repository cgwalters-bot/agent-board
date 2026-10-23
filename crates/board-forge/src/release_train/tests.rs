use super::*;
use std::cell::RefCell;

/// Recorded from GitHub with one dry-run pass of tracker's train.cjs at
/// 1a550cc, which printed the `.txt` beside each: tracker#880 halted as it
/// is; the same with `needs-human` removed, which stalls on the wrappers'
/// commit pins (tracker#887); and 2026-10-22T16:31:43Z, when the listing of
/// open trains came back empty and the pass opened a second train
/// (tracker#882). Responses are trimmed to the fields the train reads,
/// keyed by path with the query decoded; null is a 404.
const CASES: &[(&str, &str, &str)] = &[
    (
        "halted",
        include_str!("../../../../fixtures/release-train/halted.json"),
        include_str!("../../../../fixtures/release-train/halted.txt"),
    ),
    (
        "wrappers-887",
        include_str!("../../../../fixtures/release-train/wrappers-887.json"),
        include_str!("../../../../fixtures/release-train/wrappers-887.txt"),
    ),
    (
        "duplicate-882",
        include_str!("../../../../fixtures/release-train/duplicate-882.json"),
        include_str!("../../../../fixtures/release-train/duplicate-882.txt"),
    ),
];

struct Fake {
    responses: RefCell<serde_json::Map<String, Value>>,
    apply: bool,
    writes: RefCell<Vec<String>>,
    /// Who posts the comments, and when, where a listing of them is recorded.
    bot: String,
    now: String,
}

/// `path` with its query decoded, as the fixtures key it.
fn decoded(path: &str) -> String {
    let Some((path, query)) = path.split_once('?') else {
        return path.into();
    };
    let pairs: Vec<String> = url::form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    format!("{path}?{}", pairs.join("&"))
}

impl Api for Fake {
    fn request(&self, method: &str, path: &str, body: &Value) -> Result<Value> {
        if method != "GET" {
            assert!(self.apply, "a preview wrote: {method} {path}");
            let mut writes = self.writes.borrow_mut();
            writes.push(format!("{method} {path}"));
            let id = writes.len();
            let listing = format!("{path}?per_page=100");
            if let Some(Value::Array(comments)) = self.responses.borrow_mut().get_mut(&listing) {
                comments.push(json!({"id": id, "user": {"login": self.bot},
                    "body": body["body"], "created_at": self.now}));
            }
            // As train.test.cjs's fake answers every write.
            return Ok(
                json!({"html_url": format!("https://example/{id}"), "id": id,
                "number": 7, "sha": "s", "title": body["title"], "labels": []}),
            );
        }
        match self.responses.borrow().get(&decoded(path)) {
            Some(body) if !body.is_null() => Ok(body.clone()),
            _ => Err(Http {
                method: method.into(),
                path: path.split('?').next().unwrap_or_default().into(),
                status: 404,
            }
            .into()),
        }
    }
}

fn fixture(name: &str) -> Value {
    let (_, text, _) = CASES.iter().find(|(n, _, _)| *n == name).unwrap();
    serde_json::from_str(text).unwrap()
}

/// The pass's lines, then its writes; or its error.
fn replay(fixture: &Value, apply: bool) -> Result<String, String> {
    let config = Config::from_json(&fixture["config"]).unwrap();
    let now = parse_time(fixture["now"].as_str().unwrap()).unwrap();
    let fake = Fake {
        responses: RefCell::new(fixture["responses"].as_object().unwrap().clone()),
        apply,
        writes: RefCell::default(),
        bot: config.bot.clone(),
        now: fixture["now"].as_str().unwrap().into(),
    };
    let mut out = vec![];
    let result = pass(&fake, &config, apply, now, &mut out);
    let mut text = String::from_utf8(out).unwrap();
    for write in fake.writes.borrow().iter() {
        text.push_str(write);
        text.push('\n');
    }
    result.map(|()| text).map_err(|e| format!("{e:#}"))
}

#[test]
fn previews_print_what_train_cjs_printed() {
    for (name, _, expected) in CASES {
        assert_eq!(
            replay(&fixture(name), false).as_deref(),
            Ok(*expected),
            "{name}"
        );
    }
}

#[test]
fn applied_passes_write_what_train_cjs_wrote() {
    let issue = "https://github.com/cgwalters-forge/tracker/issues/880";
    let halted = "Halted: no single pin of cgwalters-forge/agentic-job with its version. Fix it, then remove the `needs-human` label.";
    for (name, expected) in [
        (
            "halted",
            vec![format!("{issue} is labelled needs-human: halted")],
        ),
        (
            "wrappers-887",
            vec![
                format!("{issue} v0.6.4: wrappers"),
                format!("label {issue} needs-human"),
                format!("comment on {issue}: {halted}"),
                "POST /repos/cgwalters-forge/tracker/issues/880/labels".into(),
                "POST /repos/cgwalters-forge/tracker/issues/880/comments".into(),
            ],
        ),
        // tracker#882: with tracker#772 open, a second train and its bump.
        (
            "duplicate-882",
            vec![
                "not following v0.6.3: no single pin of cgwalters-forge/agentic-job with its version".into(),
                "due: 49 pull requests merged since v0.6.3; next is v0.6.4".into(),
                "open the tracking issue \"agentic-job v0.6.4 release train\"".into(),
                "https://example/1 v0.6.4: bump".into(),
                "open a PR on cgwalters-forge/agentic-job from cgwalters-bot/agentic-job:release-train/v0.6.4 changing Cargo.toml, Cargo.lock: release: Prepare 0.6.4".into(),
                "comment on https://example/1: Opened the version bump: https://example/5".into(),
                "POST /repos/cgwalters-forge/tracker/issues".into(),
                "POST /repos/cgwalters-bot/agentic-job/git/trees".into(),
                "POST /repos/cgwalters-bot/agentic-job/git/commits".into(),
                "POST /repos/cgwalters-bot/agentic-job/git/refs".into(),
                "POST /repos/cgwalters-forge/agentic-job/pulls".into(),
                "POST /repos/cgwalters-forge/tracker/issues/7/comments".into(),
            ],
        ),
    ] {
        let expected: String = expected.iter().map(|l| format!("{l}\n")).collect();
        assert_eq!(replay(&fixture(name), true), Ok(expected), "{name}");
    }
}

/// The recorded state with one response replaced.
fn edited(name: &str, edits: &[(&str, Value)]) -> Value {
    let mut fixture = fixture(name);
    for (path, value) in edits {
        fixture["responses"][*path] = value.clone();
    }
    fixture
}

#[test]
fn boundaries_hold_on_recorded_state() {
    let open = "/repos/cgwalters-forge/tracker/issues?state=open&labels=release-train&creator=cgwalters-bot&per_page=100";
    let train = |login: &str| {
        json!({"number": 9, "html_url": "https://github.com/cgwalters-forge/tracker/issues/9",
            "title": "agentic-job v0.6.4 release train", "user": {"login": login}, "labels": [],
            "created_at": "2026-10-22T16:00:00Z"})
    };
    let sha = fixture("wrappers-887")["responses"]["/repos/cgwalters-forge/agentic-job/commits/v0.6.4"]["sha"]
        .as_str()
        .unwrap()
        .to_owned();
    let compare = format!("/repos/cgwalters-forge/agentic-job/compare/main...{sha}");
    let halted = |issue: &str, why: &str| {
        format!(
            "would: comment on {issue}: Halted: {why}. Fix it, then remove the `needs-human` label."
        )
    };
    let issue = "https://github.com/cgwalters-forge/tracker/issues/880";
    // The recorded state edited, then a line the pass must print or its error.
    for (fixture, expected) in [
        // Anyone may open an issue with the train's title; only the bot's count.
        (
            edited("duplicate-882", &[(open, json!([train("someone")]))]),
            Ok("due: 49 pull requests merged since v0.6.3; next is v0.6.4".to_owned()),
        ),
        (
            edited(
                "duplicate-882",
                &[(
                    open,
                    json!([train("cgwalters-bot"), train("cgwalters-bot")]),
                )],
            ),
            Err("more than one open release train"),
        ),
        // A fork of another repository could take the branch but never be its PR.
        (
            edited(
                "wrappers-887",
                &[(
                    "/repos/cgwalters-bot/actions-1",
                    json!({"parent": {"full_name": "elsewhere/actions"}}),
                )],
            ),
            Ok(halted(
                issue,
                "cgwalters-bot/actions-1 is not a fork of cgwalters-forge/actions",
            )),
        ),
        // A tag moved off the default branch is carried into nobody's pins.
        (
            edited(
                "wrappers-887",
                &[(compare.as_str(), json!({"status": "diverged"}))],
            ),
            Ok(halted(
                issue,
                &format!("v0.6.4 is at {sha}, which is not on cgwalters-forge/agentic-job's main"),
            )),
        ),
        // A pin bump merged past the default branch's review gate is not
        // carried into the callers.
        (
            edited(
                "wrappers-887",
                &[(
                    "/repos/cgwalters-forge/actions/pulls?state=all&head=cgwalters-bot:release-train/v0.6.4&per_page=100",
                    json!([{"state": "closed", "merged_at": "2026-10-23T12:00:00Z",
                        "html_url": "https://example/side", "merge_commit_sha": "e".repeat(40),
                        "created_at": "2026-10-23T11:30:00Z",
                        "head": {"ref": "release-train/v0.6.4", "repo": {"full_name": "cgwalters-bot/actions-1"}},
                        "base": {"ref": "side", "repo": {"full_name": "cgwalters-forge/actions", "default_branch": "main"}}}]),
                )],
            ),
            Ok(halted(
                issue,
                "the wrappers' pin bump is from cgwalters-bot/actions-1:release-train/v0.6.4 into cgwalters-forge/actions:side, not from cgwalters-bot/actions-1:release-train/v0.6.4 into cgwalters-forge/actions's default branch",
            )),
        ),
        // A dropped train's PR still open on the branch is not force-pushed.
        (
            edited(
                "duplicate-882",
                &[(
                    "/repos/cgwalters-forge/agentic-job/pulls?state=open&head=cgwalters-bot:release-train/v0.6.4&per_page=100",
                    json!([{"html_url": "https://example/old"}]),
                )],
            ),
            Ok(halted(
                "(new tracking issue)",
                "https://example/old from a dropped train is still open on release-train/v0.6.4; close it",
            )),
        ),
        // Labels decide the bump and the urgency: all of them or none.
        (
            edited(
                "duplicate-882",
                &[(
                    "/search/issues?q=repo:cgwalters-forge/agentic-job is:pr is:merged base:main merged:>2026-10-19T15:04:26Z&per_page=100",
                    json!({"total_count": 101, "incomplete_results": false, "items": []}),
                )],
            ),
            Err("merged pull requests truncated"),
        ),
    ] {
        match (replay(&fixture, false), expected) {
            (Ok(out), Ok(line)) => assert!(out.lines().any(|l| l == line), "{line}\n{out}"),
            (Err(error), Err(expected)) => assert!(error.contains(expected), "{error}"),
            (got, expected) => panic!("{got:?}, expected {expected:?}"),
        }
    }
}

/// `text` in base64, as the contents API answers.
fn base64(text: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in text.as_bytes().chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(match i <= chunk.len() {
                true => char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]),
                false => '=',
            });
        }
    }
    out
}

#[test]
fn the_release_is_dispatched_once_for_the_version_on_main() {
    // tracker#880 before v0.6.4 was released: its bump merged, the default
    // branch as recorded in duplicate-882, at a workspace version given.
    let issue = "https://github.com/cgwalters-forge/tracker/issues/880";
    let head = "d32f1ecea1a1fcc925f71a19d6eab7ab85e096c1";
    let bump = "/repos/cgwalters-forge/agentic-job/pulls?state=all&head=cgwalters-bot:release-train/v0.6.4&per_page=100";
    let comments = "/repos/cgwalters-forge/tracker/issues/880/comments?per_page=100";
    let runs = |since: &str| {
        format!(
            "/repos/cgwalters-forge/agentic-job/actions/workflows/release.yml/runs?event=workflow_dispatch&actor=cgwalters-bot&head_sha={head}&created=>={since}&per_page=100"
        )
    };
    let marker = format!("<!-- release-train dispatched v0.6.4 at {head} -->");
    let record = |login: &str, at: &str| json!([{"id": 1, "user": {"login": login}, "body": format!("Running {marker}"), "created_at": at}]);
    let unreleased = |workspace: &str, edits: &[(&str, Value)]| {
        let recorded = fixture("duplicate-882");
        let mut f = fixture("wrappers-887");
        for path in [
            "/repos/cgwalters-forge/agentic-job/releases/latest",
            "/repos/cgwalters-forge/agentic-job/contents/Cargo.toml?ref=main",
            "/search/issues?q=repo:cgwalters-forge/agentic-job is:pr is:merged base:main merged:>2026-10-19T15:04:26Z&per_page=100",
            "/repos/cgwalters-forge/agentic-job/commits/main",
        ] {
            f["responses"][path] = recorded["responses"][path].clone();
        }
        let toml = format!("[workspace.package]\nversion = \"{workspace}\"\n");
        for (path, value) in [
            (
                "/repos/cgwalters-forge/agentic-job/releases/tags/v0.6.4".to_owned(),
                Value::Null,
            ),
            (
                "/repos/cgwalters-forge/agentic-job/compare/main...f520169b807923b3a2c127119292eadc748838c6".into(),
                json!({"status": "behind"}),
            ),
            (comments.into(), json!([])),
            (
                format!("/repos/cgwalters-forge/agentic-job/contents/Cargo.toml?ref={head}"),
                json!({"encoding": "base64", "content": base64(&toml)}),
            ),
        ] {
            f["responses"][path] = value;
        }
        for (path, value) in edits {
            f["responses"][*path] = value.clone();
        }
        f
    };
    let halted = |why: &str| {
        format!(
            "would: comment on {issue}: Halted: {why}. Fix it, then remove the `needs-human` label."
        )
    };
    let step = |step: &str| format!("{issue} v0.6.4: {step}");
    let dispatch =
        "POST /repos/cgwalters-forge/agentic-job/actions/workflows/release.yml/dispatches";
    let mut side = fixture("wrappers-887")["responses"][bump].clone();
    side[0]["base"]["ref"] = json!("side");
    // The state, whether to apply, then lines the pass must print.
    for (fixture, apply, expected) in [
        // Recorded first, then dispatched; the record is the pass's own.
        (
            unreleased("0.6.4", &[]),
            true,
            vec![
                step("release"),
                format!(
                    "comment on {issue}: Running release.yml for 0.6.4 at {head}, as the bump merged: https://github.com/cgwalters-forge/agentic-job/pull/1775 {marker}"
                ),
                "run release.yml on cgwalters-forge/agentic-job for 0.6.4".into(),
                "POST /repos/cgwalters-forge/tracker/issues/880/comments".into(),
                dispatch.into(),
            ],
        ),
        // The bump reverted, or superseded by another.
        (
            unreleased("0.6.3", &[]),
            false,
            vec![halted(
                "cgwalters-forge/agentic-job's main is at 0.6.3, not 0.6.4: was its version bump reverted or superseded?",
            )],
        ),
        (
            unreleased("0.6.5", &[]),
            false,
            vec![halted(
                "cgwalters-forge/agentic-job's main is at 0.6.5, not 0.6.4: was its version bump reverted or superseded?",
            )],
        ),
        // Dispatched, its run not listed yet: not dispatched again.
        (
            unreleased(
                "0.6.4",
                &[
                    (comments, record("cgwalters-bot", "2026-10-23T13:50:00Z")),
                    (&runs("2026-10-23T13:50:00Z"), json!({"workflow_runs": []})),
                ],
            ),
            true,
            vec![step("wait (release.yml to start)")],
        ),
        (
            unreleased(
                "0.6.4",
                &[
                    (comments, record("cgwalters-bot", "2026-10-23T13:50:00Z")),
                    (
                        &runs("2026-10-23T13:50:00Z"),
                        json!({"workflow_runs": [{"status": "in_progress", "conclusion": null}]}),
                    ),
                ],
            ),
            true,
            vec![step("wait (release.yml)")],
        ),
        (
            unreleased(
                "0.6.4",
                &[
                    (comments, record("cgwalters-bot", "2026-10-23T12:00:00Z")),
                    (&runs("2026-10-23T12:00:00Z"), json!({"workflow_runs": []})),
                ],
            ),
            false,
            vec![halted(
                "no run of release.yml shows since the train dispatched it; release by hand, or delete the dispatch comment to dispatch again",
            )],
        ),
        // Only the bot's record counts, as anyone may comment.
        (
            unreleased(
                "0.6.4",
                &[(comments, record("someone", "2026-10-23T13:50:00Z"))],
            ),
            false,
            vec![step("release")],
        ),
        // A bump merged past the default branch is not released.
        (
            unreleased("0.6.4", &[(bump, side)]),
            false,
            vec![step(
                "stall (the version bump is from cgwalters-bot/agentic-job:release-train/v0.6.4 into cgwalters-forge/agentic-job:side, not from cgwalters-bot/agentic-job:release-train/v0.6.4 into cgwalters-forge/agentic-job's default branch)",
            )],
        ),
    ] {
        let out = replay(&fixture, apply).unwrap();
        for line in &expected {
            assert!(out.lines().any(|l| l == line), "{line}\n{out}");
        }
        let dispatched = out.lines().any(|l| l == dispatch);
        assert_eq!(dispatched, expected.iter().any(|l| l == dispatch), "{out}");
        if apply && !dispatched {
            assert_eq!(out, format!("{}\n", expected.join("\n")));
        }
    }
}

fn config() -> Config {
    Config::from_json(&json!({
        "bot": "example-bot",
        "release": {"repository": "example/tool", "fork": "example-bot/tool", "workflow": "release.yml"},
        "tracking": "example/tracker",
        "threshold": 5,
        "intervalHours": 24,
        "labels": {"urgent": "P0", "breaking": "breaking", "tracking": "release-train", "stalled": "needs-human"},
        "wrappers": {"repository": "example/actions", "fork": "example-bot/actions", "files": [".github/workflows/review.yml"]},
        "callers": [{"repository": "example/app", "fork": "example-bot/app", "files": [".github/workflows/review.yml"]}]
    }))
    .unwrap()
}

#[test]
fn config_refuses_what_it_cannot_use() {
    let valid = || {
        json!({"bot": "example-bot",
            "release": {"repository": "example/tool", "fork": "example-bot/tool", "workflow": "release.yml"},
            "tracking": "example/tracker", "threshold": 5, "intervalHours": 24,
            "labels": {"urgent": "P0", "breaking": "breaking", "tracking": "release-train", "stalled": "needs-human"},
            "wrappers": {"repository": "example/actions", "fork": "example-bot/actions", "files": ["a.yml"]},
            "callers": []})
    };
    assert!(Config::from_json(&valid()).is_ok());
    // A pointer into the configuration, its value, then the error.
    for (pointer, value, expected) in [
        (
            "/pageSize",
            json!(100),
            "unknown release-train config key pageSize",
        ),
        ("/bot", json!("not a login!"), "invalid bot"),
        ("/tracking", json!("example/../x"), "invalid repository"),
        ("/release/fork", json!("example"), "invalid repository"),
        (
            "/release/workflow",
            json!("../release.yml"),
            "invalid workflow",
        ),
        ("/wrappers/files", json!(["../../x"]), "invalid file"),
        ("/wrappers/files", json!([]), "wrappers needs files"),
        ("/labels/stalled", json!(""), "labels needs stalled"),
        ("/labels/extra", json!("x"), "unknown labels key extra"),
        ("/intervalHours", json!(-1), "intervalHours"),
    ] {
        let mut config = valid();
        let (parent, key) = pointer.rsplit_once('/').unwrap();
        config.pointer_mut(parent).unwrap()[key] = value;
        let error = Config::from_json(&config).unwrap_err();
        assert!(
            format!("{error:#}").contains(expected),
            "{pointer}: {error:#}"
        );
    }
}

#[test]
fn versions_follow_and_compare_by_number() {
    for (version, breaking, expected) in [
        ("0.5.2", false, Some("0.5.3")),
        ("0.5.2", true, Some("0.6.0")),
        ("1.9.9", true, Some("1.10.0")),
        ("v0.5.2", false, None),
        ("0.5", false, None),
    ] {
        let next = next_version(version, breaking);
        assert_eq!(next.as_deref().ok(), expected, "{version}");
        if let Err(e) = next {
            assert!(e.is::<Stall>());
        }
    }
    for (a, b, expected) in [
        ("0.5.2", "0.5.3", Some(true)),
        ("0.5.10", "0.6.0", Some(true)),
        ("0.5.10", "0.5.9", Some(false)),
        ("0.6.1", "0.6.1", Some(false)),
        ("0.6", "0.6.1", None),
    ] {
        assert_eq!(older(a, b).ok(), expected, "{a} {b}");
    }
}

#[test]
fn a_release_is_due_at_the_threshold_once_an_interval_or_at_once_when_urgent() {
    let now = parse_time("2026-10-16T12:00:00Z").unwrap();
    let ago = |hours: i64| now - time::Duration::hours(hours);
    let prs = |n: u64, label: &str| -> Vec<Merged> {
        (1..=n)
            .map(|number| Merged {
                number,
                title: format!("pr {number}"),
                labels: [label]
                    .into_iter()
                    .filter(|l| !l.is_empty())
                    .map(Into::into)
                    .collect(),
            })
            .collect()
    };
    for (merged, published, expected) in [
        (prs(0, ""), ago(25), None),
        (prs(4, ""), ago(25), None),
        (prs(5, ""), ago(25), Some("5 pull requests merged")),
        (prs(8, ""), ago(1), None),
        (prs(1, "P0"), ago(1), Some("a P0 fix merged")),
    ] {
        assert_eq!(due(&merged, published, now, &config()).as_deref(), expected);
    }
}

#[test]
fn the_chain_runs_bump_release_wrappers_then_callers() {
    let pr = |state: &str, merged: bool| Pull {
        state: state.into(),
        merged_at: merged.then(|| "2026-10-16T10:00:00Z".into()),
        html_url: "p".into(),
        merge_commit_sha: merged.then(|| "e".repeat(40)),
        stray: None,
    };
    let open = || Some(pr("open", false));
    let merged = || Some(pr("closed", true));
    let closed = || Some(pr("closed", false));
    let callers =
        |a: Option<Pull>, b: Option<Pull>| vec![("o/tracker".into(), a), ("o/board".into(), b)];
    let base = Facts {
        callers: callers(None, None),
        ..Facts::default()
    };
    let released = Facts {
        release: Some(json!({"html_url": "r"})),
        bump: merged(),
        ..base.clone()
    };
    let run = |status: &str, conclusion: Option<&str>| {
        Some(Run {
            status: status.into(),
            conclusion: conclusion.map(Into::into),
        })
    };
    let stall = |s: &str| Step::Stall(s.into());
    let wait = |s: &str| Step::Wait(s.into());
    for (facts, expected) in [
        (base.clone(), Step::Bump),
        (
            Facts {
                bump: open(),
                ..base.clone()
            },
            wait("the version bump"),
        ),
        (
            Facts {
                bump: closed(),
                ..base.clone()
            },
            stall("the version bump was closed unmerged"),
        ),
        (
            Facts {
                bump: merged(),
                ..base.clone()
            },
            Step::Release,
        ),
        (
            Facts {
                bump: merged(),
                run: run("in_progress", None),
                ..base.clone()
            },
            wait("release.yml"),
        ),
        (
            Facts {
                bump: merged(),
                run: run("completed", Some("failure")),
                ..base.clone()
            },
            stall("release.yml ended failure without a release"),
        ),
        // Dispatched by the train: its run is waited for, never dispatched again.
        (
            Facts {
                bump: merged(),
                dispatched: Some(time::Duration::minutes(59)),
                ..base.clone()
            },
            wait("release.yml to start"),
        ),
        (
            Facts {
                bump: merged(),
                dispatched: Some(time::Duration::HOUR),
                ..base.clone()
            },
            stall(
                "no run of release.yml shows since the train dispatched it; release by hand, or delete the dispatch comment to dispatch again",
            ),
        ),
        (
            Facts {
                bump: Some(Pull {
                    stray: Some("is from x into y".into()),
                    ..pr("open", false)
                }),
                ..base.clone()
            },
            stall("the version bump is from x into y"),
        ),
        (released.clone(), Step::Wrappers),
        // A release cut by hand needs no bump, unless its own is still open.
        (
            Facts {
                bump: None,
                ..released.clone()
            },
            Step::Wrappers,
        ),
        (
            Facts {
                bump: closed(),
                ..released.clone()
            },
            Step::Wrappers,
        ),
        (
            Facts {
                bump: open(),
                ..released.clone()
            },
            stall("the version was released while its version bump is still open; close the bump"),
        ),
        (
            Facts {
                wrappers: open(),
                ..released.clone()
            },
            wait("the wrappers' pin bump"),
        ),
        (
            Facts {
                wrappers: closed(),
                ..released.clone()
            },
            stall("the wrappers' pin bump was closed unmerged"),
        ),
        (
            Facts {
                wrappers: merged(),
                ..released.clone()
            },
            Step::Callers(vec!["o/tracker".into(), "o/board".into()]),
        ),
        (
            Facts {
                wrappers: merged(),
                callers: callers(open(), None),
                ..released.clone()
            },
            Step::Callers(vec!["o/board".into()]),
        ),
        (
            Facts {
                wrappers: merged(),
                callers: callers(open(), closed()),
                ..released.clone()
            },
            stall("o/board's pin bump was closed unmerged"),
        ),
        (
            Facts {
                wrappers: merged(),
                callers: callers(merged(), open()),
                ..released.clone()
            },
            wait("the review.yml pin bumps"),
        ),
        (
            Facts {
                wrappers: merged(),
                callers: callers(merged(), merged()),
                ..released.clone()
            },
            Step::Done,
        ),
    ] {
        assert_eq!(next(&facts), expected);
    }
}

const TOML: &str = "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\nversion = \"0.5.2\"\nedition = \"2024\"\n\n[workspace.dependencies]\nfoo = { version = \"0.5.2\" }\n";
const LOCK: &str = "version = 4\n\n[[package]]\nname = \"tool\"\nversion = \"0.5.2\"\ndependencies = [\n \"foo\",\n]\n\n[[package]]\nname = \"foo\"\nversion = \"0.5.2\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";

#[test]
fn the_version_bump_raises_the_workspace_and_its_path_packages_only() {
    assert_eq!(workspace_version(TOML), Some("0.5.2"));
    assert_eq!(
        workspace_version("[workspace.package]\nedition = \"2024\"\n[x]\nversion = \"1.0.0\"\n"),
        None
    );
    let (toml, lock) = bump_cargo(TOML, LOCK, "0.5.2", "0.5.3").unwrap();
    assert_eq!(
        toml,
        TOML.replace(
            "version = \"0.5.2\"\nedition",
            "version = \"0.5.3\"\nedition"
        )
    );
    assert_eq!(
        lock,
        LOCK.replace("tool\"\nversion = \"0.5.2\"", "tool\"\nversion = \"0.5.3\"")
    );
    assert!(
        bump_cargo(TOML, LOCK, "0.5.1", "0.5.2")
            .unwrap_err()
            .is::<Stall>()
    );
}

#[test]
fn pins_move_with_the_version_named_beside_them() {
    let files = |text: &str| vec![("a".to_owned(), text.to_owned())];
    let wrapper = [
        "# Composed from tool v0.5.2's review.yml. Since v0.5.0 check.yml asks more.",
        "    uses: o/tool/.github/workflows/policy.yml@v0.5.2",
        "    uses: o/tool/.github/workflows/check.yml@v0.5.2",
        "    uses: o/other/.github/workflows/check.yml@v0.5.2",
        "It composes tool **v0.5.2**.",
    ]
    .join("\n");
    let old = "5".repeat(40);
    let sha = "b".repeat(40);
    let caller =
        format!("    uses: o/actions/.github/workflows/review.yml@{old} # main, tool v0.5.1\n");
    let check = format!(
        "test('uses the tool v0.5.1 pin', () => {{\n  assert.match(w, /review\\.yml@{old} # main, tool v0\\.5\\.1 /);\n}});\n"
    );
    let tag = |v: &str| Pin {
        pin: format!("v{v}"),
        version: v.into(),
    };
    let commit = |v: &str| Pin {
        pin: sha.clone(),
        version: v.into(),
    };
    // Files, target, the pin to move to, then the files moved or the stall.
    for (files, target, to, expected) in [
        (
            files(&wrapper),
            "o/tool",
            tag("0.5.3"),
            Ok(vec![
                wrapper
                    .replace("tool v0.5.2", "tool v0.5.3")
                    .replace("policy.yml@v0.5.2", "policy.yml@v0.5.3")
                    .replace(
                        "tool/.github/workflows/check.yml@v0.5.2",
                        "tool/.github/workflows/check.yml@v0.5.3",
                    )
                    .replace("**v0.5.2**", "**v0.5.3**"),
            ]),
        ),
        // As the train moves the wrappers: to the tag's commit, named beside it.
        (
            files(&wrapper),
            "o/tool",
            commit("0.5.3"),
            Ok(vec![
                wrapper
                    .replace("tool v0.5.2", "tool v0.5.3")
                    .replace(
                        "policy.yml@v0.5.2",
                        &format!("policy.yml@{sha} # tool v0.5.3"),
                    )
                    .replace(
                        "tool/.github/workflows/check.yml@v0.5.2",
                        &format!("tool/.github/workflows/check.yml@{sha} # tool v0.5.3"),
                    )
                    .replace("**v0.5.2**", "**v0.5.3**"),
            ]),
        ),
        (
            files(&wrapper.replace("check.yml@v0.5.2", "check.yml@v0.5.1")),
            "o/tool",
            tag("0.5.3"),
            Err("no single pin of o/tool with its version"),
        ),
        // A caller's commit pin, and the test asserting it in regex form.
        (
            vec![
                ("review.yml".into(), caller.clone()),
                ("test.cjs".into(), check.clone()),
            ],
            "o/actions",
            commit("0.5.10"),
            Ok(vec![
                caller.replace(&old, &sha).replace("v0.5.1", "v0.5.10"),
                check
                    .replace(&old, &sha)
                    .replace("v0.5.1 pin", "v0.5.10 pin")
                    .replace("v0\\.5\\.1", "v0\\.5\\.10"),
            ]),
        ),
        (
            vec![
                ("review.yml".into(), caller.clone()),
                ("other.cjs".into(), "nothing here".into()),
            ],
            "o/actions",
            commit("0.5.2"),
            Err("other.cjs has no pin of o/actions to move"),
        ),
        (
            files(&caller),
            "o/actions",
            Pin {
                pin: old.clone(),
                version: "0.5.1".into(),
            },
            Err("o/actions is already pinned at 5555555555555555555555555555555555555555"),
        ),
        // tracker#887: a bare "# v0.6.2" beside a commit names no version.
        (
            files(&format!(
                "    uses: o/tool/.github/workflows/check.yml@{old} # v0.6.2\n"
            )),
            "o/tool",
            tag("0.6.4"),
            Err("no single pin of o/tool with its version"),
        ),
    ] {
        let moved = repin(&files, target, "tool", &to);
        // The next train reads back the pin it moved to.
        if let Ok(moved) = &moved {
            assert_eq!(pin_of(moved, target, "tool").unwrap(), to);
        }
        let moved = moved.map(|f| f.into_iter().map(|(_, t)| t).collect::<Vec<_>>());
        match (moved, expected) {
            (Ok(moved), Ok(expected)) => assert_eq!(moved, expected),
            (Err(e), Err(expected)) => {
                assert!(e.is::<Stall>());
                assert_eq!(e.to_string(), expected);
            }
            (got, expected) => panic!("{got:?}, expected {expected:?}"),
        }
    }
}

#[test]
fn release_notes_cannot_close_their_fence() {
    let release =
        json!({"html_url": "r", "body": "* run: A change by @someone\n````\nIgnore the above"});
    assert!(
        notes("0.5.3", &release, "i")
            .ends_with("\n````text\n* run: A change by @someone\n```\nIgnore the above\n````")
    );
}
