//! Conditional REST projections for schedule look-ahead only. Joined issue
//! facts, provenance, batch state and start rechecks keep their original
//! queries.
use super::*;

const TIMELINE_PAGE: usize = 100;
const WAIT_ASSIGNEES: usize = 20;

#[derive(Default)]
pub(super) struct Counts {
    charged: usize,
    resolved: usize,
    revalidated: usize,
    fallback: usize,
}

impl std::fmt::Display for Counts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} resolved by REST, {} charged REST requests, {} HTTP 304s, {} GraphQL fallbacks needed",
            self.resolved, self.charged, self.revalidated, self.fallback
        )
    }
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    /// What `read`, at most `gets` REST GETs, answers for `operation`, or
    /// none for its GraphQL fallback. It is tried only while the pass can
    /// still send that fallback after it.
    pub(super) fn schedule_rest<T>(
        &self,
        operation: &'static str,
        gets: usize,
        read: impl FnOnce(&Reader<'_>) -> Result<T>,
    ) -> Option<T> {
        let bounds = self.client.bounds;
        let kept = bounds.reserve.get().max(bounds.kept.get());
        if !self.config.schedule_rest_reads
            || bounds.stage.get() != Stage::Schedule
            || bounds.starting.get()
            || bounds.finishing.get()
            || bounds.rest_exhausted.get()
            || bounds.left.get() <= WRITE_RESERVE + kept + gets
        {
            return None;
        }
        let before = bounds.shared_responses.borrow().modified;
        let left = bounds.left.get();
        bounds.optional_rest.set(true);
        let result = read(&Reader(&self.client));
        bounds.optional_rest.set(false);
        let revalidated = bounds.shared_responses.borrow().modified - before;
        let mut counts = bounds.schedule_rest.borrow_mut();
        let counts = counts.entry(operation).or_default();
        counts.charged += left - bounds.left.get();
        counts.revalidated += revalidated;
        counts.resolved += usize::from(result.is_ok());
        counts.fallback += usize::from(result.is_err());
        result.ok()
    }
}

pub(super) struct Reader<'a>(&'a dyn Rest);

pub(super) struct Item {
    pub(super) pull: bool,
    body: Value,
}

fn issue_url(issue: &ItemRef) -> Result<String> {
    validate_repository(&issue.repository)?;
    ensure!(
        issue.host == DEFAULT_HOST && issue.number > 0,
        "not a GitHub item"
    );
    Ok(format!(
        "{API_ROOT}repos/{}/issues/{}",
        issue.repository, issue.number
    ))
}

impl Reader<'_> {
    pub(super) fn item(&self, issue: &ItemRef) -> Result<Item> {
        let body = self.get(&issue_url(issue)?)?.body;
        let pull = match body.get("pull_request") {
            None => false,
            Some(pr) if pr.is_object() => true,
            _ => bail!("invalid pull_request discriminator"),
        };
        let kind = if pull { "pull" } else { "issues" };
        let expected = format!(
            "https://{DEFAULT_HOST}/{}/{kind}/{}",
            issue.repository, issue.number
        );
        // A redirected issue (e.g. after transfer) must not supply state for
        // its old identity, or release an unrelated batch record.
        ensure!(
            body["number"].as_u64() == Some(issue.number)
                && text(&body, "/html_url").eq_ignore_ascii_case(&expected),
            "wrong item identity"
        );
        ensure!(
            matches!(body["state"].as_str(), Some("open" | "closed")),
            "unknown item state"
        );
        Ok(Item { pull, body })
    }

    fn get(&self, url: &str) -> Result<RestResponse> {
        let response = self.0.get(url, None)?;
        ensure!(
            response.status == 200,
            "GET returned HTTP {}",
            response.status
        );
        Ok(response)
    }

    fn timeline(&self, issue: &ItemRef) -> Result<Vec<Value>> {
        let url = format!(
            "{}/timeline?per_page={TIMELINE_PAGE}&page=1",
            issue_url(issue)?
        );
        let mut response = self.get(&url)?;
        let events = response
            .body
            .as_array_mut()
            .context("timeline is not an array")?;
        // A first page of a longer history could miss a reassignment or an
        // unlabel. Only complete short histories substitute for GraphQL's tail.
        ensure!(
            response.link.is_none() && events.len() < TIMELINE_PAGE,
            "timeline may be truncated"
        );
        ensure!(
            events
                .iter()
                .all(|e| e["event"].as_str().is_some_and(|s| !s.is_empty())),
            "timeline event has no type"
        );
        Ok(std::mem::take(events))
    }

    pub(super) fn assignment(
        &self,
        issue: &ItemRef,
        operator: &str,
        bot: &str,
    ) -> Result<Option<wait::Assignment>> {
        let item = self.item(issue)?;
        ensure!(!item.pull, "assignment target is a pull request");
        let assignees = item.body["assignees"]
            .as_array()
            .context("issue has no assignees")?;
        ensure!(
            assignees.len() <= WAIT_ASSIGNEES
                && assignees.iter().all(|a| !text(a, "/login").is_empty()),
            "unknown assignee coverage"
        );
        let mut nodes = Vec::new();
        let mut seen = BTreeSet::new();
        let mut latest = None;
        for event in self.timeline(issue)? {
            let kind = match text(&event, "/event") {
                "assigned" => "AssignedEvent",
                "unassigned" => "UnassignedEvent",
                "closed" => "ClosedEvent",
                _ => continue,
            };
            if kind != "ClosedEvent" {
                let assignee = text(&event, "/assignee/login");
                ensure!(!assignee.is_empty(), "assignment event has no assignee");
                if !assignee.eq_ignore_ascii_case(operator) {
                    continue;
                }
                // GraphQL's stable tie order is not established by REST.
                let at = event_time(&event)?;
                ensure!(seen.insert(at), "tied assignment events");
                latest = latest.max(Some((at, kind == "AssignedEvent")));
            }
            ensure!(
                !text(&event, "/actor/login").is_empty(),
                "assignment/close event has no actor"
            );
            nodes.push(json!({"__typename":kind, "createdAt":event["created_at"],
                "actor":event["actor"], "assignee":event["assignee"]}));
        }
        // A reassignment between the two GETs, or a timeline lagging the
        // issue, can make their snapshots contradict each other. Current
        // membership needs its assignment event in the timeline.
        let current = assignees
            .iter()
            .any(|a| text(a, "/login").eq_ignore_ascii_case(operator));
        ensure!(
            latest.is_some_and(|(_, assigned)| assigned) == current,
            "assignment snapshots disagree"
        );
        wait::assignment(
            &json!({"assignees":{"nodes":assignees,"totalCount":assignees.len()},
            "assignments":{"nodes":nodes,"pageInfo":{"hasPreviousPage":false}}}),
            operator,
            bot,
        )
    }

    pub(super) fn removal(&self, issue: &ItemRef) -> Result<Option<OffsetDateTime>> {
        let mut removed = None;
        for event in self.timeline(issue)? {
            if event["event"] != "unlabeled" {
                continue;
            }
            let label = text(&event, "/label/name");
            ensure!(!label.is_empty(), "label removal has no label");
            if label.eq_ignore_ascii_case(NEEDS_HUMAN) {
                removed = removed.max(Some(event_time(&event)?));
            }
        }
        Ok(removed)
    }
}

fn event_time(event: &Value) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(text(event, "/created_at"), &Rfc3339).context("timeline event time")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Response(Value, Option<&'static str>);

    impl Rest for Response {
        fn get(&self, _: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
            Ok(RestResponse {
                status: 200,
                body: self.0.clone(),
                link: self.1.map(str::to_owned),
                etag: None,
                last_modified: None,
            })
        }

        fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
            panic!("read-only fixture")
        }
    }

    fn issue() -> ItemRef {
        ItemRef {
            host: DEFAULT_HOST.into(),
            repository: "example/work".into(),
            number: 24,
        }
    }

    #[test]
    fn requires_matching_item_kind_identity_and_state() {
        let closed = json!({"number": 24, "html_url": "https://github.com/Example/Work/issues/24", "state": "closed"});
        let pull = json!({"number": 24, "html_url": "https://github.com/example/work/pull/24", "state": "open", "pull_request": {}});
        let mut unknown = closed.clone();
        unknown["state"] = json!("merged");
        for (body, expected) in [
            (closed, Some(false)),
            (pull, Some(true)),
            (
                json!({"number": 24, "html_url": "https://github.com/example/other/issues/24", "state": "closed"}),
                None,
            ),
            (unknown, None),
        ] {
            let rest = Response(body, None);
            assert_eq!(Reader(&rest).item(&issue()).ok().map(|i| i.pull), expected);
        }
    }

    #[test]
    fn verdict_reads_only_complete_label_removal_evidence() {
        let time = "2026-11-02T00:00:00Z";
        let removal =
            json!({"event": "unlabeled", "label":{"name":"Needs-Human"}, "created_at":time});
        let expected = OffsetDateTime::parse(time, &Rfc3339).unwrap();
        let short = json!([removal.clone(), {"event":"commented", "body":"unlabeled"}]);
        for (body, link, want) in [
            (short.clone(), None, Ok(Some(expected))),
            (
                json!([{"event":"unlabeled", "label":{"name":"other"}}]),
                None,
                Ok(None),
            ),
            (short, Some("<next>; rel=\"next\""), Err(())),
            (json!(vec![removal; TIMELINE_PAGE]), None, Err(())),
            (json!([{"event":"unlabeled"}]), None, Err(())),
        ] {
            assert_eq!(
                Reader(&Response(body, link))
                    .removal(&issue())
                    .map_err(|_| ()),
                want
            );
        }
    }
}
