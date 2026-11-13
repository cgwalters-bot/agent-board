//! Upstream observations use the pass's existing request/time budget. No
//! association is followed outside admitted midstream/promotion forks.
use super::*;

pub(super) const REPLAN: &str = "The board pass asks a triage run to re-plan: upstream completed";

impl<C: Rest + Transport> Scheduler<'_, C> {
    fn upstream_fork(&self, repository: &str) -> bool {
        self.config
            .promote
            .as_ref()
            .is_some_and(|p| p.forks.keys().any(|r| r.eq_ignore_ascii_case(repository)))
            || self
                .midstreams
                .as_ref()
                .is_some_and(|repos| repos.iter().any(|r| r.eq_ignore_ascii_case(repository)))
    }

    fn read_upstream(&self, facts: &Facts) {
        let key = key(&facts.issue);
        if !facts.open
            || !self.upstream_fork(&facts.issue.repository)
            || self.report.upstreams.borrow().contains_key(&key)
        {
            return;
        }
        let read = (|| {
            let Some(target) = crate::upstream::target(&self.client, &facts.issue)? else {
                return Ok(None);
            };
            crate::upstream::State::read(&self.client, &target).map(Some)
        })();
        let observation = match read {
            Ok(None) => return,
            Ok(Some(state)) => Ok(state),
            Err(error) => Err(format!("upstream link unreadable: {error:#}")),
        };
        self.report.upstreams.borrow_mut().insert(key, observation);
    }

    pub(super) fn upstream_replan(
        &self,
        read: (&Facts, &[plan::Comment]),
        context: (Option<&Value>, &Target, &Slots),
    ) -> Result<Option<Step>> {
        // Observe only a candidate scheduling actually considers, after its
        // active holds and scheduling reserve. Board enumeration must not
        // spend the capacity needed to start ready work.
        self.read_upstream(read.0);
        let observations = self.report.upstreams.borrow();
        let Some(observation) = observations.get(&key(&read.0.issue)) else {
            return Ok(None);
        };
        let state = match observation {
            Ok(state) => state,
            Err(error) => {
                return Ok(Some(Step::Hold(Hold::new(
                    HoldKind::Unreadable,
                    error.clone(),
                ))));
            }
        };
        let Some(closed) = state.completed_by_other(&self.config.bot) else {
            return Ok(None);
        };
        // A plan made after the completion has already considered it. The
        // normal triage receipt/cooldown handles that plan from here on.
        match plan::newest_plan(read.1, &self.writers()) {
            Ok((_, comment)) if comment.changed >= closed => return Ok(None),
            Err(_) => return Ok(None), // Preserve normal invalid-plan holds.
            _ => {}
        }
        let intro = format!("{REPLAN}: {} at {}.", state.item, closed.format(&Rfc3339)?);
        self.triage(read, &intro, context).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{config, preview, preview_within, replay};
    use super::*;

    #[test]
    fn upstream_pass_reads_are_bounded_and_completion_replans() {
        const FORK: &str = "cgwalters-forge/tracker";
        for case in [
            "off",
            "none",
            "open",
            "completed",
            "bot completed",
            "not planned",
            "new plan",
            "malformed",
            "budget",
            "crowded",
        ] {
            let mut replay = replay();
            // One candidate with an already-dispatched implement plan.
            for entry in &mut replay.requests {
                if let Some(nodes) = entry["answer"]
                    .pointer_mut("/data/repositoryOwner/projectV2/items/nodes")
                    .and_then(Value::as_array_mut)
                {
                    nodes.retain(|n| {
                        n["content"]["number"] == 468
                            && text(&n["content"], "/repository/nameWithOwner") == FORK
                    });
                    if case == "crowded" && !nodes.is_empty() {
                        let template = nodes[0].clone();
                        for number in 1000..1060 {
                            let mut later = template.clone();
                            later["id"] = json!(format!("later-{number}"));
                            later["content"]["id"] = json!(format!("issue-{number}"));
                            later["content"]["number"] = json!(number);
                            later["content"]["url"] =
                                json!(format!("https://github.com/{FORK}/issues/{number}"));
                            later["content"]["createdAt"] = json!("2026-10-23T15:00:00Z");
                            later["content"]["labels"] = json!({"totalCount":1,"nodes":[{"name":"needs-human"}],"pageInfo":{"hasNextPage":false}});
                            nodes.push(later);
                        }
                    }
                }
            }
            let mut settings = json!({});
            if case == "crowded" {
                for number in 1000..1060 {
                    replay.requests.push(json!({"request":{"get":format!("{API_ROOT}repos/{FORK}/issues/{number}/timeline?per_page=100&page=1")},
                        "answer":{"status":200,"link":null,"body":[]}}));
                }
            }
            if case != "off" {
                settings["promote"] = json!({"forks":{FORK:{"upstream":"other-org/project", "base":"main"}},
                    "signoff":{"name":"Operator", "email":"operator@example.invalid"}, "policyRepository":FORK});
            }
            let mut config = config(settings);
            config.epics = None;
            for (path, body) in [
                (
                    format!("repos/{FORK}"),
                    json!({"full_name":FORK,"fork":true,"parent":{"full_name":"other-org/project"}}),
                ),
                (
                    format!("repos/{FORK}/issues/468/timeline?per_page=100&page=1"),
                    json!([]),
                ),
                (
                    format!("repos/{FORK}/issues/468"),
                    json!({"number":468,"repository_url":format!("{API_ROOT}repos/{FORK}"),
                    "body":match case { "none" => "No link", "malformed" => "Upstream: other/repo#9", _ => "Upstream: #9" }}),
                ),
                (
                    "repos/other-org/project/issues/9".into(),
                    json!({"number":9,"repository_url":format!("{API_ROOT}repos/other-org/project"),
                    "state":if case == "open" {"open"} else {"closed"},
                    "state_reason":if case == "not planned" {"not_planned"} else {"completed"},
                    "closed_at":if case == "new plan" {"2026-01-01T00:00:00Z"} else {"2026-10-23T14:00:00Z"}, "closed_by":{"login":if case == "bot completed" {config.bot.as_str()} else {"maintainer"}}}),
                ),
            ] {
                replay.requests.push(json!({"request":{"get":format!("{API_ROOT}{path}")}, "answer":{"status":200,"link":null,"body":body}}));
            }
            struct Empty;
            impl Transport for Empty {
                fn graphql(&self, _: &str, _: Value) -> Result<Value> {
                    Ok(
                        json!({"repository":{"pullRequests":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}),
                    )
                }
            }
            if case != "off" {
                let recorder = Recorder::new(&Empty);
                crate::promote::awaiting::candidates(&recorder, &config, FORK).unwrap();
                replay.requests.extend(
                    Replay::from_json(
                        &serde_json::from_str(&recorder.fixture(replay.now).unwrap()).unwrap(),
                    )
                    .unwrap()
                    .requests,
                );
            }
            let report = if case == "budget" {
                preview_within(&config, &replay, 25)
            } else if case == "crowded" {
                preview_within(&config, &replay, 200)
            } else {
                preview(&config, &replay)
            };
            if case == "budget" {
                assert!(report.spent.requests <= 25);
                continue;
            }
            assert_eq!(report.failures, 0, "{case}: {:?}", report.lines);
            let observations = report.upstreams.borrow();
            let at = format!("{FORK}#468");
            match case {
                "off" | "none" => assert!(!observations.contains_key(&at), "{case}"),
                "malformed" => assert!(
                    observations[&at]
                        .as_ref()
                        .unwrap_err()
                        .contains("malformed")
                ),
                _ => assert!(
                    observations.get(&at).is_some_and(Result::is_ok),
                    "{case}: {observations:?} {:?}",
                    report.lines
                ),
            }
            let replanned = report
                .lines
                .iter()
                .any(|l| l.starts_with(&format!("start triage {FORK}#468")));
            assert_eq!(
                replanned,
                matches!(case, "completed" | "crowded"),
                "{case}: {:?}",
                report.lines
            );
            if case == "crowded" {
                assert_eq!(observations.len(), 1);
                assert!(report.spent.requests <= 200);
            }
        }
    }
}
