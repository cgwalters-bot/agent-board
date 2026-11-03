// These scenarios use the same transport and claim protocol as legacy review
// tests, with an empty local cache to model resuming after a pass outage.
fn ordinary_fixture(author: &str, association: &str, bot: bool) -> (Fake, Config) {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let mut fake = github();
    fake.page(
        &format!("repos/{REPO}/pulls?state=open&per_page=100"),
        None,
        None,
        json!([{"number": 470, "user": {"login": author}, "labels": []}]),
    );
    let mut p = pull("pull-470", json!({"reviews": {"nodes": []}}));
    p["author"] = json!({"login": author.trim_end_matches("[bot]"), "__typename": if bot {"Bot"} else {"User"}});
    if author != BOT {
        p["isCrossRepository"] = false.into();
    }
    fake.pulls.insert(470, p);
    fake.page(&format!("repos/{REPO}/pulls/470"), None, None,
        json!({"number": 470, "state": "open", "draft": false, "head": {"sha": HEAD},
            "author_association": association, "user": {"login": author, "type": if bot {"Bot"} else {"User"}, "id": 49699333}}));
    for (file, trigger) in [
        (
            "review.toml",
            "events = [\"pull_request_target\"]\nroles = [\"all\"]\nbots = [\"dependabot[bot]\"]",
        ),
        (
            "review-command.toml",
            "events = [\"issue_comment\"]\nroles = [\"write\", \"maintain\", \"admin\"]\ncommands = [\"/review\"]\nbots = []",
        ),
    ] {
        let policy = format!(
            "hosts = [\"github.com\"]\nrepos = [\"{REPO}\"]\nbases = [\"main\"]\n[github.trigger]\nforks = true\n{trigger}\n"
        );
        fake.page(&format!("repos/{REPO}/contents/workflow/{file}?ref=main"), None, None,
            json!({"type": "file", "size": policy.len(), "encoding": "base64", "content": STANDARD.encode(policy)}));
    }
    fake.page(
        &format!("repos/{REPO}/actions/variables/REVIEW_BOT_IDS"),
        None,
        None,
        json!({"value": "[49699333]"}),
    );
    ordinary_comments(&mut fake, vec![]);
    ordinary_runs(&mut fake, vec![]);
    let mut cfg = config();
    cfg.policy.review_dispatch_repositories = vec![REPO.into()];
    cfg.policy.review_pools = vec!["claude".into()];
    cfg.policy.reruns = true;
    (fake, cfg)
}

fn ordinary_comments(fake: &mut Fake, comments: Vec<Value>) {
    fake.page(
        &format!("repos/{REPO}/issues/470/comments?per_page=100"),
        None,
        None,
        json!(comments),
    );
}

fn ordinary_runs(fake: &mut Fake, runs: Vec<Value>) {
    let since = (now() - time::Duration::days(1)).date();
    fake.page(&format!("repos/{REPO}/actions/workflows/dispatch.yml/runs?event=workflow_dispatch&created=%3E%3D{since}&per_page={REVIEW_DISPATCH_READS}"),
        None, None, json!({"workflow_runs": runs}));
}

fn ordinary_run() -> Value {
    json!({"id": REVIEW_RUN, "path": ".github/workflows/dispatch.yml", "event": "workflow_dispatch",
        "head_sha": "a".repeat(40), "head_branch": "main", "display_title": format!("review {REPO}#470 {HEAD} (claude)"),
        "status": "completed", "conclusion": "failure", "run_attempt": 1,
        "created_at": "2026-10-12T02:00:01Z", "run_started_at": "2026-10-12T02:00:01Z", "updated_at": "2026-10-12T02:01:00Z",
        "repository": {"full_name": REPO}})
}

fn ordinary_pass(fake: &Fake, cfg: &Config, apply: bool, at: OffsetDateTime) -> Outcome {
    let dispatcher = Fake::default();
    run_capped(
        fake,
        fake,
        cfg,
        State::default(),
        (&[], &[REPO.to_owned()]),
        if apply { applying(&dispatcher) } else { None },
        None,
        at,
    )
    .unwrap()
}

fn ordinary_receipts(fake: &Fake) -> Vec<Value> {
    fake.writes.borrow().iter().filter(|(url, _)| url.contains("/issues/comments/")).map(|(_, body)| {
        json!({"id": 99, "user": {"login": BOT}, "body": body["body"], "created_at": "2026-10-12T02:00:00Z"})
    }).collect()
}

#[test]
fn ordinary_review_command_history_has_bounded_permission_reads() {
    for consumed in [false, true] {
        let (mut fake, cfg) = ordinary_fixture("cgwalters", "OWNER", false);
        let mut comments = Vec::new();
        for id in 1..=501 {
            comments.push(json!({"id": id, "user": {"login": "reviewer", "type": "User"},
                "body": "/review", "created_at": "2026-10-01T00:00:00Z"}));
            if consumed {
                let state = json!({"review_head": format!("{HEAD}/{id}"), "review_at": "2026-10-01T00:01:00Z"});
                comments.push(json!({"id": 1000 + id, "user": {"login": BOT},
                    "body": format!("<!-- {MARKER}: {state} -->")}));
            }
        }
        if consumed {
            comments.push(json!({"id": 502, "user": {"login": "reviewer", "type": "User"},
                "body": "/review", "created_at": "2026-10-12T01:00:00Z"}));
        }
        ordinary_comments(&mut fake, comments);
        fake.page(&format!("repos/{REPO}/collaborators/reviewer/permission"), None, None,
            json!({"permission": if consumed {"write"} else {"read"}}));
        let outcome = ordinary_pass(&fake, &cfg, true, now());
        assert_eq!(outcome.errors, 0, "{consumed}: {:?}", outcome.lines);
        assert_eq!(outcome.dispatched, 1, "{consumed}: {:?}", outcome.lines);
        assert_eq!(fake.sent.borrow().iter().filter(|(url, _)| url.ends_with("/collaborators/reviewer/permission")).count(), 1);
    }
}

#[test]
fn ordinary_review_base_policy_is_case_sensitive() {
    let (mut fake, cfg) = ordinary_fixture("cgwalters", "OWNER", false);
    fake.pulls.get_mut(&470).unwrap()["baseRefName"] = "Main".into();
    let outcome = ordinary_pass(&fake, &cfg, true, now());
    assert_eq!(outcome.errors, 0, "{:?}", outcome.lines);
    assert_eq!(outcome.dispatched, 0);
    assert!(fake.writes.borrow().is_empty());
}

#[test]
fn ordinary_review_author_admission_is_not_merge_authority() {
    for (author, association, bot, admitted) in [
        (BOT, "COLLABORATOR", false, true),
        ("dependabot[bot]", "NONE", true, true),
        ("cgwalters", "OWNER", false, true),
        ("collaborator", "COLLABORATOR", false, true),
        ("visitor", "NONE", false, false),
        ("random[bot]", "NONE", true, false),
    ] {
        let (fake, cfg) = ordinary_fixture(author, association, bot);
        let outcome = ordinary_pass(&fake, &cfg, true, now());
        assert_eq!(outcome.errors, 0, "{author}: {:?}", outcome.lines);
        assert_eq!(
            outcome.dispatched,
            usize::from(admitted),
            "{author}: {:?}",
            outcome.lines
        );
        assert_eq!(outcome.merged, 0, "{author}");
        if admitted {
            let writes = fake.writes.borrow();
            assert_eq!(writes.len(), 3, "{author}: {writes:?}");
            assert!(writes[2].0.ends_with(&format!(
                "repos/{REPO}/actions/workflows/dispatch.yml/dispatches"
            )));
            assert_eq!(
                writes[2].1["inputs"],
                json!({"kind": "review", "repo": REPO, "item": "470", "head": HEAD, "pool": "claude",
                "task": "Review the pinned pull request head using the base-owned review task and bounds."})
            );
            assert_eq!(writes[2].1["ref"], "main");
        }
    }
    for change in [
        json!({"isDraft": true}),
        json!({"baseRepository": {"isPrivate": true}}),
        json!({"labels": {"nodes": [{"name": HOLD_LABEL}]}}),
    ] {
        let (mut fake, cfg) = ordinary_fixture("cgwalters", "OWNER", false);
        for (key, value) in change.as_object().unwrap() {
            fake.pulls.get_mut(&470).unwrap()[key] = value.clone();
        }
        let outcome = ordinary_pass(&fake, &cfg, false, now());
        assert_eq!(outcome.dispatched, 0, "{:?}", outcome.lines);
        assert!(fake.writes.borrow().is_empty());
    }
}

#[test]
fn ordinary_review_duplicate_push_retry_and_lost_response() {
    for lost in [false, true] {
        let (mut fake, cfg) = ordinary_fixture("collaborator", "COLLABORATOR", false);
        if lost {
            fake.answers.borrow_mut().insert(
                format!("{API_ROOT}repos/{REPO}/actions/workflows/dispatch.yml/dispatches"),
                vec![(500, json!({"message": "response lost"}))],
            );
        }
        let first = ordinary_pass(&fake, &cfg, true, now());
        assert_eq!(first.dispatched, 1, "{:?}", first.lines);
        assert_eq!(first.errors, usize::from(lost));
        let receipts = ordinary_receipts(&fake);
        ordinary_comments(&mut fake, receipts);
        fake.writes.borrow_mut().clear();
        let second = ordinary_pass(&fake, &cfg, true, now() + time::Duration::minutes(1));
        assert_eq!(second.errors, 0, "{:?}", second.lines);
        assert_eq!(second.dispatched, 0);
        assert_eq!(second.reviewing, 1);
        assert!(fake.writes.borrow().is_empty());
        let outage = ordinary_pass(&fake, &cfg, true, now() + time::Duration::days(3));
        assert!(
            outage.lines.iter().any(|l| l.contains("needs-human")),
            "{:?}",
            outage.lines
        );
        assert!(fake.writes.borrow().is_empty());
        // New heads are found by the full rescan, not an event or local cursor.
        let new_head = "b".repeat(40);
        fake.pulls.get_mut(&470).unwrap()["headRefOid"] = new_head.clone().into();
        fake.page(
            &format!("repos/{REPO}/pulls/470"),
            None,
            None,
            json!({"state": "open", "draft": false,
            "head": {"sha": new_head}, "author_association": "COLLABORATOR"}),
        );
        let pushed = ordinary_pass(&fake, &cfg, true, now());
        assert_eq!(pushed.dispatched, 1, "{:?}", pushed.lines);
    }
    for (attempt, expected) in [(1, 1), (2, 0)] {
        let (mut fake, cfg) = ordinary_fixture(BOT, "COLLABORATOR", false);
        let mut run = ordinary_run();
        run["run_attempt"] = attempt.into();
        ordinary_runs(&mut fake, vec![run]);
        let outcome = ordinary_pass(&fake, &cfg, true, now() + time::Duration::minutes(5));
        assert_eq!(outcome.errors, 0, "{:?}", outcome.lines);
        assert_eq!(outcome.dispatched, expected, "{:?}", outcome.lines);
        assert!(
            fake.writes
                .borrow()
                .iter()
                .all(|(url, _)| url.ends_with("/rerun-failed-jobs"))
        );
    }
}

#[test]
fn ordinary_review_commands_are_authorized_and_consumed_once() {
    for role in ["read", "triage", "write", "maintain", "admin"] {
        let (mut fake, cfg) = ordinary_fixture("visitor", "NONE", false);
        let command = json!({"id": 7, "user": {"login": "reviewer", "type": "User"}, "body": "/review", "created_at": "2026-10-01T00:00:00Z"});
        ordinary_comments(&mut fake, vec![command.clone()]);
        fake.page(
            &format!("repos/{REPO}/collaborators/reviewer/permission"),
            None,
            None,
            json!({"permission": role}),
        );
        let admitted = ["write", "maintain", "admin"].contains(&role);
        let first = ordinary_pass(&fake, &cfg, true, now());
        assert_eq!(first.errors, 0, "{role}: {:?}", first.lines);
        assert_eq!(
            first.dispatched,
            usize::from(admitted),
            "{role}: {:?}",
            first.lines
        );
        let mut comments = vec![command.clone()];
        comments.extend(ordinary_receipts(&fake));
        ordinary_comments(&mut fake, comments.clone());
        fake.writes.borrow_mut().clear();
        let second = ordinary_pass(&fake, &cfg, true, now());
        assert_eq!(second.dispatched, 0, "{role}: {:?}", second.lines);
        assert!(fake.writes.borrow().is_empty());
        if admitted {
            // A second request can review the same head, but not twice itself.
            let mut command = command;
            command["id"] = 8.into();
            comments.push(command);
            ordinary_comments(&mut fake, comments);
            let third = ordinary_pass(&fake, &cfg, true, now());
            assert_eq!(third.dispatched, 1, "{role}: {:?}", third.lines);
        }
    }
}

#[test]
fn ordinary_review_old_commands_do_not_authorize_new_heads() {
    for receipt in [MARKER, CLAIM] {
        let (mut fake, cfg) = ordinary_fixture("visitor", "NONE", false);
        let mut comments = Vec::new();
        for id in [7, 8] {
            comments.push(json!({"id": id, "user": {"login": "reviewer", "type": "User"},
                "body": "/review", "created_at": "2026-10-01T00:00:00Z"}));
            let state = if receipt == MARKER {
                json!({"review_head": format!("{}/{id}", "a".repeat(40)), "review_at": "2026-10-01T00:01:00Z"})
            } else {
                json!({"from": format!("review:{}/{id}", "a".repeat(40))})
            };
            comments.push(json!({"id": 100 + id, "user": {"login": BOT},
                "body": format!("<!-- {receipt}: {state} -->"), "created_at": "2026-10-01T00:01:00Z"}));
        }
        ordinary_comments(&mut fake, comments.clone());
        fake.page(&format!("repos/{REPO}/collaborators/reviewer/permission"), None, None,
            json!({"permission": "write"}));
        let pushed = ordinary_pass(&fake, &cfg, true, now());
        assert_eq!(pushed.errors, 0, "{receipt}: {:?}", pushed.lines);
        assert_eq!(pushed.dispatched, 0, "{receipt}: {:?}", pushed.lines);
        assert!(fake.writes.borrow().is_empty());
        // A genuinely new command still admits exactly one review of this head.
        comments.push(json!({"id": 9, "user": {"login": "reviewer", "type": "User"},
            "body": "/review", "created_at": "2026-10-12T01:00:00Z"}));
        ordinary_comments(&mut fake, comments.clone());
        let requested = ordinary_pass(&fake, &cfg, true, now());
        assert_eq!(requested.dispatched, 1, "{receipt}: {:?}", requested.lines);
        comments.extend(ordinary_receipts(&fake));
        ordinary_comments(&mut fake, comments);
        fake.writes.borrow_mut().clear();
        let repeated = ordinary_pass(&fake, &cfg, true, now());
        assert_eq!(repeated.dispatched, 0, "{receipt}: {:?}", repeated.lines);
        assert!(fake.writes.borrow().is_empty());
    }
}

#[test]
fn ordinary_review_uses_shared_wip_caps_and_strict_run_identity() {
    let (mut fake, mut cfg) = ordinary_fixture(BOT, "COLLABORATOR", false);
    cfg.policy.review_wip = Some(ReviewWip {
        wip: 1,
        active: 1,
        held: None,
    });
    let full = ordinary_pass(&fake, &cfg, true, now());
    assert_eq!(full.dispatched, 0, "{:?}", full.lines);
    assert!(fake.writes.borrow().is_empty());
    let mut run = ordinary_run();
    run["status"] = "in_progress".into();
    ordinary_runs(&mut fake, vec![run.clone()]);
    cfg.dispatch_repo = REPO.into();
    cfg.policy.review_caller_workflow = Some("dispatch.yml".into());
    let once = ordinary_pass(&fake, &cfg, false, now());
    assert_eq!(once.reviewing, 0, "caller already counted it");
    cfg.policy.review_caller_workflow = None;
    let separate = ordinary_pass(&fake, &cfg, false, now());
    assert_eq!(separate.reviewing, 1);
    let p = &fake.pulls[&470];
    assert!(reviews_head(p, &run, &cfg));
    for (key, value) in [
        ("path", json!(".github/workflows/review.yml")),
        ("head_branch", json!("other")),
        (
            "display_title",
            json!(format!("review {REPO}#470 {HEAD} (claude) junk")),
        ),
        ("repository", json!({"full_name": "foreign/repo"})),
    ] {
        let mut wrong = run.clone();
        wrong[key] = value;
        assert!(!reviews_head(p, &wrong, &cfg), "{key}");
    }
    // Even an ordinary approval cannot widen the legacy merge admission.
    let p = pull("pull-470", json!({"isCrossRepository": false}));
    assert!(verdicts(&p, &cfg).1.is_none());
}

#[test]
fn ordinary_review_verified_verdict_and_explicit_request_share_the_gate() {
    let (mut fake, cfg) = ordinary_fixture(BOT, "COLLABORATOR", false);
    let mut verdict = review("APPROVED", job(), HEAD, "");
    verdict["submittedAt"] = "2026-10-12T02:00:30Z".into();
    fake.pulls.get_mut(&470).unwrap()["reviews"]["nodes"] = json!([verdict.clone()]);
    let mut run = ordinary_run();
    run["conclusion"] = "success".into();
    fake.page(
        &format!("repos/{REPO}/actions/runs/{REVIEW_RUN}"),
        None,
        None,
        run.clone(),
    );
    ordinary_runs(&mut fake, vec![run.clone()]);
    let accepted = ordinary_pass(&fake, &cfg, false, now() + time::Duration::minutes(5));
    assert_eq!(accepted.errors, 0, "{:?}", accepted.lines);
    assert!(
        accepted
            .lines
            .iter()
            .any(|l| l.contains("wait: no approving review of the head yet")),
        "{:?}",
        accepted.lines
    );
    // A write-authorized command is an explicit second review, even though the
    // old completed run and its real approval have this same head.
    let command = json!({"id": 7, "user": {"login": "reviewer", "type": "User"}, "body": "/review", "created_at": "2026-10-12T02:02:00Z"});
    ordinary_comments(&mut fake, vec![command.clone()]);
    fake.page(
        &format!("repos/{REPO}/collaborators/reviewer/permission"),
        None,
        None,
        json!({"permission": "write"}),
    );
    let requested = ordinary_pass(&fake, &cfg, true, now() + time::Duration::minutes(5));
    assert_eq!(requested.errors, 0, "{:?}", requested.lines);
    assert_eq!(requested.dispatched, 1, "{:?}", requested.lines);
    assert_eq!(requested.merged, 0);
    let mut comments = vec![command];
    comments.extend(ordinary_receipts(&fake));
    ordinary_comments(&mut fake, comments);
    fake.writes.borrow_mut().clear();
    // Once the request has its own later run/verdict it is consumed, including
    // after losing all local state. No third start is possible.
    run["created_at"] = "2026-10-12T02:05:01Z".into();
    run["run_started_at"] = "2026-10-12T02:05:01Z".into();
    run["updated_at"] = "2026-10-12T02:06:00Z".into();
    verdict["submittedAt"] = "2026-10-12T02:05:30Z".into();
    fake.pulls.get_mut(&470).unwrap()["reviews"]["nodes"] = json!([verdict]);
    fake.page(
        &format!("repos/{REPO}/actions/runs/{REVIEW_RUN}"),
        None,
        None,
        run.clone(),
    );
    ordinary_runs(&mut fake, vec![run]);
    let consumed = ordinary_pass(&fake, &cfg, false, now() + time::Duration::minutes(10));
    assert_eq!(consumed.errors, 0, "{:?}", consumed.lines);
    assert!(
        consumed
            .lines
            .iter()
            .any(|l| l.contains("wait: no approving review of the head yet")),
        "{:?}",
        consumed.lines
    );
    assert!(fake.writes.borrow().is_empty());
    // A dispatch caller can give a non-review job a review-shaped title. It
    // must never become merge/fix provenance, even with a copied real marker.
    let p = &fake.pulls[&470];
    let review = &nodes(p, "/reviews/nodes")[0];
    let mut run = ordinary_run();
    run["created_at"] = "2026-10-12T02:05:01Z".into();
    run["run_started_at"] = "2026-10-12T02:05:01Z".into();
    run["updated_at"] = "2026-10-12T02:06:00Z".into();
    assert!(!review_run_matches(p, review, Some(&run), &cfg));
    run["path"] = ".github/workflows/review.yml".into();
    assert!(review_run_matches(p, review, Some(&run), &cfg));
    fake.page(&format!("repos/{REPO}/actions/runs/{REVIEW_RUN}"), None, None, run);
    let legacy = ordinary_pass(&fake, &cfg, false, now() + time::Duration::minutes(10));
    assert_eq!(legacy.errors, 0, "{:?}", legacy.lines);
    assert!(legacy.lines.iter().any(|l| l.contains("merge: review approved")), "{:?}", legacy.lines);
}

#[test]
fn ordinary_review_unreadable_commands_fall_back_to_automatic_admission() {
    for bad in ["policy-missing", "policy-invalid", "permission", "actor"] {
        let (mut fake, cfg) = ordinary_fixture("dependabot[bot]", "NONE", true);
        ordinary_comments(&mut fake, vec![json!({"id": 7,
            "user": {"login": if bad == "actor" {"bad/login"} else {"reviewer"}, "type": "User"},
            "body": "/review", "created_at": "2026-10-01T00:00:00Z"})]);
        let path = format!("{API_ROOT}repos/{REPO}/contents/workflow/review-command.toml?ref=main");
        match bad {
            "policy-missing" => fake.missing.push(path),
            "policy-invalid" => { fake.rest.get_mut(&path).unwrap().3 = json!({}); }
            _ => {}
        }
        let outcome = ordinary_pass(&fake, &cfg, true, now());
        assert_eq!(outcome.errors, 0, "{bad}: {:?}", outcome.lines);
        assert_eq!(outcome.dispatched, 1, "{bad}: {:?}", outcome.lines);
    }
}

#[test]
fn ordinary_review_missing_verdict_does_not_swallow_own_conflict() {
    let (mut fake, mut cfg) = ordinary_fixture(BOT, "COLLABORATOR", false);
    cfg.policy.reruns = false;
    fake.pulls.get_mut(&470).unwrap()["mergeable"] = CONFLICTING.into();
    ordinary_runs(&mut fake, vec![ordinary_run()]);
    let outcome = ordinary_pass(&fake, &cfg, false, now() + time::Duration::minutes(5));
    assert_eq!(outcome.errors, 0, "{:?}", outcome.lines);
    assert!(outcome.lines.iter().any(|l| l.contains("fix: conflicts with main")), "{:?}", outcome.lines);
    assert!(outcome.steps.iter().any(|s| s.action == "fix"));
}

#[test]
fn ordinary_review_intake_bounds_pulls_comments_actors_and_policy_reads() {
    // Extra PR reads and all ordinary admission are capped across a pass.
    let (mut fake, cfg) = ordinary_fixture("visitor", "NONE", false);
    let mut open = vec![];
    for number in 470..500 {
        open.push(json!({"number": number, "user": {"login": "visitor"}, "labels": []}));
        let mut p = fake.pulls[&470].clone();
        p["number"] = number.into();
        fake.pulls.insert(number, p);
        fake.page(&format!("repos/{REPO}/issues/{number}/comments?per_page=100"), None, None,
            json!([{"id": 7, "user": {"login": "reviewer", "type": "User"},
                "body": "/review", "created_at": "2026-10-01T00:00:00Z"}]));
        fake.page(&format!("repos/{REPO}/pulls/{number}"), None, None,
            json!({"state": "open", "draft": false, "head": {"sha": HEAD}, "author_association": "NONE", "user": {"type": "User"}}));
    }
    fake.page(&format!("repos/{REPO}/pulls?state=open&per_page=100"), None, None, json!(open));
    let outcome = ordinary_pass(&fake, &cfg, false, now());
    assert_eq!(outcome.errors, 0, "{:?}", outcome.lines);
    assert_eq!(*fake.pull_reads.borrow(), reviews::INTAKE_PULLS);
    assert_eq!(fake.sent.borrow().iter().filter(|(url, _)| url.contains("/contents/workflow/review.toml")).count(), 1);
    assert_eq!(fake.sent.borrow().iter().filter(|(url, _)| url.contains("/contents/workflow/review-command.toml")).count(), 1);
    assert_eq!(fake.sent.borrow().iter().filter(|(url, _)| url.ends_with("/permission")).count(), 1);

    // A next link means the history is incomplete; never follow it or dispatch.
    let (mut fake, cfg) = ordinary_fixture("cgwalters", "OWNER", false);
    let url = format!("{API_ROOT}repos/{REPO}/issues/470/comments?per_page=100");
    fake.rest.get_mut(&url).unwrap().2 = Some(format!("<{url}&page=2>; rel=\"next\""));
    let outcome = ordinary_pass(&fake, &cfg, true, now());
    assert_eq!(outcome.errors, 0, "{:?}", outcome.lines);
    assert_eq!(outcome.dispatched, 0);
    assert!(!fake.sent.borrow().iter().any(|(url, _)| url.contains("page=2")));
    assert!(fake.writes.borrow().is_empty());

    // Distinct unauthorized logins, not just repeated comments, are bounded.
    let (mut fake, cfg) = ordinary_fixture("visitor", "NONE", false);
    ordinary_comments(&mut fake, (1..=100).map(|id| json!({"id": id,
        "user": {"login": format!("actor-{id}"), "type": "User"}, "body": "/review",
        "created_at": "2026-10-01T00:00:00Z"})).collect());
    let outcome = ordinary_pass(&fake, &cfg, true, now());
    assert_eq!(outcome.errors, 0, "{:?}", outcome.lines);
    assert_eq!(outcome.dispatched, 0);
    assert_eq!(fake.sent.borrow().iter().filter(|(url, _)| url.ends_with("/permission")).count(), reviews::INTAKE_ACTORS);
}

#[test]
fn ordinary_review_unreadable_admission_never_dispatches() {
    for bad in [
        "bot-id",
        "policy",
        "labels",
        "reviews",
        "head-moved",
    ] {
        let (mut fake, cfg) = ordinary_fixture("dependabot[bot]", "NONE", true);
        match bad {
            "bot-id" => fake.page(
                &format!("repos/{REPO}/actions/variables/REVIEW_BOT_IDS"),
                None,
                None,
                json!({"value": "[1]"}),
            ),
            "policy" => fake.page(
                &format!("repos/{REPO}/contents/workflow/review.toml?ref=main"),
                None,
                None,
                json!({"type": "file", "encoding": "base64", "size": 65537, "content": ""}),
            ),
            "permission" => ordinary_comments(
                &mut fake,
                vec![
                    json!({"id": 7, "user": {"login": "reviewer", "type": "User"}, "body": "/review", "created_at": "2026-10-01T00:00:00Z"}),
                ],
            ),
            "labels" => {
                fake.pulls.get_mut(&470).unwrap()["labels"]["pageInfo"]["hasNextPage"] = true.into()
            }
            "reviews" => {
                fake.pulls.get_mut(&470).unwrap()["reviews"]["pageInfo"]["hasPreviousPage"] =
                    true.into()
            }
            "head-moved" => fake.page(
                &format!("repos/{REPO}/pulls/470"),
                None,
                None,
                json!({"state": "open", "draft": false, "head": {"sha": "a".repeat(40)}}),
            ),
            _ => unreachable!(),
        }
        let outcome = ordinary_pass(&fake, &cfg, true, now());
        assert_eq!(outcome.dispatched, 0, "{bad}: {:?}", outcome.lines);
        assert!(fake.writes.borrow().is_empty(), "{bad}");
    }
}
