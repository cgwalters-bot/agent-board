//! Observed links are context, never authority to hold work or push a branch.
use super::*;
use serde::Serialize;

const MAX_LINKS: usize = 400;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PullLink {
    repository: String,
    number: u64,
    state: String,
}

impl PullLink {
    pub(super) fn validate(&self) -> Result<()> {
        validate_repository(&self.repository)?;
        ensure!(self.number > 0, "invalid linked pull request number");
        ensure!(
            matches!(self.state.as_str(), "OPEN" | "CLOSED" | "MERGED"),
            "invalid linked pull request state"
        );
        Ok(())
    }

    pub(super) fn display(&self) -> String {
        format!(
            "- https://github.com/{}/pull/{} ({})",
            self.repository,
            self.number,
            self.state.to_lowercase()
        )
    }
}

pub(super) fn validate(links: &[PullLink]) -> Result<()> {
    ensure!(links.len() <= MAX_LINKS, "too many linked pull requests");
    for link in links {
        link.validate()?;
    }
    Ok(())
}

fn insert(links: &mut BTreeMap<(String, u64), PullLink>, pull: &Value) -> Result<()> {
    let link = PullLink {
        repository: text(pull, "/repository/nameWithOwner").to_ascii_lowercase(),
        number: pull["number"]
            .as_u64()
            .context("linked pull request number")?,
        state: text(pull, "/state").into(),
    };
    link.validate()?;
    links.insert((link.repository.clone(), link.number), link);
    ensure!(links.len() <= MAX_LINKS, "too many linked pull requests");
    Ok(())
}

/// Read every bounded cross-reference, including non-closing body mentions.
/// Partial reads never replace the last complete observation in the ledger.
fn read(client: &(impl Rest + Transport), facts: &Facts) -> Result<Vec<PullLink>> {
    ensure!(facts.pulls_complete, "closing references are incomplete");
    ensure!(
        facts.promotion != Some(Promotion::Unknown),
        "promotion evidence is incomplete"
    );
    let issue = &facts.issue;
    let (owner, name) = issue
        .repository
        .split_once('/')
        .context("issue repository")?;
    let mut links = BTreeMap::new();
    let mut cursor = Value::Null;
    let mut seen = BTreeSet::new();
    for page in 0..active::LINK_PAGES {
        let data = client.graphql(
            active::LINKS,
            json!({
                "owner": owner, "name": name, "number": issue.number, "cursor": cursor
            }),
        )?;
        let timeline = data
            .pointer("/repository/issue/timelineItems")
            .context("missing issue cross references")?;
        let nodes = timeline["nodes"]
            .as_array()
            .context("missing cross references")?;
        ensure!(nodes.len() <= 100, "oversized cross-reference page");
        for event in nodes {
            let source = event
                .get("source")
                .filter(|v| v.is_object())
                .context("unreadable cross-reference source")?;
            // GitHub returns an empty object for the query's other union types.
            if source.get("number").is_some() {
                insert(&mut links, source)?;
            }
        }
        match timeline
            .pointer("/pageInfo/hasNextPage")
            .and_then(Value::as_bool)
        {
            Some(false) => break,
            Some(true) => {
                ensure!(
                    page + 1 < active::LINK_PAGES,
                    "cross references exceed page bound"
                );
                let next = timeline
                    .pointer("/pageInfo/endCursor")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .context("cross references have no cursor")?;
                ensure!(
                    seen.insert(next.to_owned()),
                    "repeated cross-reference cursor"
                );
                cursor = next.into();
            }
            None => bail!("cross references have no page coverage"),
        }
    }
    // Closing references and verified promotion evidence may name PRs the
    // issue's timeline does not (notably the published upstream PR).
    for pull in facts.pulls.iter().chain(match &facts.promotion {
        Some(Promotion::Open(pull)) => Some(pull),
        _ => None,
    }) {
        insert(
            &mut links,
            &json!({"repository":{"nameWithOwner":pull.repository},
                "number":pull.number,"state":"OPEN"}),
        )?;
    }
    // A token can see private PRs that readers of this issue cannot. Never
    // disclose cross-repository metadata unless GitHub confirms it is public.
    // Same-repository links have the destination's own visibility boundary.
    let mut public = BTreeMap::new();
    let mut visible = Vec::new();
    for link in links.into_values() {
        if !link.repository.eq_ignore_ascii_case(&issue.repository) {
            if !public.contains_key(&link.repository) {
                let response = client.get(&format!("{API_ROOT}repos/{}", link.repository), None)?;
                ensure!(
                    response.status == 200,
                    "linked repository visibility: HTTP {}",
                    response.status
                );
                ensure!(
                    text(&response.body, "/full_name").eq_ignore_ascii_case(&link.repository),
                    "linked repository identity differs"
                );
                let private = response.body["private"]
                    .as_bool()
                    .context("linked repository visibility missing")?;
                public.insert(link.repository.clone(), !private);
            }
            if !public[&link.repository] {
                continue;
            }
        }
        visible.push(link);
    }
    Ok(visible)
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    /// Optional observation runs only after scheduling has made every decision:
    /// visitor-controlled visibility fan-out cannot consume a start's reads.
    pub(super) fn refresh_pull_links(&mut self) {
        let candidates = std::mem::take(&mut *self.client.bounds.pull_link_candidates.borrow_mut());
        for facts in candidates.into_values() {
            if let Err(error) = self.track_pull_links(&facts) {
                self.line(format!(
                    "warning: pull links {}: {error:#}; previous inventory retained; scheduling unchanged",
                    facts.issue.display()
                ));
                if budget_spent(&error) {
                    break;
                }
            }
        }
    }

    pub(super) fn track_pull_links(&self, facts: &Facts) -> Result<()> {
        if self.config.living_status.is_none() {
            return Ok(());
        }
        // Authenticate the public status before changing its local observation.
        plan::issue_comments(&self.client, &facts.issue)?;
        let links = read(&self.kept(), facts)?;
        if self.apply
            && let Some(store) = self.client.bounds.living.borrow_mut().as_mut()
        {
            store.track_pulls(&facts.issue, links)?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// Older live recordings predate inventory reads. Explicit empty observations
    /// keep unrelated living-status protocol tests focused on their own writes.
    pub(in crate::pass) fn seed_empty(replay: &mut Replay) {
        let issues: BTreeSet<_> = replay
            .requests
            .iter()
            .filter_map(|e| e["answer"].pointer("/data/repositoryOwner/projectV2/items/nodes"))
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(|node| issue_ref(&node["content"]).ok())
            .map(|i| (i.repository, i.number))
            .collect();
        for (repo, number) in issues {
            let (owner, name) = repo.split_once('/').unwrap();
            for (query, variables, data) in [
                (
                    active::LINKS,
                    json!({"owner":owner,"name":name,"number":number,"cursor":null}),
                    json!({"repository":{"issue":{"timelineItems":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}),
                ),
                (
                    plan::COMMENTS_QUERY,
                    json!({"owner":owner,"name":name,"number":number,"after":null}),
                    json!({"repository":{"issue":{"body":"","author":{"login":"cgwalters-bot"},"comments":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}),
                ),
            ] {
                let key = graphql_key(query, &variables);
                if !replay.requests.iter().any(|e| e["request"] == key) {
                    replay
                        .requests
                        .push(json!({"request":key,"answer":{"data":data}}));
                }
            }
        }
    }

    fn pull(repo: &str, number: u64, state: &str) -> Value {
        json!({"source":{"repository":{"nameWithOwner":repo},"number":number,"state":state}})
    }

    fn fixture(pages: Vec<Value>) -> Replay {
        let mut replay = super::super::tests::replay();
        for repo in ["up/r", "private/r"] {
            replay.requests.push(json!({"request":{"get":format!("{API_ROOT}repos/{repo}")},
                "answer":{"status":200,"body":{"full_name":repo,"private":repo == "private/r"},"link":null}}));
        }
        for (page, timeline) in pages.into_iter().enumerate() {
            let cursor = if page == 0 {
                Value::Null
            } else {
                json!(format!("p{page}"))
            };
            replay
                .requests
                .push(json!({"request":graphql_key(active::LINKS,
                &json!({"owner":"o","name":"r","number":1,"cursor":cursor})),
                "answer":{"data":{"repository":{"issue":{"timelineItems":timeline}}}}}));
        }
        replay
    }

    fn page(nodes: Vec<Value>, more: bool, cursor: Value) -> Value {
        json!({"nodes":nodes,"pageInfo":{"hasNextPage":more,"endCursor":cursor}})
    }

    #[test]
    fn tracks_mentions_closing_and_verified_upstream_without_author_admission() {
        let replay = fixture(vec![
            page(
                vec![pull("O/R", 3, "OPEN"), json!({"source":{}})],
                true,
                json!("p1"),
            ),
            page(
                vec![pull("o/r", 3, "OPEN"), pull("up/r", 9, "MERGED")],
                false,
                Value::Null,
            ),
        ]);
        let mut facts = super::super::tests::facts("o/r", 1, "2026-01-01T00:00:00Z");
        facts.pulls.push(ClosingPull {
            repository: "o/r".into(),
            number: 2,
            author: None,
            updated: None,
        });
        facts.promotion = Some(Promotion::Open(ClosingPull {
            repository: "up/r".into(),
            number: 10,
            author: None,
            updated: None,
        }));
        let links = read(&replay, &facts).unwrap();
        assert_eq!(
            links
                .iter()
                .map(|p| (p.repository.as_str(), p.number, p.state.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("o/r", 2, "OPEN"),
                ("o/r", 3, "OPEN"),
                ("up/r", 9, "MERGED"),
                ("up/r", 10, "OPEN")
            ]
        );
    }

    #[test]
    fn incomplete_or_malformed_observations_refuse() {
        let facts = super::super::tests::facts("o/r", 1, "2026-01-01T00:00:00Z");
        for pages in [
            vec![json!({"nodes":[]})],
            vec![page(vec![json!({"source":null})], false, Value::Null)],
            vec![page(vec![pull("../r", 1, "OPEN")], false, Value::Null)],
            vec![page(vec![pull("o/r", 0, "OPEN")], false, Value::Null)],
            vec![page(vec![pull("o/r", 1, "UNKNOWN")], false, Value::Null)],
            vec![page(vec![], true, Value::Null)],
            vec![
                page(vec![], true, json!("p1")),
                page(vec![], true, json!("p1")),
            ],
            (1..=3)
                .map(|n| page(vec![], true, json!(format!("p{n}"))))
                .collect(),
            vec![page(vec![pull("o/r", 1, "OPEN"); 101], false, Value::Null)],
        ] {
            assert!(read(&fixture(pages.clone()), &facts).is_err(), "{pages:?}");
        }
        for unknown_promotion in [false, true] {
            let mut partial = facts.clone();
            if unknown_promotion {
                partial.promotion = Some(Promotion::Unknown);
            } else {
                partial.pulls_complete = false;
            }
            assert!(read(&fixture(vec![page(vec![], false, Value::Null)]), &partial).is_err());
        }
    }

    #[test]
    fn private_cross_repository_links_are_not_public_context() {
        let facts = super::super::tests::facts("o/r", 1, "2026-01-01T00:00:00Z");
        let replay = fixture(vec![page(
            vec![pull("private/r", 8, "OPEN"), pull("o/r", 9, "OPEN")],
            false,
            Value::Null,
        )]);
        let links = read(&replay, &facts).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].repository, "o/r");
        let replay = fixture(vec![page(
            vec![pull("unreadable/r", 8, "OPEN")],
            false,
            Value::Null,
        )]);
        assert!(read(&replay, &facts).is_err());
    }
}
