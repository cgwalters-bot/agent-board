//! Read-only inbox context. Unread parentage is unknown, never “none”.
use super::*;
use holds::{EpicActivity, EpicChain};

const RUN_HOURS: i64 = 48;
const COMMENT_DAYS: i64 = 7;
/// GitHub permits up to 100 identities in one cheap validation read.
const BATCH_SIZE: usize = 100;

pub(in crate::pass) const CHAIN_FIELDS: &str = r#"
  __typename
  ... on Issue {
    issueState: state labels(first: 100) { nodes { name } totalCount }
    parent { number repository { nameWithOwner } }
  }
  ... on PullRequest {
    pullRequestState: state labels(first: 100) { nodes { name } totalCount }
    closingIssuesReferences(first: 2) { nodes { number repository { nameWithOwner } } totalCount }
  }
"#;
pub(in crate::pass) const CONTEXT_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issueOrPullRequest(number: $number) {
      ... on Issue {
        number state repository { nameWithOwner }
        labels(first: 50) { nodes { name } totalCount }
        parent { number repository { nameWithOwner } }
        subIssues(first: 100) { nodes { number repository { nameWithOwner } } totalCount }
        closedByPullRequestsReferences(first: 1, includeClosedPrs: false) { totalCount }
        comments(last: 100) { nodes { author { login } createdAt body } totalCount }
      }
      ... on PullRequest {
        closingIssuesReferences(first: 100) { nodes { number repository { nameWithOwner } } totalCount }
      }
    }
  }
}"#;

#[derive(Default)]
pub(super) struct Node {
    pub(super) parent: Option<String>,
    pub(super) live: bool,
    children: Vec<String>,
    why: BTreeSet<String>,
    complete: bool,
}

pub(super) fn reference(node: &Value) -> Option<ItemRef> {
    let repository = node["repository"]["nameWithOwner"].as_str()?;
    validate_repository(repository).ok()?;
    let number = node["number"].as_u64().filter(|n| *n > 0)?;
    Some(on_github(repository, number))
}

fn recent(at: Option<&str>, now: OffsetDateTime, window: time::Duration) -> bool {
    at.and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .is_some_and(|at| at <= now && now - at <= window)
}

fn chain(start: &str, nodes: &BTreeMap<String, Node>) -> EpicChain {
    let mut current = Some(start.to_owned());
    let mut seen = BTreeSet::new();
    let mut epics = Vec::new();
    while let Some(at) = current {
        if !seen.insert(at.clone()) {
            return EpicChain::default();
        }
        let Some(node) = nodes.get(&at) else {
            return EpicChain::default();
        };
        if node.live {
            epics.push(at);
        }
        current = node.parent.clone();
    }
    EpicChain {
        epic: epics.first().cloned().unwrap_or_else(|| "none".into()),
        top_epic: epics.last().cloned().unwrap_or_else(|| "none".into()),
    }
}

fn activity(at: &str, nodes: &BTreeMap<String, Node>) -> EpicActivity {
    let mut pending = vec![at.to_owned()];
    let mut seen = BTreeSet::new();
    let mut why = BTreeSet::new();
    let mut complete = true;
    while let Some(key) = pending.pop() {
        if !seen.insert(key.clone()) {
            complete = false;
            continue;
        }
        let Some(node) = nodes.get(&key) else {
            complete = false;
            continue;
        };
        complete &= node.complete;
        for reason in &node.why {
            // Runs and PRs count on descendants, not on the epic itself.
            if reason == "operator-comment"
                || reason == "focus" && key == at
                || key != at && matches!(reason.as_str(), "recent-run" | "open-pr")
            {
                why.insert(reason.clone());
            }
        }
        pending.extend(node.children.clone());
    }
    EpicActivity {
        active: !why.is_empty(),
        active_why: why.into_iter().collect(),
        complete,
    }
}

pub(super) fn decorate(list: &mut NeedsYou, nodes: &BTreeMap<String, Node>) {
    for identity in &mut list.identities {
        let resolved = chain(&identity.item.to_ascii_lowercase(), nodes);
        identity.epic = resolved.epic.clone();
        identity.top_epic = resolved.top_epic.clone();
        if let Some(ask) = list.items.iter_mut().find(|a| a.key == identity.key) {
            ask.chain = resolved;
        }
    }
    list.epics = nodes
        .iter()
        .filter(|(_, n)| n.live)
        .map(|(key, _)| (key.clone(), activity(key, nodes)))
        .collect();
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    /// Breadth-first, deduplicated upward reads. Activity never takes the
    /// requests needed to establish another identity's parentage.
    pub(super) fn inbox_chains(
        &mut self,
        starts: Vec<ItemRef>,
        found: &mut BTreeMap<String, Value>,
        attempted: &mut BTreeSet<String>,
    ) {
        let mut pending: BTreeMap<_, _> = starts.into_iter().map(|i| (key(&i), i)).collect();
        pending.retain(|at, _| !attempted.contains(at));
        let mut next = BTreeMap::new();
        while !pending.is_empty() && self.room() {
            let batch: Vec<_> = pending.values().take(BATCH_SIZE).cloned().collect();
            let mut query = String::from("query { ");
            for (n, issue) in batch.iter().enumerate() {
                pending.remove(&key(issue));
                attempted.insert(key(issue));
                let Some((owner, name)) = issue.repository.split_once('/') else {
                    continue;
                };
                // ItemRef admission above restricts repository names; JSON
                // quoting also keeps all interpolated strings as data.
                query.push_str(&format!(
                    "r{n}: repository(owner: {}, name: {}) {{ issueOrPullRequest(number: {}) {{ {} }} }} ",
                    json!(owner), json!(name), issue.number, CHAIN_FIELDS
                ));
            }
            query.push('}');
            match self.client.graphql(&query, json!({})) {
                Ok(data) => {
                    for (n, issue) in batch.iter().enumerate() {
                        let value = &data[format!("r{n}")]["issueOrPullRequest"];
                        if !value.is_object() {
                            continue;
                        }
                        let parent = if value.get("closingIssuesReferences").is_some() {
                            let closing = &value["closingIssuesReferences"];
                            (closing["totalCount"] == 1)
                                .then(|| reference(&closing["nodes"][0]))
                                .flatten()
                        } else {
                            reference(&value["parent"])
                        };
                        if let Some(parent) = parent
                            .filter(|p| self.config.epics.is_some() && !attempted.contains(&key(p)))
                        {
                            next.insert(key(&parent), parent);
                        }
                        let mut value = value.clone();
                        value["state"] = value
                            .get("issueState")
                            .or_else(|| value.get("pullRequestState"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        found.insert(key(issue), value);
                    }
                }
                Err(error) => self.line(format!("needs-you validation error: {error:#}")),
            }
            if pending.is_empty() {
                pending = std::mem::take(&mut next);
                pending.retain(|at, _| !attempted.contains(at));
            }
        }
    }

    fn chain_node(&mut self, issue: &ItemRef, node: &Value) -> Result<Node> {
        if let Some(closing) = node.get("closingIssuesReferences") {
            let count = closing["totalCount"]
                .as_u64()
                .context("missing closing issue count")?;
            ensure!(
                closing["nodes"]
                    .as_array()
                    .is_some_and(|n| n.len() as u64 == count),
                "incomplete closing issues"
            );
            ensure!(count <= 1, "PR has no unique closing issue");
            let parent = if count == 1 {
                Some(key(
                    &reference(&closing["nodes"][0]).context("missing closing issue")?
                ))
            } else {
                None
            };
            return Ok(Node {
                parent,
                ..Node::default()
            });
        }
        let labels = labels(node).context("incomplete labels")?;
        ensure!(
            matches!(node["state"].as_str(), Some("OPEN" | "CLOSED")),
            "missing state"
        );
        let parent = node.get("parent").context("missing parent")?;
        let parent = if parent.is_null() {
            None
        } else {
            Some(key(&reference(parent).context("invalid parent")?))
        };
        let live = if node["state"] != "OPEN" || !labels.iter().any(|l| l == EPIC_LABEL) {
            false
        } else {
            ensure!(
                self.epics.contains_key(&key(issue)) || self.room(),
                "no budget for epic provenance"
            );
            self.live(&Parent {
                issue: issue.clone(),
                open: node["state"] == "OPEN",
                labels_complete: true,
                labels,
            })
            .map_err(anyhow::Error::msg)?
        };
        Ok(Node {
            live,
            parent,
            ..Node::default()
        })
    }

    pub(super) fn inbox_epics(
        &mut self,
        list: &mut NeedsYou,
        raw: &BTreeMap<String, Value>,
        expand: bool,
    ) {
        if self.config.epics.is_none() {
            for identity in &mut list.identities {
                identity.epic = "none".into();
                identity.top_epic = "none".into();
            }
            for ask in &mut list.items {
                ask.chain = EpicChain {
                    epic: "none".into(),
                    top_epic: "none".into(),
                };
            }
            return;
        }
        let mut nodes: BTreeMap<_, _> = raw
            .iter()
            .filter_map(|(at, value)| {
                let (r, n) = pull_ref(at)?;
                self.chain_node(&on_github(&r, n), value)
                    .ok()
                    .map(|node| (at.clone(), node))
            })
            .collect();
        // Resolve membership from the cheap upward reads, not optional activity.
        decorate(list, &nodes);
        let mut pending: Vec<ItemRef> = Vec::new();
        if expand {
            pending.extend(
                self.epics
                    .iter()
                    .filter(|(_, live)| **live == Ok(true))
                    .filter_map(|(k, _)| pull_ref(k).map(|(r, n)| on_github(&r, n))),
            );
        }
        let mut attempted = BTreeSet::new();
        while let Some(issue) = pending.pop() {
            let at = key(&issue);
            if !attempted.insert(at.clone()) || !self.room() {
                continue;
            }
            match self.inbox_node(&issue) {
                Ok((node, refs)) => {
                    // Only downward activity expansion remains. Upward
                    // membership is resolved independently above.
                    pending.extend(refs.into_iter().filter(|r| node.children.contains(&key(r))));
                    nodes.insert(at, node);
                }
                Err(error) => self.line(format!(
                    "needs-you epic context error: {}: {error:#}",
                    issue.display()
                )),
            }
        }
        // Inventory/ancestor reads already resolved every identity, before
        // optional descendant activity could spend the remaining budget.
        list.epics = nodes
            .iter()
            .filter(|(_, n)| n.live)
            .map(|(key, _)| (key.clone(), activity(key, &nodes)))
            .collect();
        for (at, live) in &self.epics {
            if *live == Ok(true) {
                list.epics
                    .entry(at.clone())
                    .or_insert_with(|| activity(at, &nodes));
            }
        }
        if let Some(focus) = self.config.focus().as_ref().map(key)
            && let Some(activity) = list.epics.get_mut(&focus)
        {
            activity.active = true;
            if !activity.active_why.iter().any(|why| why == "focus") {
                activity.active_why.insert(0, "focus".into());
            }
        }
    }

    fn inbox_node(&mut self, issue: &ItemRef) -> Result<(Node, Vec<ItemRef>)> {
        let (owner, name) = issue.repository.split_once('/').context("repository")?;
        let data = self.client.graphql(
            CONTEXT_QUERY,
            json!({"owner":owner,"name":name,"number":issue.number}),
        )?;
        let node = &data["repository"]["issueOrPullRequest"];
        ensure!(node.is_object(), "missing issue or pull request");
        let mut refs = Vec::new();
        if let Some(issues) = node.get("closingIssuesReferences") {
            if issues["totalCount"].as_u64() == Some(0) {
                return Ok((
                    Node {
                        complete: true,
                        ..Node::default()
                    },
                    refs,
                ));
            }
            ensure!(
                issues["totalCount"].as_u64() == Some(1),
                "PR has no unique closing issue; epic unknown"
            );
            let parent = reference(&issues["nodes"][0]).context("invalid closing issue")?;
            let result = Node {
                parent: Some(key(&parent)),
                complete: true,
                ..Node::default()
            };
            return Ok((result, vec![parent]));
        }
        let labels: Vec<String> = node["labels"]["nodes"]
            .as_array()
            .context("missing labels")?
            .iter()
            .filter_map(|l| l["name"].as_str().map(str::to_owned))
            .collect();
        ensure!(
            matches!(node["state"].as_str(), Some("OPEN" | "CLOSED")),
            "missing issue state"
        );
        ensure!(node.get("parent").is_some(), "missing parent connection");
        let parent = Parent {
            issue: issue.clone(),
            open: node["state"] == "OPEN",
            labels_complete: node["labels"]["totalCount"].as_u64() == Some(labels.len() as u64),
            labels,
        };
        let live = self.live(&parent).map_err(anyhow::Error::msg)?;
        let mut result = Node {
            live,
            complete: true,
            ..Node::default()
        };
        if !node["parent"].is_null() {
            let parent = reference(&node["parent"]).context("invalid parent")?;
            result.parent = Some(key(&parent));
            refs.push(parent);
        }
        let children = node["subIssues"]["nodes"]
            .as_array()
            .context("missing sub-issues")?;
        result.complete &= node["subIssues"]["totalCount"].as_u64() == Some(children.len() as u64);
        for child in children {
            let child = reference(child).context("invalid sub-issue")?;
            result.children.push(key(&child));
            refs.push(child);
        }
        if self.config.focus().as_ref().map(key).as_deref() == Some(key(issue).as_str()) {
            result.why.insert("focus".into());
        }
        if node["closedByPullRequestsReferences"]["totalCount"]
            .as_u64()
            .is_some_and(|n| n > 0)
        {
            result.why.insert("open-pr".into());
        }
        let comments = node["comments"]["nodes"]
            .as_array()
            .context("missing comments")?;
        result.complete &= node["comments"]["totalCount"].as_u64() == Some(comments.len() as u64);
        for comment in comments {
            if is(
                comment["author"]["login"].as_str(),
                &[&self.config.operator],
            ) && recent(
                comment["createdAt"].as_str(),
                self.now,
                time::Duration::days(COMMENT_DAYS),
            ) {
                result.why.insert("operator-comment".into());
            }
            let body = comment["body"].as_str().unwrap_or_default();
            if !is(comment["author"]["login"].as_str(), &[&self.config.bot])
                || !body.starts_with(&format!("<!-- {}:", plan::DISPATCH_MARKER))
            {
                continue;
            }
            let Some(run) = super::super::epics::record_run(body) else {
                continue;
            };
            if !self.room() {
                result.complete = false;
                break;
            }
            let run = self.client.get(
                &format!(
                    "{API_ROOT}repos/{}/actions/runs/{run}",
                    self.config.dispatch_repo
                ),
                None,
            );
            let run = match run {
                Ok(run) if run.status == 200 => run,
                _ => {
                    result.complete = false;
                    continue;
                }
            };
            if ["run_started_at", "updated_at"].iter().any(|field| {
                (*field != "updated_at" || run.body["status"] == "completed")
                    && recent(
                        run.body[*field].as_str(),
                        self.now,
                        time::Duration::hours(RUN_HOURS),
                    )
            }) {
                result.why.insert("recent-run".into());
            }
        }
        Ok((result, refs))
    }
}

pub(super) fn labels(node: &Value) -> Option<Vec<String>> {
    let nodes = node["labels"]["nodes"].as_array()?;
    if node["labels"]["totalCount"].as_u64()? != nodes.len() as u64 {
        return None;
    }
    nodes
        .iter()
        .map(|n| n["name"].as_str().map(str::to_ascii_lowercase))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn representative_sanitized_replay_requires_complete_parent_evidence() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../../fixtures/needs-you-chains-sanitized.json"
        ))
        .unwrap();
        let nodes: BTreeMap<_, _> = fixture["nodes"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(at, value)| {
                assert!(value.get("parent").is_some());
                (
                    at.clone(),
                    Node {
                        parent: value["parent"].as_str().map(str::to_owned),
                        live: value["live"].as_bool().unwrap(),
                        ..Node::default()
                    },
                )
            })
            .collect();
        let identities = fixture["identities"].as_array().unwrap();
        let mut unknown = 0;
        for identity in identities {
            let at = identity.as_str().unwrap();
            let resolved = chain(at, &nodes);
            unknown += usize::from(resolved.epic == "unknown");
            assert_eq!(
                json!([resolved.epic, resolved.top_epic]),
                fixture["expected"][at]
            );
        }
        assert!(unknown * 100 < identities.len() * 5);
        assert_eq!(
            chain("cgwalters-forge/composefs-rs#33", &nodes).epic,
            "cgwalters-forge/tracker#160"
        );
        // Missing evidence cannot be filled in to satisfy a percentage.
        let mut partial = nodes;
        partial.remove("cgwalters-forge/tracker#160");
        assert_eq!(
            chain("cgwalters-forge/composefs-rs#33", &partial),
            EpicChain::default()
        );
    }

    #[test]
    fn grouped_members_keep_individual_epics_when_detail_is_evicted() {
        let at = "2026-11-04T12:00:00Z";
        let on = on_github("a/ops", 1);
        let waits = (1..=2)
            .map(|number| poll::Wait {
                repository: "a/work".into(),
                number,
                head: "a".repeat(40),
                kind: "needs-human",
                reason: "inspect stopped PR".into(),
            })
            .collect();
        let reading = holds::Reading {
            operator: "operator",
            epics: true,
            considered: true,
            options: &[],
            on: &on,
            reviews: review_asks(true, Some(&(waits, BTreeSet::new())), at),
            proposals: Collected::off(),
            digest: Collected::off(),
            inbox: BTreeMap::new(),
            observed: holds::Observed::off(),
            summaries: (Read::Complete, BTreeMap::new()),
        };
        let mut snapshot = holds::snapshot(&[], at.into(), Default::default());
        let mut list = holds::needs_you(&snapshot, None, &reading);
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.identities.len(), 2);
        let nodes = BTreeMap::from([
            (
                "a/work#1".into(),
                Node {
                    parent: Some("b/plans#10".into()),
                    ..Node::default()
                },
            ),
            (
                "a/work#2".into(),
                Node {
                    parent: Some("c/plans#20".into()),
                    ..Node::default()
                },
            ),
            (
                "b/plans#10".into(),
                Node {
                    live: true,
                    ..Node::default()
                },
            ),
            (
                "c/plans#20".into(),
                Node {
                    live: true,
                    ..Node::default()
                },
            ),
        ]);
        decorate(&mut list, &nodes);
        // Force the group detail over the publication bound; only expensive
        // detail goes, never a member's own identity or epic context.
        list.items[0].recommendation = Some("x".repeat(holds::MAX_COMMENT_CHARS));
        snapshot.needs_you = Some(list);
        let (_, body) = holds::fit(snapshot).unwrap();
        let published = holds::published(&body).unwrap();
        let list = &published["needs_you"];
        assert!(list["items"].as_array().unwrap().is_empty());
        assert_eq!(list["count"], 2);
        assert_eq!(list["details_omitted"], 2);
        let members = list["identities"].as_array().unwrap();
        assert_eq!(members[0]["item"], "a/work#1");
        assert_eq!(members[0]["epic"], "b/plans#10");
        assert_eq!(members[1]["item"], "a/work#2");
        assert_eq!(members[1]["epic"], "c/plans#20");
    }

    #[test]
    fn cross_repository_chain_and_activity() {
        let mut nodes = BTreeMap::from([
            (
                "a/work#1".into(),
                Node {
                    parent: Some("b/plans#2".into()),
                    complete: true,
                    ..Node::default()
                },
            ),
            (
                "b/plans#2".into(),
                Node {
                    parent: Some("c/top#3".into()),
                    live: true,
                    children: vec!["a/work#1".into()],
                    complete: true,
                    ..Node::default()
                },
            ),
            (
                "c/top#3".into(),
                Node {
                    live: true,
                    children: vec!["b/plans#2".into()],
                    complete: true,
                    ..Node::default()
                },
            ),
        ]);
        assert_eq!(
            chain("a/work#1", &nodes),
            EpicChain {
                epic: "b/plans#2".into(),
                top_epic: "c/top#3".into()
            }
        );
        assert!(!activity("c/top#3", &nodes).active);
        nodes.insert(
            "a/alone#4".into(),
            Node {
                complete: true,
                ..Node::default()
            },
        );
        assert_eq!(chain("a/alone#4", &nodes).epic, "none");
        nodes
            .get_mut("b/plans#2")
            .unwrap()
            .why
            .insert("focus".into());
        assert!(!activity("c/top#3", &nodes).active);
        nodes.get_mut("b/plans#2").unwrap().why.clear();
        nodes.get_mut("c/top#3").unwrap().why.insert("focus".into());
        assert_eq!(activity("c/top#3", &nodes).active_why, ["focus"]);
        nodes.get_mut("c/top#3").unwrap().why.clear();
        for reason in ["recent-run", "open-pr", "operator-comment"] {
            nodes.get_mut("a/work#1").unwrap().why = BTreeSet::from([reason.into()]);
            let active = activity("c/top#3", &nodes);
            assert!(active.active && active.complete, "{reason}");
            assert_eq!(active.active_why, [reason]);
        }
        nodes.remove("b/plans#2");
        assert_eq!(chain("a/work#1", &nodes), EpicChain::default());
        assert!(!activity("c/top#3", &nodes).complete);
        assert_eq!(chain("c/top#3", &nodes).epic, "c/top#3");
        nodes.get_mut("c/top#3").unwrap().parent = Some("c/top#3".into());
        assert_eq!(chain("c/top#3", &nodes), EpicChain::default());
    }

    #[test]
    fn activity_windows_exclude_future_and_old_events() {
        let now = OffsetDateTime::parse("2026-11-04T12:00:00Z", &Rfc3339).unwrap();
        for window in [
            time::Duration::hours(RUN_HOURS),
            time::Duration::days(COMMENT_DAYS),
        ] {
            assert!(recent(Some(&stamp(now - window)), now, window));
            assert!(!recent(
                Some(&stamp(now - window - time::Duration::seconds(1))),
                now,
                window
            ));
            assert!(!recent(
                Some(&stamp(now + time::Duration::seconds(1))),
                now,
                window
            ));
        }
    }
}
