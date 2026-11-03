//! Additional inbox sources use the same read budget, private destination and
//! carry-forward as other needs-you sources. Source prose never authorizes a send.
use super::*;
use holds::{DECISIONS, DRAFTS};

const MAX_DRAFT_ISSUES: usize = 10;
const MAX_DECISIONS: usize = 50;
pub(crate) const DOCUMENT_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) { issue(number: $number) {
    url body createdAt lastEditedAt author { login } editor { login } state
  } }
}"#;

/// Only the outstanding section of the existing decisions format. Checked
/// answers remain outstanding until moved to Done; neither prose nor a checkbox
/// is permission for this publisher to resolve a decision.
fn document_asks(on: &ItemRef, body: &Comment) -> Result<Collected> {
    let mut collected = Collected::of(Read::Complete);
    for (n, entry) in crate::decisions::entries(&body.body)? {
        if entry.done {
            continue;
        }
        let section = &body.body[entry.start..entry.end];
        let (_, title) = section
            .lines()
            .next()
            .and_then(|l| l.split_once(' '))
            .and_then(|(_, rest)| rest.split_once(' '))
            .context("decision title")?;
        if collected.asks.len() == MAX_DECISIONS {
            collected.read = Read::Partial;
            collected.unread.insert(key(on));
            continue;
        }
        let (question, priority) = title.rsplit_once(" (").map_or((title, None), |(q, meta)| {
            let priority = meta
                .split_once(", blocks ")
                .map(|(p, _)| p)
                .filter(|p| ["P0", "P1", "P2"].contains(p));
            (q, priority.map(str::to_owned))
        });
        let action = Reference::new(RefKind::Issue, &on.repository, on.number);
        collected.asks.push(Ask {
            key: format!("{}/decision-{n}", key(on)),
            item: on.display(),
            source: DECISIONS.into(),
            kind: "document-decision".into(),
            priority,
            waiting_since: stamp(body.changed),
            ask: holds::one_line(question, holds::MAX_ASK).unwrap_or_default(),
            recommendation: None,
            target: action.url(),
            head: None,
            source_ref: action.clone(),
            action,
            carried: false,
            group: None,
            chain: holds::EpicChain::default(),
        });
    }
    Ok(collected)
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    pub(super) fn inbox(&mut self) -> BTreeMap<String, Collected> {
        let mut sources = BTreeMap::new();
        if let Some(at) = &self.config.decisions {
            let on = on_github(&at.repository, at.issue);
            let read = self.document(&on);
            sources.insert(DECISIONS.into(), self.inbox_result(DECISIONS, read));
        }
        if let Some(repository) = self.config.holds.as_ref().and_then(|h| h.drafts.as_deref()) {
            let read = self.drafts(repository);
            sources.insert(DRAFTS.into(), self.inbox_result(DRAFTS, read));
        }
        sources
    }

    fn inbox_result(&mut self, name: &str, read: Result<Collected>) -> Collected {
        match read {
            Ok(collected) => collected,
            Err(error) => {
                self.line(format!("needs-you {name} error: {error:#}"));
                Collected::of(Read::Unread)
            }
        }
    }

    fn document(&mut self, on: &ItemRef) -> Result<Collected> {
        ensure!(self.room(), "the pass's request budget or time is spent");
        let (owner, name) = on
            .repository
            .split_once('/')
            .context("decisions repository")?;
        let data = self.client.graphql(
            DOCUMENT_QUERY,
            json!({"owner": owner, "name": name, "number": on.number}),
        )?;
        let node = &data["repository"]["issue"];
        ensure!(node["state"] == "OPEN", "decisions issue is not open");
        let body = plan::comment(node)?;
        let writers = [self.config.operator.as_str(), self.config.bot.as_str()];
        // Threat: another writer edits a trusted issue to plant operator asks.
        ensure!(
            is(body.author.as_deref(), &writers) && !foreign_edit(&body, &writers),
            "decisions document is not the operator's or bot's as last edited"
        );
        document_asks(on, &body)
    }

    fn drafts(&mut self, repository: &str) -> Result<Collected> {
        ensure!(self.room(), "the pass's request budget or time is spent");
        let url = format!(
            "{API_ROOT}repos/{repository}/issues?state=open&labels=review-draft&sort=created&direction=asc&per_page=100"
        );
        let response = self.client.get(&url, None)?;
        ensure!(
            response.status == 200,
            "draft listing HTTP status {}",
            response.status
        );
        let issues = response
            .body
            .as_array()
            .context("draft issues are not a list")?;
        let mut collected = Collected::of(Read::Complete);
        if issues.len() >= 100 {
            collected.read = Read::Partial;
            collected.unread.insert(repository.to_ascii_lowercase());
        }
        for (i, issue) in issues
            .iter()
            .filter(|i| i.get("pull_request").is_none())
            .enumerate()
        {
            let number = issue["number"]
                .as_u64()
                .filter(|n| *n > 0)
                .context("draft issue number")?;
            let on = on_github(repository, number);
            if i >= MAX_DRAFT_ISSUES || !self.room() {
                collected.read = Read::Partial;
                collected.unread.insert(key(&on));
                continue;
            }
            match crate::review::inbox_draft(
                &self.client,
                &on,
                &self.config.operator,
                &self.config.bot,
            ) {
                Ok(Some((draft, source))) => {
                    let action = Reference::new(RefKind::Issue, repository, number);
                    let source_ref = Reference {
                        comment: comment_id(&source.url),
                        ..action.clone()
                    };
                    collected.asks.push(Ask {
                        key: format!("{}/draft", key(&on)),
                        item: on.display(),
                        source: DRAFTS.into(),
                        kind: "review-draft".into(),
                        priority: None,
                        waiting_since: stamp(source.changed),
                        ask: format!(
                            "Review the {} draft for {}#{} at {}?",
                            draft.event, draft.repository, draft.number, draft.head
                        ),
                        recommendation: None,
                        target: action.url(),
                        head: None,
                        source_ref,
                        action,
                        carried: false,
                        group: None,
                        chain: holds::EpicChain::default(),
                    });
                }
                Ok(None) => {}
                Err(error) => {
                    self.line(format!(
                        "needs-you drafts error: {}: {error:#}",
                        on.display()
                    ));
                    collected.read = Read::Partial;
                    collected.unread.insert(key(&on));
                }
            }
        }
        Ok(collected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outstanding_documents_ignore_examples_and_keep_checked_answers() {
        let on = on_github("o/ops", 1);
        let mut body = super::super::tests::comment(
            1,
            ("cgwalters", None),
            0,
            "# Decisions\n### D1 Ship? (P0, blocks o/r#1)\n- [x] Yes\n```\n### D2 Example\n```\n## Done\n### D3 Old\n",
        );
        let collected = document_asks(&on, &body).unwrap();
        assert_eq!(collected.read, Read::Complete);
        assert_eq!(collected.asks.len(), 1);
        assert_eq!(collected.asks[0].ask, "Ship?");
        assert_eq!(collected.asks[0].priority.as_deref(), Some("P0"));
        body.body = "### D1 First\n### D1 Duplicate".into();
        assert!(document_asks(&on, &body).is_err());
        body.body = "```\n### D1 Hidden".into();
        assert!(document_asks(&on, &body).is_err());
        body.body = (1..=MAX_DECISIONS + 1)
            .map(|n| format!("### D{n} Ask?\n"))
            .collect();
        let capped = document_asks(&on, &body).unwrap();
        assert_eq!(
            (capped.read, capped.asks.len()),
            (Read::Partial, MAX_DECISIONS)
        );
        assert!(capped.unread.contains(&key(&on)));
    }
}
