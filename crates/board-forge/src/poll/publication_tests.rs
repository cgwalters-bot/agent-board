#[test]
fn upstream_publication_ci_repairs_the_original_fork_pr() {
    const FORK: &str = "cgwalters-forge/composefs-rs";
    const UPSTREAM: &str = "composefs/composefs-rs";
    struct Queries<'a>(&'a Fake, &'a crate::pass::Replay);
    impl Transport for Queries<'_> {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            if query.contains("lastEditedAt author") {
                self.1.graphql(query, variables)
            } else {
                self.0.graphql(query, variables)
            }
        }
    }
    for case in ["red", "second round", "exhausted", "awaiting approval", "pending publication", "green", "pending", "none", "legacy green", "legacy none", "edited", "wrong head", "fork moved", "off", "held", "discovery unavailable"] {
        let legacy = case.starts_with("legacy ");
        let (mut replay, settings) = crate::promote::tests::handoff_scenario();
        let mut fake = github();
        let published = "a".repeat(40);
        let publication_branch = "bot/promote/test";
        // The receipt is canonical, with a publication head/branch distinct
        // from the fork PR. The existing metadata authenticates its writer.
        for entry in &mut replay.requests {
            let path = entry["request"]["get"].as_str().unwrap_or("").to_owned();
            let body = &mut entry["answer"]["body"];
            if path.ends_with("/pulls/34") {
                body["state"] = json!("open");
                if case == "awaiting approval" { body["head"]["sha"] = json!("c".repeat(40)); }
            } else if path.ends_with("/pulls/70") {
                body["head"]["sha"] = json!(if case == "wrong head" { "b".repeat(40) } else { published.clone() });
                body["head"]["ref"] = json!(publication_branch);
            } else if path.contains("/issues/34/comments?") {
                let old = body[0]["body"].as_str().unwrap();
                let record = crate::plan::fenced_json_as(old, "agent-board-promoted", "test").unwrap().unwrap();
                body[0]["body"] = json!(old.replace(
                    &format!("\"published_oid\": {}", record["published_oid"]),
                    &format!("\"published_oid\": \"{published}\""),
                ).replace(
                    &format!("\"published_branch\": {}", record["published_branch"]),
                    &format!("\"published_branch\": \"{publication_branch}\""),
                ));
                if case == "edited" {
                    body[0]["updated_at"] = json!("2026-10-24T00:00:00Z");
                }
                if case == "pending publication" {
                    body[0]["body"] = json!(crate::promote::tests::pending_repair_receipt());
                }
            }
            if !path.is_empty() {
                fake.page(path.strip_prefix(API_ROOT).unwrap(), None, None, body.clone());
                if path.contains("?per_page=100&page=1") {
                    fake.page(path.strip_prefix(API_ROOT).unwrap().replace("&page=1", "").as_str(), None, None, body.clone());
                }
            }
        }
        let fork = fake.rest[&format!("{API_ROOT}repos/{FORK}/pulls/34")].3.clone();
        let head = fork["head"]["sha"].as_str().unwrap();
        assert_ne!(head, published);
        let mut original = pull("pull-470", json!({
            "number":34,"headRefOid":head,"headRefName":fork["head"]["ref"],
            "body":fork["body"],"isDraft":true,"baseRepository":{"nameWithOwner":FORK},
            "contexts":{"ci":null,"review":null}
        }));
        original["commits"]["nodes"][0]["commit"]["oid"] = json!(head);
        if legacy {
            original["isDraft"] = json!(false);
            original["baseRepository"]["defaultBranchRef"] = json!({"name":"main"});
            original["reviews"] = json!({"nodes":[review("APPROVED", user("cgwalters"), head, "")],"pageInfo":{"hasPreviousPage":false}});
        }
        if case == "pending publication" || legacy {
            original["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = json!({"contexts":{"totalCount":1,"pageInfo":{"hasNextPage":false},"nodes":[{"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":"FAILURE"}]}});
        }
        if case == "held" { original["labels"]["nodes"] = json!([{"name":"hold"}]); }
        if case == "fork moved" { original["headRefOid"] = json!("c".repeat(40)); }
        fake.pulls.insert(34, original);
        let mut upstream = pull("pull-470", json!({"number":70,"headRefOid":published,"headRefName":publication_branch,
            "contexts":{"review":null,"ci":{"status":if case == "pending" {"IN_PROGRESS"} else {"COMPLETED"},"conclusion":if ["green", "legacy green"].contains(&case) {"SUCCESS"} else {"FAILURE"}}}}));
        upstream["commits"]["nodes"][0]["commit"]["oid"] = json!(published);
        if ["none", "legacy none"].contains(&case) { upstream["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = Value::Null; }
        fake.pulls.insert(70, upstream);
        for repo in [FORK, UPSTREAM] {
            fake.page(&format!("repos/{repo}/rules/branches/main"), None, None, json!([]));
            fake.page(&format!("repos/{repo}/branches/main"), None, None, json!({"protected":false}));
        }
        fake.page(&format!("repos/{FORK}/pulls?state=open&per_page=100"), None, None, json!([{"number":34,"user":{"login":BOT}}]));
        fake.page(&format!("repos/{FORK}/events?per_page=100"), None, None, json!([]));
        let mut cfg = config();
        if !legacy { cfg.policy.no_merge = vec![FORK.into()]; }
        if case != "off" { cfg.policy.publication_forks = settings.promote.unwrap().forks; }
        if case == "discovery unavailable" { cfg.policy.unavailable_publication_forks = vec![FORK.into()]; }
        if ["second round", "exhausted"].contains(&case) {
            let url = format!("repos/{FORK}/issues/34/comments?per_page=100");
            let mut rows = fake.rest[&format!("{API_ROOT}{url}")].3.clone();
            rows.as_array_mut().unwrap().push(json!({"id":100,"user":{"login":BOT},"body":marker_body(&json!({"rounds":if case == "exhausted" {2} else {1},"sha":"old repaired head"}), "Fix round.")}));
            fake.page(&url, None, None, rows.clone());
            // Authenticate all comments, including the existing round marker.
            for entry in &mut replay.requests {
                if entry["request"]["variables"]["number"] == 34 {
                    let nodes = &mut entry["answer"]["data"]["repository"]["pullRequest"]["comments"]["nodes"];
                    if let Some(nodes) = nodes.as_array_mut() { nodes.push(json!({"url":format!("https://github.com/{FORK}/pull/34#issuecomment-100"),"createdAt":"2026-10-23T00:00:00Z","lastEditedAt":null,"author":{"login":BOT},"editor":null})); }
                }
            }
        }
        let repos = if case == "discovery unavailable" { vec![FORK.into(), REPO.into()] } else { vec![FORK.into()] };
        let outcome = run(&fake, &Queries(&fake, &replay), &cfg, State::default(), &[], &repos, None, replay.now).unwrap();
        if ["edited", "wrong head", "fork moved", "discovery unavailable"].contains(&case) {
            assert!(outcome.errors > 0, "{case}: {:?}", outcome.lines);
            assert!(outcome.steps.iter().filter(|s| s.repository == FORK).all(|s| s.action != "fix"));
            if case == "discovery unavailable" { assert!(outcome.steps.iter().any(|s| s.repository == REPO && s.number == 470), "{:?}", outcome.lines); }
        } else {
            assert_eq!(outcome.errors, 0, "{case}: {:?}", outcome.lines);
            let step = &outcome.steps[0];
            assert_eq!(step.repository, FORK);
            assert_eq!(step.number, 34);
            assert_eq!(step.action == "fix", ["red", "second round"].contains(&case), "{case}: {:?}", outcome.lines);
            if legacy {
                assert_eq!(step.action, "wait", "{case}: {:?}", outcome.lines);
                assert!(step.reason.contains("merging into this base repository is disabled"), "{case}: {:?}", outcome.lines);
            }
            if case == "red" {
                assert_eq!(step.head, head);
                assert!(step.reason.contains(&format!("upstream {UPSTREAM}#70")));
            }
        }
        assert!(fake.writes.borrow().is_empty());
        if case == "pending publication" || legacy {
            let dispatcher = Fake::default();
            let applied = run(&fake, &Queries(&fake, &replay), &cfg, State::default(), &[], &[FORK.into()], applying(&dispatcher), replay.now).unwrap();
            assert_eq!(applied.errors, 0, "{:?}", applied.lines);
            assert_eq!(applied.dispatched, 0);
            assert_eq!(applied.merged, 0);
            assert!(fake.writes.borrow().is_empty());
        }
        if case == "red" {
            let dispatcher = Fake::default();
            let applied = run(&fake, &Queries(&fake, &replay), &cfg, State::default(), &[], &[FORK.into()], applying(&dispatcher), replay.now).unwrap();
            assert_eq!(applied.errors, 0, "{:?}", applied.lines);
            assert_eq!(applied.dispatched, 1, "{:?}", applied.lines);
            assert!(fake.writes.borrow().iter().all(|(url, _)| !url.contains(UPSTREAM)));
            let writes = fake.writes.borrow();
            assert!(writes.iter().any(|(url, body)| url.contains("issues/34/comments") && body["body"].as_str().is_some_and(|s| s.contains("round"))));
        }
    }
}
