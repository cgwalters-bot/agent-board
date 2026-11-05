const DRAFT_REPO: &str = "cgwalters-forge/bootc";

fn draft_result(p: &Value) -> Value {
    json!({"schema": drafts::RESULT_SCHEMA, "repository":DRAFT_REPO, "pr":470,
        "reviewed_head":HEAD, "title":p["title"], "description":p["body"],
        "semantic_diff":"pass", "description_assessment":"matches", "change_category":"docs",
        "risk":"none", "evidence":"Inspected README diff: correct spelling only", "reason":"Small mechanical spelling correction"})
}

fn draft_fixture() -> (Fake, Config) {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let (mut fake, mut cfg) = ordinary_fixture(BOT, "COLLABORATOR", false);
    fake.rest = fake.rest.into_iter().map(|(url, (e,m,n,v))| {
        let v = serde_json::from_str(&serde_json::to_string(&v).unwrap().replace(REPO, DRAFT_REPO)).unwrap();
        (url.replace(REPO, DRAFT_REPO), (e,m,n,v))
    }).collect();
    let p = fake.pulls.get_mut(&470).unwrap();
    p["isDraft"] = true.into();
    p["body"] = "Correct a spelling error in README.".into();
    p["isCrossRepository"] = false.into();
    p["baseRepository"]["nameWithOwner"] = DRAFT_REPO.into();
    p["commits"]["nodes"][0]["commit"]["oid"] = HEAD.into();
    p["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = json!({"state":"SUCCESS", "contexts": {
        "totalCount":1, "nodes":[{"__typename":"StatusContext", "id":"ci", "context":"ci", "state":"SUCCESS", "isRequired":true}],
        "pageInfo":{"hasNextPage":false}}});
    let evidence = draft_result(p);
    p["reviews"]["nodes"] = json!([review("APPROVED", job(), HEAD, "")]);
    p["reviews"]["nodes"][0]["body"] = format!("```{}\n{}\n```\n<!-- gh-aw-agentic-workflow: review, id: 1, run: https://github.com/{DRAFT_REPO}/actions/runs/{REVIEW_RUN} -->", drafts::RESULT_FENCE, evidence).into();
    p["reviews"]["nodes"][0]["submittedAt"] = "2026-10-12T02:00:30Z".into();
    let raw = json!({"number":470, "state":"open", "draft":true, "head":{"sha":HEAD}, "base":{"ref":"main"},
        "author_association":"COLLABORATOR", "user":{"login":BOT,"type":"User"},
        "title":p["title"], "body":p["body"], "labels":[{"name":"rescue"},{"name":"review:deep"}], "changed_files":1});
    fake.page(&format!("repos/{DRAFT_REPO}/pulls/470"), None, None, raw);
    fake.page(&format!("repos/{DRAFT_REPO}/rules/branches/main"), None, None,
        json!([{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"ci"}]}}]));
    fake.page(&format!("repos/{DRAFT_REPO}/pulls/470/files?per_page=100"), None, None,
        json!([{"filename":"README.md","status":"modified","changes":2,"patch":"@@ spelling"}]));
    fake.page(&format!("repos/{DRAFT_REPO}/issues/470/comments?per_page=100"), None, None,
        json!([{"id":99,"user":{"login":BOT},"body": format!("<!-- {MARKER}: {} -->", json!({"review_head":HEAD,"review_at":"2026-10-12T02:00:00Z"}))}]));
    let mut run = ordinary_run();
    run["repository"]["full_name"] = DRAFT_REPO.into();
    run["display_title"] = format!("review {DRAFT_REPO}#470 {HEAD} (claude)").into();
    run["conclusion"] = "success".into();
    fake.page(&format!("repos/{DRAFT_REPO}/actions/runs/{REVIEW_RUN}"), None, None, run);
    fake.page(&format!("repos/{DRAFT_REPO}/actions/runs/{REVIEW_RUN}/attempts/1/jobs?per_page=100"), None, None,
        json!({"total_count":1,"jobs":[{"name":drafts::RESULT_JOB,"run_id":REVIEW_RUN,"status":"completed","conclusion":"success","started_at":"2026-10-12T02:00:01Z","completed_at":"2026-10-12T02:01:00Z"}]}));
    let policy = format!("hosts = [\"github.com\"]\nrepos = [\"{DRAFT_REPO}\"]\nbases = [\"main\"]\n[github.trigger]\nevents = [\"pull_request_target\"]\nroles = [\"all\"]\nbots = []\nforks = true\n");
    fake.page(&format!("repos/{DRAFT_REPO}/contents/workflow/review.toml?ref=main"), None, None,
        json!({"type":"file","size":policy.len(),"encoding":"base64","content":STANDARD.encode(policy)}));
    cfg.policy.review_dispatch_repositories = vec![DRAFT_REPO.into()];
    cfg.policy.draft_review_repositories = vec![DRAFT_REPO.into()];
    cfg.policy.no_merge = vec![DRAFT_REPO.into()];
    (fake, cfg)
}

fn draft_pass(fake: &Fake, cfg: &Config, apply: bool, caps: Option<&caps::Caps>) -> Outcome {
    let dispatcher = Fake::default();
    run_capped(fake, fake, cfg, State::default(), (&[], &[DRAFT_REPO.into()]),
        if apply { applying(&dispatcher) } else {None}, caps, now()).unwrap()
}

#[test]
fn draft_review_verified_completion_repeat_push_and_ci_change() {
    let (mut fake, cfg) = draft_fixture();
    for apply in [false, true] {
        let out = draft_pass(&fake, &cfg, apply, None);
        assert_eq!(out.errors, 0, "{:?}", out.lines);
        assert_eq!(out.waits[0].review_bucket.as_ref().unwrap().bucket, drafts::Bucket::Quick, "{:?}", out.waits);
        assert_eq!(out.merged, 0);
        assert_eq!(out.dispatched, 0);
        if !apply { assert!(fake.writes.borrow().is_empty()); }
    }
    assert_eq!(fake.writes.borrow().len(), 2);
    assert!(fake.writes.borrow()[0].0.contains("DELETE "));
    assert_eq!(fake.writes.borrow()[1].1, json!({"labels":["review:quick"]}));
    let raw = &mut fake.rest.get_mut(&format!("{API_ROOT}repos/{DRAFT_REPO}/pulls/470")).unwrap().3;
    raw["labels"] = json!([{"name":"rescue"},{"name":"review:quick"}]);
    fake.writes.borrow_mut().clear();
    let repeat = draft_pass(&fake, &cfg, true, None);
    assert_eq!(repeat.errors, 0, "{:?}", repeat.lines);
    assert!(fake.writes.borrow().is_empty());
    fake.pulls.get_mut(&470).unwrap()["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["nodes"][0]["state"] = "FAILURE".into();
    let failed = draft_pass(&fake, &cfg, true, None);
    assert_eq!(failed.waits[0].review_bucket.as_ref().unwrap().bucket, drafts::Bucket::Deep);
    assert!(fake.writes.borrow()[0].0.ends_with("/labels/review:quick"));
    assert!(fake.writes.borrow().iter().all(|(url,_)| !url.contains("rescue")));
    fake.pulls.get_mut(&470).unwrap()["headRefOid"] = "b".repeat(40).into();
    fake.rest.get_mut(&format!("{API_ROOT}repos/{DRAFT_REPO}/pulls/470")).unwrap().3["head"]["sha"] = "b".repeat(40).into();
    let pushed = draft_pass(&fake, &cfg, false, None);
    assert_eq!(pushed.errors, 0, "{:?}", pushed.lines);
    let assessment = pushed.waits[0].review_bucket.as_ref().unwrap();
    assert_eq!(assessment.bucket, drafts::Bucket::Deep);
    assert_eq!(assessment.assessed_head, "b".repeat(40));
}

#[test]
fn draft_review_vetoes_forgery_staleness_and_incomplete_reads() {
    for bad in ["author", "run", "title", "interval", "job", "job-interval", "job-page", "reservation", "head", "result", "semantic", "risk", "description", "category", "evidence", "file-page", "file-count", "unknown-path", "rename", "ci-page", "ci-count", "ci-head", "ci-pending", "required", "reviews"] {
        let (mut fake, cfg) = draft_fixture();
        let p = fake.pulls.get_mut(&470).unwrap();
        match bad {
            "author" => p["reviews"]["nodes"][0]["author"]["databaseId"] = 1.into(),
            "run" | "title" | "interval" => {
                let run = &mut fake.rest.get_mut(&format!("{API_ROOT}repos/{DRAFT_REPO}/actions/runs/{REVIEW_RUN}")).unwrap().3;
                match bad { "run" => run["path"] = ".github/workflows/other.yml".into(), "title" => run["display_title"] = "forged title".into(), _ => run["run_started_at"] = "2026-10-12T03:00:00Z".into() }
            }
            "reservation" => fake.rest.get_mut(&format!("{API_ROOT}repos/{DRAFT_REPO}/issues/470/comments?per_page=100")).unwrap().3 = json!([]),
            "job" | "job-interval" | "job-page" => {
                let page = fake.rest.get_mut(&format!("{API_ROOT}repos/{DRAFT_REPO}/actions/runs/{REVIEW_RUN}/attempts/1/jobs?per_page=100")).unwrap();
                match bad { "job" => page.3["jobs"][0]["name"] = "implement".into(), "job-interval" => page.3["jobs"][0]["started_at"] = "2026-10-12T02:00:59Z".into(), _ => page.3["total_count"] = 2.into() }
            }
            "head" | "result" | "semantic" | "risk" | "description" | "category" | "evidence" => {
                let mut r = draft_result(p);
                let (key, v) = match bad { "head" => ("reviewed_head", json!("a".repeat(40))), "result" => ("schema", json!("v0")), "semantic" => ("semantic_diff",json!("fail")), "risk" => ("risk",json!("unknown")), "description" => ("description",json!("edited")), "category" => ("change_category",json!("feature")), _ => ("evidence",json!("")) };
                r[key] = v;
                p["reviews"]["nodes"][0]["body"] = format!("```{}\n{}\n```\n<!-- gh-aw-agentic-workflow: review, id: 1, run: https://github.com/{DRAFT_REPO}/actions/runs/{REVIEW_RUN} -->", drafts::RESULT_FENCE,r).into();
            }
            "file-page" => fake.rest.get_mut(&format!("{API_ROOT}repos/{DRAFT_REPO}/pulls/470/files?per_page=100")).unwrap().2 = Some("next".into()),
            "file-count" => fake.rest.get_mut(&format!("{API_ROOT}repos/{DRAFT_REPO}/pulls/470")).unwrap().3["changed_files"] = 2.into(),
            "unknown-path" | "rename" => {
                let f = &mut fake.rest.get_mut(&format!("{API_ROOT}repos/{DRAFT_REPO}/pulls/470/files?per_page=100")).unwrap().3[0];
                f[if bad == "rename" {"previous_filename"} else {"filename"}] = "src/install.rs".into();
            }
            "ci-page" => p["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["pageInfo"]["hasNextPage"] = true.into(),
            "ci-count" => p["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["totalCount"] = 2.into(),
            "ci-head" => p["commits"]["nodes"][0]["commit"]["oid"] = "a".repeat(40).into(),
            "ci-pending" => p["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["nodes"][0]["state"] = "PENDING".into(),
            "required" => { fake.page(&format!("repos/{DRAFT_REPO}/rules/branches/main"),None,None,json!([{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"missing"}]}}])); }
            "reviews" => p["reviews"]["pageInfo"]["hasPreviousPage"] = true.into(),
            _ => unreachable!(),
        }
        let out = draft_pass(&fake, &cfg, false, None);
        assert_eq!(out.errors, 0, "{bad}: {:?}", out.lines);
        assert_eq!(out.waits[0].review_bucket.as_ref().unwrap().bucket, drafts::Bucket::Deep, "{bad}: {:?}",out.waits);
        assert_eq!(out.merged, 0);
    }
}

#[test]
fn draft_review_interrupted_labels_caps_and_head_races() {
    for interrupted in [false, true] {
        let (mut fake, cfg) = draft_fixture();
        if interrupted {
            fake.answers.borrow_mut().insert(format!("{API_ROOT}repos/{DRAFT_REPO}/issues/470/labels"), vec![(500,json!({"message":"interrupted"}))]);
        }
        let caps = caps::Caps::new(caps::Limits { writes:Some(0), ..Default::default() });
        let held = draft_pass(&fake, &cfg, true, Some(&caps));
        assert_eq!(held.errors, 0, "{:?}", held.lines);
        assert!(fake.writes.borrow().is_empty());
        let first = draft_pass(&fake, &cfg, true, None);
        assert_eq!(first.errors, usize::from(interrupted), "{:?}",first.lines);
        // Model the delete's success and lost POST. Next pass repairs the gap,
        // without replacing rescue or redispatching the reviewed head.
        fake.rest.get_mut(&format!("{API_ROOT}repos/{DRAFT_REPO}/pulls/470")).unwrap().3["labels"] = json!([{"name":"rescue"}]);
        fake.writes.borrow_mut().clear();
        let repaired = draft_pass(&fake, &cfg, true, None);
        assert_eq!(repaired.errors, 0, "{:?}",repaired.lines);
        assert_eq!(repaired.dispatched, 0);
        assert_eq!(fake.writes.borrow().len(), 1);
        assert_eq!(fake.writes.borrow()[0].1, json!({"labels":["review:quick"]}));
    }
    for before_publication in [false,true] {
        let (fake,cfg) = draft_fixture();
        let path = format!("{API_ROOT}repos/{DRAFT_REPO}/pulls/470");
        let old = fake.rest[&path].3.clone();
        let mut pushed = old.clone();
        pushed["head"]["sha"] = "b".repeat(40).into();
        pushed["labels"] = json!([{"name":"review:quick"},{"name":"rescue"}]);
        let answers = if before_publication {vec![old.clone(),old.clone(),pushed]} else {vec![old.clone(),pushed.clone(),pushed]};
        fake.read_answers.borrow_mut().insert(path, answers);
        let out = draft_pass(&fake,&cfg,true,None);
        assert_eq!(out.errors,0,"{:?}",out.lines);
        let r = out.waits[0].review_bucket.as_ref().unwrap();
        assert_eq!(r.bucket,drafts::Bucket::Deep);
        assert_eq!(r.assessed_head,"b".repeat(40));
        assert!(fake.writes.borrow().iter().any(|(u,_)| u.ends_with("/labels/review:quick")));
        assert_eq!(out.dispatched,0);
        assert_eq!(out.merged,0);
    }
}

#[test]
fn draft_review_intake_is_explicit_and_reserves_heads_without_ready_or_fix() {
    let (mut fake, mut cfg) = draft_fixture();
    cfg.policy.draft_review_repositories.clear();
    let off = draft_pass(&fake,&cfg,false,None);
    assert_eq!(off.dispatched,0);
    assert!(off.waits.iter().all(|w| w.review_bucket.is_none()));
    cfg.policy.draft_review_repositories = vec![DRAFT_REPO.into()];
    fake.pulls.get_mut(&470).unwrap()["reviews"]["nodes"] = json!([]);
    fake.page(&format!("repos/{DRAFT_REPO}/issues/470/comments?per_page=100"),None,None,json!([]));
    let out = draft_pass(&fake,&cfg,true,None);
    assert_eq!(out.errors,0,"{:?}",out.lines);
    assert_eq!(out.dispatched,1,"{:?}",out.lines);
    assert_eq!(out.merged,0);
    let writes = fake.writes.borrow();
    let dispatch = writes.iter().find(|(url,_)| url.ends_with("/dispatches")).unwrap();
    assert_eq!(dispatch.1["inputs"]["kind"],"review");
    assert_eq!(dispatch.1["inputs"]["head"],HEAD);
    assert!(dispatch.1["inputs"]["task"].as_str().unwrap().contains(drafts::RESULT_SCHEMA));
    assert!(!writes.iter().any(|(u,b)| u.starts_with("mutation") || b.get("draft").is_some() || b["inputs"]["kind"] == "fix"));
}
