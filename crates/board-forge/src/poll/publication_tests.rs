#[test]
fn upstream_publication_ci_repairs_the_original_fork_pr() {
    const FORK: &str = "cgwalters-forge/composefs-rs";
    const UPSTREAM: &str = "composefs/composefs-rs";
    const LATER: &str = "2026-10-24T00:00:00Z";
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
    for case in ["red", "second round", "exhausted", "awaiting approval", "pending publication", "green", "pending", "none", "legacy green", "legacy none", "edited", "wrong head", "fork moved", "off", "held", "discovery unavailable", "follow", "follow posted", "follow held", "follow unread labels", "follow late hold", "follow upstream moved", "follow close fails", "follow retry", "follow republished", "follow closed", "follow manual close", "follow unknown publication", "follow awaiting approval", "follow unfollowed", "follow handed over", "follow unbounded", "follow direct"] {
        let legacy = case.starts_with("legacy ");
        // promote.followUpstream (agent-board#1205): close the fork PR instead.
        let follow = case.starts_with("follow");
        // The receipt already records the hand-over.
        let handed_over = ["follow retry", "follow closed"].contains(&case);
        // The fork PR is closed, so only its upstream PR is walked.
        let closed = ["follow closed", "follow manual close", "follow unknown publication"].contains(&case);
        let (mut replay, settings) = crate::promote::tests::handoff_scenario();
        let mut fake = github();
        let published = "a".repeat(40);
        // A rewritten publication's branch names the head first approved,
        // which a published repair keeps; a direct one is the fork PR's own.
        let original_head = replay.requests.iter()
            .find(|e| e["request"]["get"].as_str().is_some_and(|p| p.ends_with("/pulls/34")))
            .map(|e| e["answer"]["body"]["head"].clone())
            .unwrap();
        let approved = original_head["sha"].as_str().unwrap().to_owned();
        let publication_branch = match case {
            "follow direct" => original_head["ref"].as_str().unwrap().to_owned(),
            "follow republished" => format!("bot/promote/{}", "e".repeat(40)),
            _ => format!("bot/promote/{approved}"),
        };
        // The receipt is canonical, with a publication head/branch distinct
        // from the fork PR. The existing metadata authenticates its writer.
        for entry in &mut replay.requests {
            let path = entry["request"]["get"].as_str().unwrap_or("").to_owned();
            if handed_over && entry["request"]["variables"]["number"] == 34
                && let Some(node) = entry["answer"]["data"]["repository"]["pullRequest"]["comments"]["nodes"].get_mut(0)
            {
                node["lastEditedAt"] = json!(LATER);
                node["editor"] = json!({"login":BOT});
            }
            let body = &mut entry["answer"]["body"];
            if path.ends_with("/pulls/34") {
                body["state"] = json!(if closed { "closed" } else { "open" });
                if case.ends_with("awaiting approval") { body["head"]["sha"] = json!("c".repeat(40)); }
            } else if path.ends_with("/pulls/70") {
                body["head"]["sha"] = json!(if case == "wrong head" { "b".repeat(40) } else { published.clone() });
                body["head"]["ref"] = json!(publication_branch);
            } else if path.contains("/issues/34/comments?") {
                let old = body[0]["body"].as_str().unwrap();
                let record = crate::plan::fenced_json_as(old, "agent-board-promoted", "test").unwrap().unwrap();
                let mut new = old.replace(
                    &format!("\"published_oid\": {}", record["published_oid"]),
                    &format!("\"published_oid\": \"{published}\""),
                ).replace(
                    &format!("\"published_branch\": {}", record["published_branch"]),
                    &format!("\"published_branch\": \"{publication_branch}\""),
                );
                if handed_over {
                    new = new.replace("\n}\n```", ",\n  \"handed_over\": true\n}\n```");
                    body[0]["updated_at"] = json!(LATER);
                }
                body[0]["body"] = json!(new);
                if case == "edited" {
                    body[0]["updated_at"] = json!(LATER);
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
        if case.ends_with("held") { original["labels"]["nodes"] = json!([{"name":"hold"}]); }
        // A hold past the labels read would go unseen.
        if case == "follow unread labels" { original["labels"]["pageInfo"] = json!({"hasNextPage":true}); }
        if case == "fork moved" { original["headRefOid"] = json!("c".repeat(40)); }
        fake.pulls.insert(34, original);
        let mut upstream = pull("pull-470", json!({"number":70,"headRefOid":published,"headRefName":publication_branch,
            "headRepository":{"nameWithOwner":fork["head"]["repo"]["full_name"]},
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
        cfg.policy.follow_promoted = follow;
        // The upstream PR's head is in the fork, whose owner the bot pushes to.
        cfg.policy.head_owners = vec!["cgwalters-forge".into()];
        // Nor where its maintainers have it, or fix runs may not push there.
        if case == "follow handed over" { cfg.policy.maintainers = [UPSTREAM.to_ascii_lowercase()].into(); }
        if case == "follow unbounded" {
            let fork_only = format!("repos = [\"{FORK}\"]\n[outputs.push_to_pull_request_branch]\nbranches = [{}]\n", fork["head"]["ref"]);
            cfg.policy.fix_bounds = bounds::FixBoundsSet::new(vec![bounds::FixBounds::parse(&fork_only).unwrap()]);
        }
        // The fork PR's source issue, which tracks the work: a visitor's
        // quote and another slice's hand-over do not count as this one's.
        let marker = |n| crate::promote::handoff::handover_marker(FORK, n, &format!("{UPSTREAM}#70"), &approved);
        let mut tracking = vec![
            json!({"id":7,"user":{"login":"someone"},"body":format!("{} quoted", marker(34))}),
            json!({"id":8,"user":{"login":BOT},"body":marker(33)}),
        ];
        if case == "follow posted" {
            tracking.push(json!({"id":9,"user":{"login":BOT.to_uppercase()},"body":format!("{}\nWork continues upstream.", marker(34))}));
        }
        fake.page(&format!("repos/{FORK}/issues/31/comments?per_page=100"), None, None, json!(tracking));
        // The upstream PR is red too, and in the same walk. Its receipt's
        // link leaves the fork PR among its cross-references, beside another
        // PR's of the same fork that has none.
        fake.page(&format!("repos/{UPSTREAM}/pulls?state=open&per_page=100"), None, None, json!([{"number":70,"user":{"login":BOT}}]));
        fake.page(&format!("repos/{UPSTREAM}/issues/70/comments?per_page=100"), None, None, json!([]));
        let reference = |n: u64| json!({"event":"cross-referenced","source":{"issue":{"html_url":format!("https://github.com/{FORK}/pull/{n}"),"pull_request":{}}}});
        let timeline = match case {
            "follow unknown publication" => json!([]),
            _ => json!([reference(35), reference(34)]),
        };
        fake.page(&format!("repos/{UPSTREAM}/issues/70/timeline?per_page=100"), None, None, timeline);
        fake.page(&format!("repos/{FORK}/pulls/35"), None, None, json!({"number":35,"user":{"login":"someone"}}));
        if case == "follow close fails" {
            fake.answers.borrow_mut().insert(format!("{API_ROOT}repos/{FORK}/pulls/34"), vec![(500, json!({"message":"boom"}))]);
        }
        fake.page(&format!("repos/{UPSTREAM}/events?per_page=100"), None, None, json!([]));
        if ["second round", "exhausted"].contains(&case) {
            let url = format!("repos/{FORK}/issues/34/comments?per_page=100");
            let mut rows = fake.rest[&format!("{API_ROOT}{url}")].3.clone();
            rows.as_array_mut().unwrap().push(json!({"id":100,"user":{"login":BOT},"body":marker_body(&json!({"rounds":if case == "exhausted" {FIX_ROUNDS} else {1},"sha":"old repaired head"}), "Fix round.")}));
            fake.page(&url, None, None, rows.clone());
            // Authenticate all comments, including the existing round marker.
            for entry in &mut replay.requests {
                if entry["request"]["variables"]["number"] == 34 {
                    let nodes = &mut entry["answer"]["data"]["repository"]["pullRequest"]["comments"]["nodes"];
                    if let Some(nodes) = nodes.as_array_mut() { nodes.push(json!({"url":format!("https://github.com/{FORK}/pull/34#issuecomment-100"),"createdAt":"2026-10-23T00:00:00Z","lastEditedAt":null,"author":{"login":BOT},"editor":null})); }
                }
            }
        }
        // Unless stage 2 follows the upstream PR, the fork PR keeps its rounds.
        let repos: Vec<String> = match case {
            "discovery unavailable" => vec![FORK.into(), REPO.into()],
            _ if closed => vec![UPSTREAM.into()],
            _ if follow && case != "follow unfollowed" => vec![FORK.into(), UPSTREAM.into()],
            _ => vec![FORK.into()],
        };
        let outcome = run(&fake, &Queries(&fake, &replay), &cfg, State::default(), &[], &repos, None, replay.now).unwrap();
        // Only a receipt that records the hand-over lets the upstream PR's
        // rounds move it: not a closed fork PR, nor another PR's reference.
        let upstream_step = outcome.steps.iter().find(|s| s.repository == UPSTREAM);
        if ["edited", "wrong head", "fork moved", "discovery unavailable"].contains(&case) {
            assert!(outcome.errors > 0, "{case}: {:?}", outcome.lines);
            assert!(outcome.steps.iter().filter(|s| s.repository == FORK).all(|s| s.action != "fix"));
            if case == "discovery unavailable" { assert!(outcome.steps.iter().any(|s| s.repository == REPO && s.number == 470), "{:?}", outcome.lines); }
        } else if closed {
            assert_eq!(outcome.errors, 0, "{case}: {:?}", outcome.lines);
            let step = upstream_step.unwrap();
            assert_eq!(step.number, 70);
            assert_eq!(step.action == "fix", case == "follow closed", "{case}: {:?}", outcome.lines);
            let why = match case {
                "follow manual close" => format!("it publishes {FORK}#34, which has not handed it over yet"),
                "follow unknown publication" => "it may publish a fork PR, and no receipt names it".into(),
                _ => String::new(),
            };
            assert!(step.reason.contains(&why), "{case}: {:?}", outcome.lines);
        } else {
            assert_eq!(outcome.errors, 0, "{case}: {:?}", outcome.lines);
            // An upstream PR the fork PR has not handed over gets no round
            // first, or the round would move it off the receipt's head.
            if follow && !["follow unfollowed", "follow awaiting approval"].contains(&case) {
                assert!(upstream_step.is_none_or(|s| s.number == 70 && (s.action == "fix") == handed_over), "{case}: {:?}", outcome.lines);
                if let Some(step) = upstream_step.filter(|_| !["follow handed over", "follow unbounded", "follow retry"].contains(&case)) {
                    assert!(step.reason.contains(&format!("it publishes {FORK}#34, which has not handed it over yet")), "{case}: {:?}", outcome.lines);
                }
            }
            let step = &outcome.steps[0];
            assert_eq!(step.repository, FORK);
            assert_eq!(step.number, 34);
            assert_eq!(step.action == "fix", ["red", "second round", "follow unfollowed", "follow handed over", "follow unbounded", "follow direct"].contains(&case), "{case}: {:?}", outcome.lines);
            if legacy {
                assert_eq!(step.action, "wait", "{case}: {:?}", outcome.lines);
                assert!(step.reason.contains("merging into this base repository is disabled"), "{case}: {:?}", outcome.lines);
            }
            if case == "red" {
                assert_eq!(step.head, head);
                assert!(step.reason.contains(&format!("upstream {UPSTREAM}#70")));
            }
            // Only a current publication hands over, also once republished
            // from a repaired head, and a hold keeps it.
            assert_eq!(step.action == "close", ["follow", "follow posted", "follow late hold", "follow upstream moved", "follow close fails", "follow retry", "follow republished"].contains(&case), "{case}: {:?}", outcome.lines);
            if step.action == "close" {
                assert!(step.ran && step.reason.contains(&format!("promoted to {UPSTREAM}#70")), "{:?}", outcome.lines);
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
        if ["follow", "follow posted", "follow held", "follow unread labels", "follow late hold", "follow upstream moved", "follow close fails", "follow retry"].contains(&case) {
            // A hold put on, or an upstream push, while the hand-over posts
            // its comment, seen on reading both again before the receipt.
            let reads = *fake.pull_reads.borrow();
            if case == "follow late hold" { fake.hold_after = Some(reads + 3); }
            if case == "follow upstream moved" { fake.moved_after = Some((70, reads + 4)); }
            let dispatcher = Fake::default();
            let applied = run(&fake, &Queries(&fake, &replay), &cfg, State::default(), &[], &repos, applying(&dispatcher), replay.now).unwrap();
            // A failed close leaves the fork PR open; its recorded hand-over
            // lets the upstream PR's rounds start, and a retry closes it.
            assert_eq!(applied.errors > 0, case == "follow close fails", "{:?}", applied.lines);
            assert_eq!(applied.dispatched, usize::from(handed_over), "{:?}", applied.lines);
            let upstream = applied.steps.iter().find(|s| s.repository == UPSTREAM).unwrap();
            assert_eq!(upstream.action == "fix", handed_over, "{case}: {:?}", applied.lines);
            let writes: Vec<String> = fake.writes.borrow().iter().map(|(at, _)| at.clone()).filter(|at| !at.contains(UPSTREAM)).collect();
            let mut want = vec![
                format!("POST {API_ROOT}repos/{FORK}/issues/31/comments"),
                format!("PATCH {API_ROOT}repos/{FORK}/issues/comments/99"),
                format!("PATCH {API_ROOT}repos/{FORK}/pulls/34"),
            ];
            match case {
                "follow posted" => drop(want.remove(0)),
                "follow held" | "follow unread labels" => want.clear(),
                "follow late hold" | "follow upstream moved" => want.truncate(1),
                "follow retry" => drop(want.drain(..2)),
                _ => {}
            }
            assert_eq!(writes, want, "{case}: {:?}", applied.lines);
            for (at, body) in fake.writes.borrow().iter().filter(|(at, _)| !at.ends_with("/pulls/34") && !at.contains(UPSTREAM)) {
                assert!(text(body, "/body").contains(&format!("https://github.com/{UPSTREAM}/pull/70")), "{at}");
                // The receipt, rewritten in its canonical form, is the hand-over.
                assert_eq!(text(body, "/body").contains("\"handed_over\": true\n}\n```"), at.contains("/issues/comments/99"), "{at}");
            }
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
