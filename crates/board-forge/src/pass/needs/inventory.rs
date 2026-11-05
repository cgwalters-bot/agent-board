//! Cheap inventory reads before optional summaries and descendant activity.
use super::*;

/// Bound aliases and label/closing-issue connections in each request.
#[cfg(test)]
pub(super) const BATCH_SIZE: usize = 100;
#[cfg(test)]
const FIELDS: &str = r#"
  ... on Issue {
    issueState: state labels(first: 100) { nodes { name } totalCount }
    parent { number repository { nameWithOwner } }
  }
  ... on PullRequest {
    pullRequestState: state labels(first: 100) { nodes { name } totalCount }
    closingIssuesReferences(first: 2) { nodes { number repository { nameWithOwner } } totalCount }
  }
"#;

#[cfg(test)]
pub(super) fn batch_query(items: &[ItemRef]) -> (String, Value) {
    let mut declarations = Vec::new();
    let mut selections = Vec::new();
    let mut variables = serde_json::Map::new();
    for (n, item) in items.iter().enumerate() {
        let (owner, name) = item.repository.split_once('/').unwrap_or_default();
        declarations.push(format!("$o{n}: String!, $r{n}: String!, $n{n}: Int!"));
        selections.push(format!(
            "i{n}: repository(owner: $o{n}, name: $r{n}) {{ issueOrPullRequest(number: $n{n}) {{ {FIELDS} }} }}"
        ));
        variables.insert(format!("o{n}"), json!(owner));
        variables.insert(format!("r{n}"), json!(name));
        variables.insert(format!("n{n}"), json!(item.number));
    }
    (
        format!(
            "query InboxInventory({}) {{ {} }}",
            declarations.join(", "),
            selections.join(" ")
        ),
        Value::Object(variables),
    )
}

fn labels(node: &Value) -> Result<Vec<String>> {
    let labels = node["labels"]["nodes"]
        .as_array()
        .context("missing labels")?;
    ensure!(
        node["labels"]["totalCount"].as_u64() == Some(labels.len() as u64),
        "incomplete labels"
    );
    labels
        .iter()
        .map(|l| {
            l["name"]
                .as_str()
                .map(str::to_owned)
                .context("invalid label")
        })
        .collect()
}

/// A failed check leaves the decision unverified and the list incomplete, but
/// preserves its carry record so a later pass can retry without a full source read.
pub(in crate::pass) fn revalidate(
    list: &mut NeedsYou,
    nodes: &BTreeMap<String, Value>,
    proposal_label: Option<&str>,
) {
    let before = list.identities.clone();
    list.identities.retain(|i| {
        if !i.carried || i.source == "pass" {
            return true;
        }
        let Some(node) = nodes.get(&i.item.to_ascii_lowercase()) else {
            list.complete = false;
            return true;
        };
        if matches!(node["state"].as_str(), Some("CLOSED" | "MERGED")) {
            return false;
        }
        let Ok(labels) = labels(node) else {
            list.complete = false;
            return true;
        };
        if node["state"] != "OPEN" {
            list.complete = false;
            return true;
        }
        let required = match i.kind.as_str() {
            "needs-human" => Some(NEEDS_HUMAN),
            // Poll also calls unlabelled policy refusals needs-human. The
            // inventory preserves source evidence before grouping rewrites
            // questions; missing evidence requires a full source decision.
            "review-needs-human" if i.label_backed => Some(NEEDS_HUMAN),
            "operator-action" => proposal_label,
            _ => None,
        };
        required.is_none_or(|label| labels.iter().any(|l| l.eq_ignore_ascii_case(label)))
    });
    // One legacy card may display several independent semantic decisions.
    // Retiring its selected hold must not retire a different source's wait.
    let mut replacements = BTreeMap::new();
    let mut replacement_identities = Vec::new();
    for old in before.iter().filter(|i| !list.identities.contains(i)) {
        if nodes
            .get(&old.item.to_ascii_lowercase())
            .is_some_and(|n| matches!(n["state"].as_str(), Some("CLOSED" | "MERGED")))
        {
            continue;
        }
        let sources: Vec<_> = list
            .request_sources
            .iter()
            .filter(|s| {
                s.key == old.detail_key && (s.ask.source != old.source || s.ask.kind != old.kind)
            })
            .cloned()
            .collect();
        if let Some(source) = sources.first() {
            let mut identity = old.clone();
            identity.kind = source.ask.kind.clone();
            identity.source = source.ask.source.clone();
            identity.carried = source.ask.carried;
            identity.label_backed = false;
            identity.waiting_since = source.ask.waiting_since.clone();
            replacement_identities.push(identity);
            if let Some(card) = list.items.iter_mut().find(|a| a.key == old.detail_key) {
                let key = card.key.clone();
                *card = source.ask.clone();
                card.key = key;
            }
            replacements.insert(old.detail_key.clone(), sources);
        }
    }
    list.identities.extend(replacement_identities);
    list.request_sources.retain(|s| {
        replacements
            .get(&s.key)
            .is_none_or(|sources| sources.contains(s))
    });
    // Canonical counts lose a member per dropped identity, and a decision
    // (and group) per card none of whose identities is left.
    let dropped: Vec<_> = before
        .iter()
        .filter(|i| !list.identities.contains(i))
        .collect();
    let cards: BTreeSet<_> = dropped
        .iter()
        .map(|d| &d.detail_key)
        .filter(|key| !list.identities.iter().any(|i| &i.detail_key == *key))
        .collect();
    // A group's representative shares its card's key; the card, where it
    // still fits, or any other member tells it is a group.
    let groups = cards
        .iter()
        .filter(|key| {
            list.items
                .iter()
                .any(|a| &a.key == **key && a.group.is_some())
                || before
                    .iter()
                    .any(|i| &i.detail_key == **key && i.key != i.detail_key)
        })
        .count();
    let counts = &mut list.counts;
    counts.members = counts.members.saturating_sub(dropped.len());
    counts.decisions = counts.decisions.saturating_sub(cards.len());
    counts.groups = counts.groups.saturating_sub(groups);
    // Keep all projections in step, including grouped cards and carry sources.
    let identities = &list.identities;
    let trim = |ask: &mut Ask| {
        let members: Vec<_> = identities
            .iter()
            .filter(|i| i.detail_key == ask.key)
            .collect();
        if let Some(group) = &mut ask.group {
            let before = group.numbers.len();
            group.numbers.retain(|n| {
                members
                    .iter()
                    .any(|i| pull_ref(&i.item).is_some_and(|(_, number)| number == *n))
            });
            if group.numbers.len() != before {
                let repo = ask.action.repository.rsplit('/').next().unwrap_or_default();
                ask.ask = format!("{repo}: {} {}", group.numbers.len(), group.reason);
            }
            if let Some(first) = group.numbers.first() {
                if ask.action.number != *first {
                    // The new representative's head was not read by the old
                    // card; never transfer head-bound authority between PRs.
                    ask.head = None;
                    ask.action.head = None;
                    ask.source_ref.head = None;
                    ask.recommendation = None;
                }
                ask.action.number = *first;
                ask.source_ref.number = *first;
                ask.item = format!("{}#{first}", ask.action.repository);
            }
        }
        !members.is_empty()
    };
    list.items.retain_mut(trim);
    list.request_sources.retain_mut(|s| {
        let number = s.ask.action.number;
        // SourceAsk.key maps a source-specific alias to its displayed card;
        // ask.key remains the source's own stable carry identity.
        let original_key = s.ask.key.clone();
        s.ask.key = s.key.clone();
        let keep = trim(&mut s.ask);
        s.ask.key = original_key;
        if s.ask.action.number != number {
            s.observed_at = None;
            s.source_time = None;
        }
        keep
    });
    let mut replaced_requests_removed = 0;
    list.requests.retain_mut(|r| {
        if let Some(sources) = replacements.get(&r.key)
            && !sources
                .iter()
                .any(|s| holds::requests::identity(&s.ask, &r.forge) == r.identity)
        {
            replaced_requests_removed += 1;
            return false;
        }
        if let Some(card) = list
            .items
            .iter()
            .find(|a| a.key == r.key && a.group.is_some())
        {
            holds::requests::refresh_group(r, card);
        }
        identities.iter().any(|i| {
            i.detail_key == r.key
                && i.item
                    .eq_ignore_ascii_case(&format!("{}#{}", r.target.repository, r.target.number))
        })
    });
    list.counts.decisions = list
        .counts
        .decisions
        .saturating_sub(replaced_requests_removed);
    list.summary_cache
        .retain(|key, _| list.items.iter().any(|a| &a.key == key));
    list.count = identities.len() + list.omitted;
    list.details_omitted = list.omitted
        + identities
            .iter()
            .filter(|i| !list.items.iter().any(|a| a.key == i.detail_key))
            .count();
}

#[cfg(test)]
fn read_inventory(
    list: &NeedsYou,
    mut fetch: impl FnMut(&[ItemRef]) -> Result<Value>,
) -> (BTreeMap<String, Value>, Vec<String>) {
    let mut pending: BTreeMap<_, _> = list
        .identities
        .iter()
        .filter_map(|i| pull_ref(&i.item).map(|(r, n)| on_github(&r, n)))
        .map(|i| (key(&i), i))
        .collect();
    let mut attempted = BTreeSet::new();
    let mut nodes = BTreeMap::new();
    let mut errors = Vec::new();
    while !pending.is_empty() {
        let batch: Vec<_> = (0..BATCH_SIZE)
            .filter_map(|_| pending.pop_first().map(|(_, i)| i))
            .collect();
        attempted.extend(batch.iter().map(key));
        let data = match fetch(&batch) {
            Ok(data) => data,
            Err(error) => {
                errors.push(format!("needs-you inventory error: {error:#}"));
                break;
            }
        };
        for (n, issue) in batch.into_iter().enumerate() {
            let node = &data[format!("i{n}")]["issueOrPullRequest"];
            if !node.is_object() {
                continue;
            }
            let parent = if node.get("closingIssuesReferences").is_some() {
                (node["closingIssuesReferences"]["totalCount"] == 1)
                    .then(|| &node["closingIssuesReferences"]["nodes"][0])
            } else {
                node.get("parent")
            };
            if let Some(parent) = parent.and_then(epics::reference)
                && !attempted.contains(&key(&parent))
            {
                pending.insert(key(&parent), parent);
            }
            // IssueState and PullRequestState are different enum types. They
            // need distinct response names even in exclusive union fragments.
            let mut node = node.clone();
            let state = node
                .get("issueState")
                .or_else(|| node.get("pullRequestState"))
                .cloned()
                .unwrap_or(Value::Null);
            node["state"] = state;
            nodes.insert(key(&issue), node);
        }
    }
    (nodes, errors)
}

#[cfg(test)]
fn resolve_chains(
    list: &mut NeedsYou,
    inventory: &BTreeMap<String, Value>,
    mut live: impl FnMut(&Parent) -> Result<bool>,
) -> Vec<String> {
    let mut nodes = BTreeMap::new();
    let mut errors = Vec::new();
    for (at, raw) in inventory {
        let parsed = (|| -> Result<epics::Node> {
            let (repo, number) = pull_ref(at).context("invalid inventory identity")?;
            let mut node = epics::Node::default();
            if let Some(closing) = raw.get("closingIssuesReferences") {
                let count = closing["totalCount"]
                    .as_u64()
                    .context("missing closing issues")?;
                ensure!(count <= 1, "PR has no unique closing issue");
                if count == 1 {
                    node.parent =
                        Some(key(&epics::reference(&closing["nodes"][0])
                            .context("invalid closing issue")?));
                }
            } else {
                let parent = raw.get("parent").context("missing parent connection")?;
                if !parent.is_null() {
                    node.parent = Some(key(&epics::reference(parent).context("invalid parent")?));
                }
                ensure!(
                    matches!(raw["state"].as_str(), Some("OPEN" | "CLOSED")),
                    "missing state"
                );
                let parent = Parent {
                    issue: on_github(&repo, number),
                    open: raw["state"] == "OPEN",
                    labels_complete: true,
                    labels: labels(raw)?,
                };
                node.live = live(&parent)?;
            }
            Ok(node)
        })();
        match parsed {
            Ok(node) => {
                nodes.insert(at.clone(), node);
            }
            Err(error) => errors.push(format!(
                "needs-you inventory context error: {at}: {error:#}"
            )),
        }
    }
    epics::decorate(list, &nodes);
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliased(mut node: Value) -> Value {
        if let Some(fields) = node.as_object_mut()
            && let Some(state) = fields.remove("state")
        {
            let field = if fields.contains_key("closingIssuesReferences") {
                "pullRequestState"
            } else {
                "issueState"
            };
            fields.insert(field.into(), state);
        }
        node
    }

    #[test]
    fn union_state_response_names_and_decoding() {
        // GitHub's union members have incompatible enum return types:
        // Issue.state: IssueState!, PullRequest.state: PullRequestState!.
        // Distinct aliases are required by SameResponseShape, even though
        // the two fragments cannot both apply to one object.
        assert!(FIELDS.contains("issueState: state"));
        assert!(FIELDS.contains("pullRequestState: state"));
        let list = inventory(&["a/work#1".into(), "a/work#2".into()]);
        let (nodes, errors) = read_inventory(&list, |_| {
            Ok(json!({
                "i0":{"issueOrPullRequest":{"issueState":"CLOSED","parent":null}},
                "i1":{"issueOrPullRequest":{"pullRequestState":"MERGED","closingIssuesReferences":{"totalCount":0,"nodes":[]}}}
            }))
        });
        assert!(errors.is_empty());
        assert_eq!(nodes["a/work#1"]["state"], "CLOSED");
        assert_eq!(nodes["a/work#2"]["state"], "MERGED");
        let mut list = list;
        revalidate(&mut list, &nodes, None);
        assert_eq!(list.count, 0);
    }

    fn inventory(items: &[String]) -> NeedsYou {
        let on = on_github("a/ops", 1);
        let reading = holds::Reading {
            operator: "operator",
            epics: true,
            considered: false,
            options: &[],
            on: &on,
            reviews: Collected::off(),
            proposals: Collected::off(),
            digest: Collected::off(),
            inbox: BTreeMap::new(),
            observed: holds::Observed::off(),
            summaries: (Read::Complete, BTreeMap::new()),
        };
        let snapshot = holds::snapshot(&[], "2026-11-04T21:00:00Z".into(), Default::default());
        let mut list = holds::needs_you(&snapshot, None, &reading);
        list.identities = items
            .iter()
            .map(|item| holds::Identity {
                item: item.clone(),
                key: item.to_ascii_lowercase(),
                detail_key: item.to_ascii_lowercase(),
                kind: "needs-human".into(),
                source: "items".into(),
                carried: true,
                label_backed: false,
                waiting_since: "2026-11-04T20:31:00Z".into(),
                priority: None,
                epic: "unknown".into(),
                top_epic: "unknown".into(),
            })
            .collect();
        list.count = list.identities.len();
        list
    }

    #[test]
    fn public_data_replay_revalidates_carried_and_resolves_cross_repo_epics() {
        let replay: Value = serde_json::from_str(include_str!(
            "../../../../../fixtures/pass/inbox-public-2026-11-04.json"
        ))
        .unwrap();
        let items: Vec<String> = serde_json::from_value(replay["carried"].clone()).unwrap();
        let mut list = inventory(&items);
        let mut requests = 0;
        let (nodes, errors) = read_inventory(&list, |batch| {
            requests += 1;
            assert!(batch.len() <= BATCH_SIZE);
            let (query, vars) = batch_query(batch);
            assert!(query.starts_with("query InboxInventory("));
            assert_eq!(vars.as_object().unwrap().len(), batch.len() * 3);
            Ok(Value::Object(
                batch
                    .iter()
                    .enumerate()
                    .map(|(n, item)| {
                        (
                            format!("i{n}"),
                            json!({"issueOrPullRequest": aliased(replay["nodes"][key(item)].clone())}),
                        )
                    })
                    .collect(),
            ))
        });
        assert!(errors.is_empty());
        assert_eq!(requests, 2, "one identity batch, one shared ancestor batch");
        revalidate(&mut list, &nodes, None);
        let dropped: Vec<String> = serde_json::from_value(replay["dropped"].clone()).unwrap();
        for item in &items {
            assert_eq!(
                list.identities.iter().any(|i| &i.item == item),
                !dropped.contains(item),
                "{item}"
            );
        }
        // This replay isolates context resolution; authority checks remain the
        // production live() path and its existing authenticated-label tests.
        let errors = resolve_chains(&mut list, &nodes, |p| {
            Ok(p.open && p.labels.iter().any(|l| l == "epic"))
        });
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(list.count, 4);
        assert!(
            list.identities
                .iter()
                .filter(|i| i.epic == "unknown")
                .count()
                * 100
                < list.count * 5
        );
        for i in &list.identities {
            assert_eq!(
                i.epic,
                if i.item.ends_with("composefs-rs#33") {
                    "cgwalters-forge/tracker#160"
                } else {
                    "cgwalters-forge/tracker#1215"
                }
            );
        }
        // Every pass reads again: clearing a surviving label removes it even
        // when its detail was evicted, rather than refreshing its carry age.
        let mut changed = nodes.clone();
        changed.get_mut("cgwalters-forge/composefs-rs#33").unwrap()["labels"] =
            json!({"nodes":[],"totalCount":0});
        revalidate(&mut list, &changed, None);
        assert_eq!(list.count, 3);
        assert_eq!(list.details_omitted, 3);
    }

    #[test]
    fn batches_bounds_failures_and_incomplete_connections() {
        let items: Vec<_> = (1..=214).map(|n| format!("a/work#{n}")).collect();
        let original = inventory(&items);
        let mut requests = Vec::new();
        let (nodes, errors) = read_inventory(&original, |batch| {
            requests.push(batch.len());
            Ok(Value::Object(batch.iter().enumerate().map(|(n, _)| (format!("i{n}"), json!({"issueOrPullRequest":{
                "issueState":"OPEN", "labels":{"nodes":[{"name":"needs-human"}],"totalCount":1}, "parent":null
            }}))).collect()))
        });
        assert!(errors.is_empty());
        assert_eq!(requests, [100, 100, 14]);
        let mut list = original.clone();
        revalidate(&mut list, &nodes, None);
        assert_eq!(list.count, 214);
        for node in [
            Value::Null,
            json!({"state":"OPEN"}),
            json!({"state":"OPEN","labels":{"nodes":[],"totalCount":101}}),
        ] {
            let mut list = original.clone();
            revalidate(
                &mut list,
                &BTreeMap::from([("a/work#1".into(), node)]),
                None,
            );
            assert_eq!(list.count, 214);
            assert!(!list.complete);
        }
        let mut calls = 0;
        let (nodes, errors) = read_inventory(&original, |_| {
            calls += 1;
            bail!("budget spent")
        });
        assert_eq!(calls, 1);
        assert_eq!(errors.len(), 1);
        let mut list = original;
        revalidate(&mut list, &nodes, None);
        assert_eq!(list.count, 214);
        assert!(!list.complete);
    }

    #[test]
    fn unlabelled_policy_wait_survives_partial_and_unread_reviews() {
        let on = on_github("a/ops", 1);
        let at = "2026-11-04T21:00:00Z";
        let waits = vec![poll::Wait {
            review_bucket: None,
            approval: None,
            repository: "a/work".into(),
            number: 1,
            head: "a".repeat(40),
            kind: "needs-human",
            reason: "left to the operator: all fix rounds used".into(),
        }];
        let mut reading = holds::Reading {
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
        snapshot.needs_you = Some(holds::needs_you(&snapshot, None, &reading));
        let mut previous = serde_json::to_value(&snapshot).unwrap();
        let nodes = BTreeMap::from([(
            "a/work#1".into(),
            json!({"state":"OPEN","labels":{"nodes":[],"totalCount":0}}),
        )]);
        for read in [Read::Partial, Read::Unread] {
            reading.reviews = Collected::of(read);
            reading.reviews.unread.insert("a/work".into());
            let mut list = holds::needs_you(&snapshot, Some(&previous), &reading);
            revalidate(&mut list, &nodes, None);
            assert_eq!(list.count, 1, "{read:?}");
            assert!(list.identities[0].carried);
            assert_eq!(list.items.len(), 1);
            assert_eq!(list.requests.len(), 1);
            assert_eq!(list.request_sources.len(), 1);
            let mut evicted = list.clone();
            evicted.items.clear();
            evicted.requests.clear();
            evicted.request_sources.clear();
            revalidate(&mut evicted, &nodes, None);
            assert_eq!(evicted.count, 1, "missing evidence must not retire a wait");
            snapshot.needs_you = Some(list);
            previous = serde_json::to_value(&snapshot).unwrap();
        }
        reading.reviews = Collected::of(Read::Complete);
        let list = holds::needs_you(&snapshot, Some(&previous), &reading);
        assert_eq!(
            list.count, 0,
            "only a full source decision retires the wait"
        );
    }

    #[test]
    fn closed_group_member_is_removed_from_every_projection() {
        let on = on_github("a/ops", 1);
        let at = "2026-11-04T21:00:00Z";
        let waits = (1..=2)
            .map(|number| poll::Wait {
                review_bucket: None,
                approval: None,
                repository: "a/work".into(),
                number,
                head: "a".repeat(40),
                kind: "needs-human",
                reason: "a stranded draft; inspect".into(),
            })
            .collect();
        let mut reading = holds::Reading {
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
        for i in &mut list.identities {
            i.carried = true;
        }
        let group_target = list.items[0].target.clone();
        let counts = |decisions, groups, members| holds::Counts {
            decisions,
            groups,
            members,
        };
        assert_eq!(list.counts, counts(1, 1, 2));
        // Dropping the whole group at once is one decision, beside another
        // decision left out of the identities.
        let mut whole = list.clone();
        whole.counts = counts(2, 1, 3);
        let closed = BTreeMap::from([
            ("a/work#1".into(), json!({"state":"CLOSED"})),
            ("a/work#2".into(), json!({"state":"CLOSED"})),
        ]);
        revalidate(&mut whole, &closed, None);
        assert_eq!(whole.counts, counts(1, 0, 1));
        let nodes = BTreeMap::from([
            ("a/work#1".into(), json!({"state":"CLOSED"})),
            (
                "a/work#2".into(),
                json!({"state":"OPEN","labels":{"nodes":[{"name":"needs-human"}],"totalCount":1}}),
            ),
        ]);
        revalidate(&mut list, &nodes, None);
        assert_eq!(list.count, 1);
        assert_eq!(list.counts, counts(1, 1, 1));
        assert_eq!(list.identities[0].item, "a/work#2");
        let card = &list.items[0];
        assert_eq!(card.group.as_ref().unwrap().numbers, [2]);
        assert_eq!(card.action.number, 2);
        assert_eq!(card.head, None);
        assert_eq!(card.ask, "work: 1 drafts to inspect and promote");
        assert_eq!(card.target, group_target);
        assert_eq!(list.requests.len(), 1);
        assert!(list.requests.iter().all(|r| r.target.number != 1));
        let request = &list.requests[0];
        assert_eq!(request.target.number, 2);
        assert!(request.identity.starts_with("github.com/a/work#2/"));
        assert_eq!(request.target.head, None);
        assert_eq!(request.revision, None);
        assert_eq!(request.observed_at, None);
        assert_eq!(request.quality, "unverified-carried");
        assert_eq!(request.question, card.ask);
        assert!(
            request
                .sources
                .iter()
                .all(|r| r.number == 2 && r.head.is_none())
        );
        assert!(
            list.request_sources
                .iter()
                .all(|s| s.ask.action.number != 1)
        );
        // Publish the trimmed group, fail its inventory check on an unread
        // pass, then recover with the source still unread. Neither membership
        // nor the original waiting clock may disappear during the outage.
        let since = list.identities[0].waiting_since.clone();
        snapshot.needs_you = Some(list);
        let previous = serde_json::to_value(&snapshot).unwrap();
        reading.reviews = Collected::of(Read::Unread);
        let mut list = holds::needs_you(&snapshot, Some(&previous), &reading);
        revalidate(&mut list, &BTreeMap::new(), None);
        assert!(!list.complete);
        assert_eq!(list.count, 1);
        assert_eq!(list.requests[0].quality, "unverified-carried");
        snapshot.needs_you = Some(list);
        let previous = serde_json::to_value(&snapshot).unwrap();
        let mut list = holds::needs_you(&snapshot, Some(&previous), &reading);
        revalidate(&mut list, &nodes, None);
        assert_eq!(list.count, 1);
        assert_eq!(list.identities[0].waiting_since, since);
        assert_eq!(list.requests.len(), 1);
        assert_eq!(list.requests[0].target.number, 2);
        assert_eq!(list.requests[0].target.head, None);
        assert_eq!(list.requests[0].revision, None);
        // A cleared label does resolve this label-backed (not policy) wait.
        let cleared = BTreeMap::from([(
            "a/work#2".into(),
            json!({"state":"OPEN","labels":{"nodes":[],"totalCount":0}}),
        )]);
        let mut cleared_list = list.clone();
        revalidate(&mut cleared_list, &cleared, None);
        assert_eq!(cleared_list.count, 0);
        revalidate(&mut list, &closed, None);
        assert_eq!(list.count, 0);
        assert_eq!(list.counts, counts(0, 0, 0));
        assert!(
            list.items.is_empty() && list.requests.is_empty() && list.request_sources.is_empty()
        );
    }

    #[test]
    fn ambiguous_missing_and_cyclic_chains_stay_unknown() {
        let mut list = inventory(&["a/work#1".into()]);
        for raw in [
            json!({"closingIssuesReferences":{"totalCount":2,"nodes":[]}}),
            json!({"closingIssuesReferences":null}),
            json!({"state":"OPEN","labels":{"nodes":[],"totalCount":0}}),
            json!({"state":"OPEN","labels":{"nodes":[],"totalCount":0},"parent":{"number":1,"repository":{"nameWithOwner":"a/work"}}}),
            json!({"state":"OPEN","labels":{"nodes":[],"totalCount":0},"parent":{"number":2,"repository":{"nameWithOwner":"b/plans"}}}),
        ] {
            let nodes = BTreeMap::from([("a/work#1".into(), raw)]);
            resolve_chains(&mut list, &nodes, |_| Ok(false));
            assert_eq!(list.identities[0].epic, "unknown");
        }
        let nodes = BTreeMap::from([(
            "a/work#1".into(),
            json!({"closingIssuesReferences":{"totalCount":0,"nodes":[]}}),
        )]);
        assert!(resolve_chains(&mut list, &nodes, |_| Ok(false)).is_empty());
        assert_eq!(list.identities[0].epic, "none");
    }
}
