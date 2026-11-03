//! Duplicate planning suppression, opt-in and read-only. As with [`hold`], only
//! the operator's and the bot's closing PRs hold an issue, so that a visitor
//! cannot starve it; a mere mention must come from its plan's run.
use super::*;

/// The first open closing PR by one of `people`.
pub(super) fn trusted_pull<'f>(facts: &'f Facts, people: &[&str]) -> Option<&'f ClosingPull> {
    facts.pulls.iter().find(|p| is(p.author.as_deref(), people))
}

const LINK_PAGES: usize = 3;
pub(super) const LINKS: &str = r#"query($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) { issue(number: $number) {
    lastEditedAt updatedAt
    timelineItems(itemTypes: [CROSS_REFERENCED_EVENT], first: 100, after: $cursor) {
      nodes { ... on CrossReferencedEvent { source { ... on PullRequest {
        number state headRefName author { login } repository { nameWithOwner }
      } } } }
      pageInfo { hasNextPage endCursor }
    }
  } }
}"#;

fn run_pull(pull: &Value, issue: &ItemRef, bot: &str, runs: &BTreeSet<u64>) -> bool {
    pull["state"] == "OPEN"
        && is(
            pull.pointer("/author/login").and_then(Value::as_str),
            &[bot],
        )
        && text(pull, "/repository/nameWithOwner").eq_ignore_ascii_case(&issue.repository)
        && text(pull, "/headRefName")
            .strip_prefix(IMPLEMENT_BRANCH_PREFIX)
            .and_then(|id| id.parse::<u64>().ok())
            .is_some_and(|id| runs.contains(&id))
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    pub(super) fn active_hold(&self, rule: &str, held: Hold) -> Step {
        *self
            .report
            .triage_skipped
            .borrow_mut()
            .entry(rule.into())
            .or_default() += 1;
        Step::Hold(held)
    }

    pub(super) fn active_work(
        &self,
        facts: &Facts,
        comments: &[plan::Comment],
        target: &Target,
    ) -> Result<Option<(&'static str, Hold)>> {
        let writers = self.writers();
        let mut runs = BTreeSet::new();
        let mut research = None;
        let newest = plan::newest_plan(comments, &writers).ok();
        for comment in comments {
            // Apply the normal author/editor/plan validation even to older plans.
            let Ok((plan, from)) = plan::newest_plan(std::slice::from_ref(comment), &writers)
            else {
                continue;
            };
            if plan.repo != facts.issue.repository || plan.item != facts.issue.number {
                continue;
            }
            let id = plan::plan_id(from)?;
            let records = plan::records(comments, &writers, &id);
            if plan.kind == "implement" {
                runs.extend(
                    records
                        .iter()
                        .filter_map(|(_, c)| epics::record_run(&c.body)),
                );
            }
            if plan.kind == "research" {
                let record = records
                    .iter()
                    .max_by_key(|(_, c)| c.changed)
                    .map(|(_, c)| *c);
                let latest = newest.as_ref().is_some_and(|(_, c)| c.url == from.url);
                if (latest || record.is_some())
                    && research
                        .as_ref()
                        .is_none_or(|(old, _, _): &(&plan::Comment, _, _)| {
                            old.changed < from.changed
                        })
                {
                    research = Some((from, record, latest));
                }
            }
        }
        if runs.is_empty() && research.is_none() {
            return Ok(None);
        }
        let (owner, name) = facts
            .issue
            .repository
            .split_once('/')
            .context("issue repository")?;
        let mut cursor = Value::Null;
        let mut edited = None;
        let mut updated = None;
        for page in 0..LINK_PAGES {
            let data = self.kept().graphql(
                LINKS,
                json!({
                    "owner": owner, "name": name, "number": facts.issue.number, "cursor": cursor
                }),
            )?;
            let issue = data.pointer("/repository/issue").context("missing issue")?;
            if page == 0 {
                edited = issue["lastEditedAt"]
                    .as_str()
                    .map(|s| OffsetDateTime::parse(s, &Rfc3339))
                    .transpose()?;
                updated = issue["updatedAt"]
                    .as_str()
                    .map(|s| OffsetDateTime::parse(s, &Rfc3339))
                    .transpose()?;
            }
            let links = &issue["timelineItems"];
            for event in links["nodes"]
                .as_array()
                .context("missing cross references")?
            {
                let pull = &event["source"];
                if run_pull(pull, &facts.issue, &self.config.bot, &runs) {
                    let number = pull["number"].as_u64().context("pull request number")?;
                    let why = format!("pull request {}#{number} is open", facts.issue.repository);
                    return Ok(Some(("pull-request", Hold::new(HoldKind::PrOpen, why))));
                }
            }
            if links
                .pointer("/pageInfo/hasNextPage")
                .and_then(Value::as_bool)
                == Some(false)
            {
                break;
            }
            ensure!(
                page + 1 < LINK_PAGES,
                "cross references exceed {LINK_PAGES} pages"
            );
            cursor = links
                .pointer("/pageInfo/endCursor")
                .filter(|v| v.is_string())
                .context("cross references have no cursor")?
                .clone();
        }
        let Some((from, record, latest)) = research else {
            return Ok(None);
        };
        let mut since = from.changed;
        if let Some(record) = record {
            since = since.max(record.changed);
            if let Some(id) = epics::record_run(&record.body) {
                let url = format!("{API_ROOT}repos/{}/actions/runs/{id}", target.caller);
                let run = self.kept().get(&url, None)?;
                ensure!(run.status == 200, "research run: HTTP {}", run.status);
                if ACTIVE_RUNS.contains(&text(&run.body, "/status")) {
                    return Ok(Some((
                        "research",
                        Hold::new(
                            HoldKind::InFlight,
                            format!("research run {id} is answering this issue"),
                        ),
                    )));
                }
                since = since.max(OffsetDateTime::parse(
                    text(&run.body, "/updated_at"),
                    &Rfc3339,
                )?);
            }
        } else if !latest {
            return Ok(None);
        }
        // An accepted implementation of the findings is new work, not another
        // triage of the research request, once the research run above has
        // ended. Its own run/PR guards still apply.
        if newest
            .as_ref()
            .is_some_and(|(plan, c)| plan.kind == "implement" && c.changed > from.changed)
        {
            return Ok(None);
        }
        if research_changed([edited, updated], comments, &self.config.bot, since) {
            return Ok(None);
        }
        Ok(Some((
            "research",
            Hold::new(
                HoldKind::TriageCooldown,
                "its research plan or run answers this issue; triaged again once the issue changes after it",
            ),
        )))
    }
}

/// Whether the issue changed after `since`: its body edit or latest update
/// time, or a comment someone other than the bot wrote or edited. A later
/// bot comment counts too, as the latest update time alone cannot tell it
/// from a label, title or state change made before it.
fn research_changed(
    [edited, updated]: [Option<OffsetDateTime>; 2],
    comments: &[plan::Comment],
    bot: &str,
    since: OffsetDateTime,
) -> bool {
    [edited, updated].into_iter().flatten().any(|at| at > since)
        || comments.iter().any(|c| {
            (!is(c.author.as_deref(), &[bot])
                || c.editor
                    .as_deref()
                    .is_some_and(|editor| !editor.eq_ignore_ascii_case(bot)))
                && c.changed > since
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn research_waits_for_changed_inputs_not_its_own_answer() {
        let since = OffsetDateTime::UNIX_EPOCH;
        let later = since + time::Duration::hours(1);
        // The bot's answer, written by the run `since` covers, is no change.
        for (edited, updated, (author, editor, changed), expected) in [
            (None, None, ("bot", None, since), false),
            (None, Some(since), ("bot", None, since), false),
            (Some(later), Some(later), ("bot", None, since), true),
            (None, Some(later), ("operator", None, later), true),
            (None, Some(later), ("bot", Some("operator"), later), true),
            // A label, title or state update alone.
            (None, Some(later), ("bot", None, since), true),
            // Such an update, then a bot comment that is the latest update:
            // the one timestamp left cannot rule the first out.
            (None, Some(later), ("bot", None, later), true),
        ] {
            let comment = plan::Comment {
                url: String::new(),
                body: String::new(),
                author: Some(author.into()),
                edited: editor.is_some(),
                editor: editor.map(str::to_owned),
                changed,
            };
            assert_eq!(
                research_changed([edited, updated], &[comment], "bot", since),
                expected,
                "{edited:?} {updated:?} {author} {editor:?} {changed}"
            );
        }
    }

    #[test]
    fn linked_drafts_require_the_issues_plan_run() {
        let issue = ItemRef {
            host: DEFAULT_HOST.into(),
            repository: "example/work".into(),
            number: 7,
        };
        for (author, branch, state, repo, expected) in [
            (
                "bot",
                "dispatch/implement/agent-run-42",
                "OPEN",
                "example/work",
                true,
            ),
            (
                "someone-else",
                "dispatch/implement/agent-run-42",
                "OPEN",
                "example/work",
                false,
            ),
            (
                "bot",
                "dispatch/implement/agent-run-43",
                "OPEN",
                "example/work",
                false,
            ),
            (
                "bot",
                "dispatch/implement/agent-run-42",
                "CLOSED",
                "example/work",
                false,
            ),
            (
                "bot",
                "dispatch/implement/agent-run-42",
                "MERGED",
                "example/work",
                false,
            ),
            (
                "bot",
                "dispatch/implement/agent-run-42",
                "OPEN",
                "other/work",
                false,
            ),
        ] {
            for draft in [false, true] {
                let pull = json!({"author": {"login": author}, "headRefName": branch,
                    "state": state, "repository": {"nameWithOwner": repo}, "isDraft": draft});
                assert_eq!(
                    run_pull(&pull, &issue, "bot", &BTreeSet::from([42])),
                    expected,
                    "{pull}"
                );
            }
        }
    }
}
