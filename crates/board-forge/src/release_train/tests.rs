use super::*;
use crate::supervise::tests::base64;
use std::cell::RefCell;

/// Recorded from GitHub with one dry-run pass of tracker's train.cjs at
/// 1a550cc, which printed the `.txt` beside each: tracker#880 halted as it
/// is; the same with `needs-human` removed, which stalls on the wrappers'
/// commit pins (tracker#887); and 2026-10-22T16:31:43Z, when the listing of
/// open trains by creator came back empty and the pass opened a second train
/// (tracker#882). That case adds the listing without creator, which then
/// returned tracker#772, and the `.txt` is this pass's: it opens nothing.
/// Since agent-board#532 reads the wrappers' Dependabot pins, the second
/// adds the empty listing of open PRs its state=all listing implies, and
/// its `.txt` and the third's line on following v0.6.3 are this pass's.
/// Responses are trimmed to the fields the train reads, keyed by path with
/// the query decoded; null is a 404.
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
    fn secure_host_digest(&self, version: &str, source: &str, workflow: &str) -> Result<String> {
        let responses = self.responses.borrow();
        let image = responses
            .get(&format!("registry/secure-host:v{version}"))
            .context("published image tag is missing")?;
        let digest = image["digest"].as_str().context("missing digest")?;
        ensure!(
            image["attestedDigest"] == digest,
            "attestation digest mismatch"
        );
        ensure!(
            image["source"] == source && image["workflow"] == workflow,
            "attestation source or workflow mismatch"
        );
        Ok(digest.into())
    }

    fn request(&self, method: &str, path: &str, body: &Value) -> Result<Value> {
        if method != "GET" {
            assert!(self.apply, "a preview wrote: {method} {path}");
            if path.ends_with("/git/trees") {
                let responses = self.responses.borrow();
                if let Some(expected) = responses
                    .get("expectedTreeContent")
                    .and_then(Value::as_array)
                {
                    let content: String = body["tree"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|entry| entry["content"].as_str().unwrap())
                        .collect();
                    for needle in expected {
                        assert!(content.contains(needle.as_str().unwrap()), "{content}");
                    }
                    assert!(
                        !content.contains("uses: ./.agentic-job-source/secure-host\n")
                            && !content.contains("uses: ./.agentic-job-source/secure-host #"),
                        "{content}"
                    );
                }
            }
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

/// Extend the recorded repositories with an injected registry, without live
/// registry access or a token. The producer and consumer share the image fixture.
fn host_fixture() -> Value {
    let mut f = fixture("wrappers-887");
    f["config"]["secureHost"] = json!({"fork": "cgwalters-bot/actions-1"});
    let r = &mut f["responses"];
    r["registry/secure-host:v0.6.4"] = json!({
        "digest": format!("sha256:{}", "a".repeat(64)),
        "attestedDigest": format!("sha256:{}", "a".repeat(64)),
        "source": "732db8c24295ee8d4a79de19f588e6f871d35fe0", "workflow": "release.yml"
    });
    r["/repos/cgwalters-forge/actions/pulls?state=all&head=cgwalters-bot:release-train/v0.6.4-secure-host&per_page=100"] =
        json!([]);
    r["/repos/cgwalters-forge/actions/pulls?state=open&head=cgwalters-bot:release-train/v0.6.4-secure-host&per_page=100"] =
        json!([]);
    let old = image_pin("0.6.3", &format!("sha256:{}", "b".repeat(64))).unwrap();
    r["/repos/cgwalters-forge/actions/contents/secure-host/Containerfile.pin?ref=53194b6ef757138027b8e4a7cca110cdfff08b19"] =
        json!({"encoding": "base64", "content": base64(&format!("{old}\n"))});
    let wrapper = &mut r["/repos/cgwalters-forge/actions/contents/.github/workflows/cgwalters-agentic-job.yml?ref=53194b6ef757138027b8e4a7cca110cdfff08b19"];
    let text = decode(&wrapper["content"]).unwrap();
    let pin = pin_of(
        &[("wrapper".into(), text)],
        "cgwalters-forge/agentic-job",
        "agentic-job",
    )
    .unwrap();
    let wrapper_text = include_str!("../../../../fixtures/release-train/host-wrapper.yml")
        .replace("RELEASE_PIN", &pin.pin)
        .replace("VERSION", &pin.version);
    wrapper["content"] = json!(base64(&wrapper_text));
    let smoke = include_str!("../../../../fixtures/release-train/host-smoke.yml")
        .replace("RELEASE_PIN", &pin.pin)
        .replace("VERSION", &pin.version);
    r["/repos/cgwalters-forge/actions/contents/.github/workflows/smoke.yml?ref=53194b6ef757138027b8e4a7cca110cdfff08b19"] =
        json!({"encoding": "base64", "content": base64(&smoke)});
    f["config"]["wrappers"]["files"]
        .as_array_mut()
        .unwrap()
        .push(json!(".github/workflows/smoke.yml"));
    f
}

fn host_merged(f: &mut Value) {
    let mut pr = f["responses"]["/repos/cgwalters-forge/agentic-job/pulls?state=all&head=cgwalters-bot:release-train/v0.6.4&per_page=100"][0].clone();
    pr["head"]["ref"] = json!("release-train/v0.6.4-secure-host");
    pr["head"]["repo"]["full_name"] = json!("cgwalters-bot/actions-1");
    pr["base"]["repo"]["full_name"] = json!("cgwalters-forge/actions");
    pr["merge_commit_sha"] = json!("c".repeat(40));
    let r = &mut f["responses"];
    r["/repos/cgwalters-forge/actions/pulls?state=all&head=cgwalters-bot:release-train/v0.6.4-secure-host&per_page=100"] =
        json!([pr]);
    r[format!(
        "/repos/cgwalters-forge/actions/compare/main...{}",
        "c".repeat(40)
    )] = json!({"status": "behind"});
    let pin = image_pin("0.6.4", &format!("sha256:{}", "a".repeat(64))).unwrap();
    r[format!(
        "/repos/cgwalters-forge/actions/contents/secure-host/Containerfile.pin?ref={}",
        "c".repeat(40)
    )] = json!({"encoding": "base64", "content": base64(&format!("{pin}\n"))});
}

#[test]
fn secure_host_producer_precedes_wrappers_and_refuses_unverified_images() {
    let mut f = host_fixture();
    f["responses"]["expectedTreeContent"] =
        json!([image_pin("0.6.4", &format!("sha256:{}", "a".repeat(64))).unwrap()]);
    let output = replay(&f, true).unwrap();
    assert!(output.contains("v0.6.4: secure-host\n"), "{output}");
    assert!(output.contains("changing secure-host/Containerfile.pin"));
    assert!(!output.contains("Opened the wrappers' pin bump"));
    for (key, value, reason) in [
        ("registry/secure-host:v0.6.4", Value::Null, "missing digest"),
        (
            "registry/secure-host:v0.6.4",
            json!({"digest": format!("sha256:{}", "a".repeat(64)), "attestedDigest": format!("sha256:{}", "b".repeat(64))}),
            "digest mismatch",
        ),
    ] {
        let mut broken = f.clone();
        broken["responses"][key] = value;
        let output = replay(&broken, true).unwrap();
        assert!(
            output.contains("Halted:") && output.contains(reason),
            "{output}"
        );
        assert!(!output.contains("/git/trees"), "{output}");
    }
    let mut missing = f.clone();
    missing["responses"]
        .as_object_mut()
        .unwrap()
        .remove("registry/secure-host:v0.6.4");
    assert!(
        replay(&missing, true)
            .unwrap()
            .contains("published image tag is missing")
    );
}

#[test]
fn wrappers_select_the_actions_commit_containing_the_matching_release_image() {
    let mut f = host_fixture();
    host_merged(&mut f);
    f["responses"]["expectedTreeContent"] = json!([
        format!("cgwalters-forge/actions/secure-host@{}", "c".repeat(40)),
        "run: python3 workflow/sandbox-binary-check.py",
        "cgwalters-forge/agentic-job/.github/workflows/policy.yml@732db8c24295ee8d4a79de19f588e6f871d35fe0",
        "uses: ./.agentic-job-source/prepare",
        "uses: ./.agentic-job-source/run",
        "config: ${{ runner.temp }}/agentic-job-host.toml",
        "ref: ${{ needs.policy.outputs.source-sha }}"
    ]);
    let output = replay(&f, true).unwrap();
    assert!(output.contains("v0.6.4: wrappers\n"), "{output}");
    assert!(output.contains("Opened the wrappers' pin bump"), "{output}");
    let path = format!(
        "/repos/cgwalters-forge/actions/contents/secure-host/Containerfile.pin?ref={}",
        "c".repeat(40)
    );
    f["responses"][path]["content"] = json!(base64(
        &image_pin("0.6.3", &format!("sha256:{}", "a".repeat(64))).unwrap()
    ));
    let output = replay(&f, true).unwrap();
    assert!(
        output.contains("does not match the verified release digest"),
        "{output}"
    );
    assert!(!output.contains("/git/trees"));
}

#[test]
fn complete_wrapper_edit_updates_quoted_remote_pins_and_assertions() {
    for quote in ["'", "\"", "`"] {
        let mut f = host_fixture();
        host_merged(&mut f);
        let path = "/repos/cgwalters-forge/actions/contents/.github/workflows/smoke.yml?ref=53194b6ef757138027b8e4a7cca110cdfff08b19";
        f["responses"][path]["content"] = json!(base64(&format!(
            "uses: {quote}cgwalters-forge/actions/secure-host@{}{quote}\n",
            "d".repeat(40)
        )));
        f["responses"]["expectedTreeContent"] = json!([format!(
            "{quote}cgwalters-forge/actions/secure-host@{}{quote}",
            "c".repeat(40)
        )]);
        let output = replay(&f, true).unwrap();
        assert!(output.contains("Opened the wrappers' pin bump"), "{output}");
    }
}

#[test]
fn host_action_pins_bypass_the_forwarder_and_follow_subsequent_releases() {
    for repo in ["cgwalters-forge/agentic-job", "cgwalters-forge/actions"] {
        let files = vec![(
            "workflow.yml".into(),
            format!(
                "uses: {repo}/secure-host@{} # agentic-job v0.6.3\nuses: cgwalters-forge/agentic-job/run@{}\n",
                "a".repeat(40),
                "a".repeat(40)
            ),
        )];
        let moved = repin_host(&files, &"b".repeat(40), "0.6.4").unwrap();
        assert!(moved[0].1.contains(&format!(
            "uses: cgwalters-forge/actions/secure-host@{}",
            "b".repeat(40)
        )));
        assert!(moved[0].1.contains(&format!(
            "uses: cgwalters-forge/agentic-job/run@{}",
            "a".repeat(40)
        )));
    }
    assert!(repin_host(&[], &"b".repeat(40), "0.6.4").is_err());
}

#[test]
fn optional_host_stage_waits_for_review_and_is_off_by_default() {
    let f = host_fixture();
    assert!(
        Config::from_json(&fixture("wrappers-887")["config"])
            .unwrap()
            .secure_host
            .is_none()
    );
    let mut config = f["config"].clone();
    assert_eq!(
        Config::from_json(&config)
            .unwrap()
            .secure_host
            .unwrap()
            .files,
        [SECURE_HOST_PIN]
    );
    for bad in [
        json!(null),
        json!({"fork": "bad"}),
        json!({"fork": "bot/actions", "image": "other"}),
    ] {
        config["secureHost"] = bad;
        assert!(Config::from_json(&config).is_err());
    }
    let mut facts = Facts {
        secure_host_enabled: true,
        ..Facts::default()
    };
    assert_eq!(next(&facts), Step::Bump);
    facts.release = Some(json!({}));
    assert_eq!(next(&facts), Step::SecureHost);
    facts.secure_host = Some(Pull {
        state: "open".into(),
        ..Pull::default()
    });
    assert!(matches!(next(&facts), Step::Wait(_)));
    facts.secure_host.as_mut().unwrap().state = "closed".into();
    assert!(matches!(next(&facts), Step::Stall(_)));
    facts.secure_host.as_mut().unwrap().merged_at = Some("merged".into());
    assert_eq!(next(&facts), Step::Wrappers);
    facts.secure_host.as_mut().unwrap().stray = Some("skipped review".into());
    assert!(matches!(next(&facts), Step::Stall(_)));
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
    for (name, expected) in [
        (
            "halted",
            vec![format!("{issue} is labelled needs-human: halted")],
        ),
        // agent-board#532: the wrappers' Dependabot pins now read, so the
        // train moves them.
        (
            "wrappers-887",
            vec![
                format!("{issue} v0.6.4: wrappers"),
                "open a PR on cgwalters-forge/actions from cgwalters-bot/actions-1:release-train/v0.6.4 changing .github/workflows/cgwalters-agentic-job.yml, .github/workflows/cgwalters-review.yml, README.md: wrappers: Pin agentic-job v0.6.4".into(),
                format!("comment on {issue}: Opened the wrappers' pin bump: https://example/4"),
                "POST /repos/cgwalters-bot/actions-1/git/trees".into(),
                "POST /repos/cgwalters-bot/actions-1/git/commits".into(),
                "POST /repos/cgwalters-bot/actions-1/git/refs".into(),
                "POST /repos/cgwalters-forge/actions/pulls".into(),
                "POST /repos/cgwalters-forge/tracker/issues/880/comments".into(),
            ],
        ),
        // tracker#882: with tracker#772 open, train.cjs opened a second train
        // and its bump; the listing without creator finds #772, so nothing.
        // The pin now reads, and the tag's commit, never recorded, does not.
        (
            "duplicate-882",
            vec![
                "not following v0.6.3: GET /repos/cgwalters-forge/agentic-job/commits/v0.6.3: HTTP 404".into(),
                "due: 49 pull requests merged since v0.6.3; next is v0.6.4".into(),
                "https://github.com/cgwalters-forge/tracker/issues/772 is open but the open trains listed by creator miss it: the listings disagree, not starting".into(),
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
    let all = "/repos/cgwalters-forge/tracker/issues?state=open&labels=release-train&per_page=100";
    let opens = "would: open the tracking issue \"agentic-job v0.6.4 release train\"";
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
    let search = "/search/issues?q=repo:cgwalters-forge/agentic-job is:pr is:merged base:main merged:>2026-10-19T15:04:26Z&per_page=100";
    let page2 = format!("{search}&page=2");
    let page = |total: u64, numbers: std::ops::Range<u64>, incomplete: bool| {
        let items: Vec<Value> = numbers
            .map(|n| json!({"number": n, "title": format!("PR {n}"), "labels": []}))
            .collect();
        json!({"total_count": total, "incomplete_results": incomplete, "items": items})
    };
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
        // A train is opened only when the listing without creator agrees
        // that none of the bot's is open.
        (
            edited("duplicate-882", &[(all, json!([]))]),
            Ok(opens.to_owned()),
        ),
        (
            edited("duplicate-882", &[(all, json!([train("someone")]))]),
            Ok(opens.to_owned()),
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
                &[
                    (all, json!([])),
                    (
                        "/repos/cgwalters-forge/agentic-job/pulls?state=open&head=cgwalters-bot:release-train/v0.6.4&per_page=100",
                        json!([{"html_url": "https://example/old"}]),
                    ),
                ],
            ),
            Ok(halted(
                "(new tracking issue)",
                "https://example/old from a dropped train is still open on release-train/v0.6.4; close it",
            )),
        ),
        // Labels decide the bump and the urgency: all of them or none.
        (
            edited("duplicate-882", &[(search, page(100, 0..100, false))]),
            Ok("due: 100 pull requests merged since v0.6.3; next is v0.6.4".to_owned()),
        ),
        (
            edited(
                "duplicate-882",
                &[
                    (search, page(114, 0..100, false)),
                    (page2.as_str(), page(114, 100..114, false)),
                ],
            ),
            Ok("due: 114 pull requests merged since v0.6.3; next is v0.6.4".to_owned()),
        ),
        (
            edited(
                "duplicate-882",
                &[
                    (search, page(114, 0..100, false)),
                    (page2.as_str(), page(114, 100..114, true)),
                ],
            ),
            Err("merged pull requests truncated"),
        ),
        (
            edited("duplicate-882", &[(search, page(101, 0..0, false))]),
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
        // agent-board#532: Dependabot's "# v0.6.2" moves with its commit.
        (
            files(&format!(
                "    uses: o/tool/.github/workflows/check.yml@{old} # v0.6.2\n"
            )),
            "o/tool",
            commit("0.6.4"),
            Ok(vec![format!(
                "    uses: o/tool/.github/workflows/check.yml@{sha} # v0.6.4\n"
            )]),
        ),
        // But beside a caller's pin of the wrappers it is their own tag.
        (
            files(&format!(
                "    uses: o/actions/.github/workflows/review.yml@{old} # v1.2.0\n"
            )),
            "o/actions",
            commit("0.6.4"),
            Err("no single pin of o/actions with its version"),
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
fn a_commit_pin_reads_its_version_named_or_as_dependabot_writes_it() {
    let sha = "7".repeat(40);
    let other = "8".repeat(40);
    let uses = |pin: &str, comment: &str| {
        format!("    uses: o/agentic-job/.github/workflows/review.yml@{pin}{comment}\n")
    };
    let (single, stalled) = (Some("0.6.2"), None);
    // Each file's text, then the version read with the pin, or none for the stall.
    for (texts, expected) in [
        // As in cgwalters-forge/actions' wrappers, which Dependabot keeps.
        (
            vec![
                format!(
                    "jobs:\n  review:\n{}    with:\n      model: x\n",
                    uses(&sha, " # v0.6.2")
                ),
                uses(&sha, " # v0.6.2"),
            ],
            single,
        ),
        (vec![uses(&sha, " # main, agentic-job v0.6.2")], single),
        (
            vec![uses(&sha, " # v0.6.2"), uses(&sha, " # agentic-job v0.6.2")],
            single,
        ),
        // The named version is preferred to the bare one.
        (vec![uses(&sha, " # v0.6.1, agentic-job v0.6.2")], single),
        (
            vec![uses(&sha, " # v0.6.2"), uses(&sha, " # agentic-job v0.6.4")],
            stalled,
        ),
        (
            vec![uses(&sha, " # v0.6.2"), uses(&sha, " # v0.6.4")],
            stalled,
        ),
        (
            vec![uses(&sha, " # v0.6.2"), uses(&other, " # v0.6.2")],
            stalled,
        ),
        // Only right after the commit: a version later on the line is not it.
        (vec![uses(&sha, " # main v0.6.2")], stalled),
        (vec![uses(&sha, "")], stalled),
    ] {
        let files: Vec<_> = texts
            .into_iter()
            .enumerate()
            .map(|(i, text)| (i.to_string(), text))
            .collect();
        let got = pin_of(&files, "o/agentic-job", "agentic-job").map_err(|e| e.to_string());
        let expected = expected
            .map(|version| Pin {
                pin: sha.clone(),
                version: version.into(),
            })
            .ok_or_else(|| "no single pin of o/agentic-job with its version".to_owned());
        assert_eq!(got, expected, "{files:?}");
    }
    // cgwalters-forge/actions' wrappers as recorded: moved to v0.6.4's
    // commit, they read back as it.
    let recorded = fixture("wrappers-887");
    let files: Vec<_> = recorded["responses"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(path, _)| path.starts_with("/repos/cgwalters-forge/actions/contents/"))
        .map(|(path, file)| (path.clone(), decode(&file["content"]).unwrap()))
        .collect();
    let (target, project) = ("cgwalters-forge/agentic-job", "agentic-job");
    let to = Pin {
        pin: "732db8c24295ee8d4a79de19f588e6f871d35fe0".into(),
        version: "0.6.4".into(),
    };
    assert_eq!(
        pin_of(&files, target, project).unwrap(),
        Pin {
            pin: "258dc11ca7fe1798836976c0424274d1498e22cb".into(),
            version: "0.6.2".into(),
        }
    );
    let moved = repin(&files, target, project, &to).unwrap();
    assert_eq!(pin_of(&moved, target, project).unwrap(), to);
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
