//! With [`Config::assign_holds`] (agent-board#727), stage 4 waits on the
//! operator by assigning them rather than by the [`NEEDS_HUMAN`] label, so
//! that the wait shows in GitHub's own assignment lists and notifications and
//! the operator never edits labels. The wait is read from GitHub alone: the
//! bot's newest assignment of the operator in the issue's timeline, standing
//! while no one has assigned or unassigned them since, and the operator's
//! comments after it. Any response releases it: a comment or a close of
//! theirs, or an unassignment by anyone but the bot (a closed issue is no
//! candidate, so a close releases it once the issue is open again). While the
//! assignment stands, the pass takes it off itself and goes on with the
//! issue. An assignment that is not the bot's, such as the operator's own, is
//! no wait; but so that the bot's is always recorded, the pass unassigns the
//! operator before it assigns them, which replaces their own. A candidate labelled
//! [`NEEDS_HUMAN`] is moved over: the operator assigned and the label taken
//! off.
use super::*;

/// The issue's assignees and its assignment and close events, the newest
/// 100. An older assignment is not read: where none of the bot's is among
/// them and the operator is assigned, the wait cannot be told and
/// [`assignment`] fails, rather than let a flood of events release it.
pub(super) const WAIT_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) { issue(number: $number) {
    assignees(first: 20) { nodes { login } totalCount }
    assignments: timelineItems(itemTypes: [ASSIGNED_EVENT, UNASSIGNED_EVENT, CLOSED_EVENT], last: 100) {
      pageInfo { hasPreviousPage }
      nodes {
        __typename
        ... on AssignedEvent { createdAt actor { login } assignee { ... on User { login } } }
        ... on UnassignedEvent { createdAt actor { login } assignee { ... on User { login } } }
        ... on ClosedEvent { createdAt actor { login } }
      }
    }
  } }
}"#;

/// The writes of an assignment ([`Scheduler::assign`]): the unassignment
/// before it and the assignment.
pub(super) const ASSIGN_WRITES: usize = 2;

/// The bot's newest assignment of the operator, as [`WAIT_QUERY`] read it.
#[derive(Debug, PartialEq)]
pub(super) struct Assignment {
    pub since: OffsetDateTime,
    /// The operator is still assigned, and no one assigned or unassigned
    /// them since.
    pub standing: bool,
    /// Someone but the bot unassigned them since.
    pub unassigned: bool,
    /// The operator closed the issue since.
    pub closed: bool,
}

/// What released an [`Assignment`].
#[derive(Debug)]
pub(super) enum Response<'c> {
    /// The operator's newest comment since, which a triage takes as context.
    Comment(&'c plan::Comment),
    /// Someone but the bot unassigned the operator: only those who may
    /// triage the repository can.
    Unassigned,
    /// The operator closed the issue, open again since.
    Closed,
}

impl Response<'_> {
    /// What happened, for a line.
    pub(super) fn said(&self, operator: &str) -> String {
        match self {
            Response::Comment(c) => format!("{} answered its assignment", c.url),
            Response::Unassigned => format!("{operator} was unassigned"),
            Response::Closed => "it was closed".into(),
        }
    }
}

/// The bot's newest assignment of `operator` in `node`, [`WAIT_QUERY`]'s
/// issue, or `None` where there is none among the events read.
pub(super) fn assignment(node: &Value, operator: &str, bot: &str) -> Result<Option<Assignment>> {
    let nodes = node["assignments"]["nodes"]
        .as_array()
        .context("issue has no assignment events")?;
    let mut events = Vec::new();
    let mut closes = Vec::new();
    for event in nodes {
        let at = OffsetDateTime::parse(text(event, "/createdAt"), &Rfc3339)
            .context("assignment event time")?;
        let actor = event.pointer("/actor/login").and_then(Value::as_str);
        let by_bot = is(actor, &[bot]);
        let assigned = match text(event, "/__typename") {
            "AssignedEvent" => true,
            "UnassignedEvent" => false,
            // Only the operator's: an issue's author may close and reopen it.
            "ClosedEvent" => {
                closes.extend(is(actor, &[operator]).then_some(at));
                continue;
            }
            other => bail!("unexpected assignment event {other:?}"),
        };
        let assignee = event.pointer("/assignee/login").and_then(Value::as_str);
        if is(assignee, &[operator]) {
            events.push((assigned, at, by_bot));
        }
    }
    // GitHub lists the timeline oldest first; a tie keeps that order.
    events.sort_by_key(|(_, at, _)| *at);
    let assignees = node["assignees"]["nodes"]
        .as_array()
        .context("issue has no assignees")?;
    let total = node["assignees"]["totalCount"].as_u64();
    // An operator past the assignees read may be assigned: still waiting.
    let assigned = total != Some(assignees.len() as u64)
        || assignees
            .iter()
            .any(|a| is(a["login"].as_str(), &[operator]));
    let Some(n) = events
        .iter()
        .rposition(|&(assigned, _, by_bot)| assigned && by_bot)
    else {
        let truncated = node
            .pointer("/assignments/pageInfo/hasPreviousPage")
            .and_then(Value::as_bool)
            .context("assignment events have no page info")?;
        ensure!(
            !(truncated && assigned),
            "{operator} is assigned, but no assignment of theirs by {bot} is among the newest {} events",
            nodes.len()
        );
        return Ok(None);
    };
    let (since, later) = (events[n].1, &events[n + 1..]);
    Ok(Some(Assignment {
        since,
        standing: assigned && later.is_empty(),
        unassigned: later
            .iter()
            .any(|&(assigned, _, by_bot)| !assigned && !by_bot),
        closed: closes.iter().any(|at| *at > since),
    }))
}

/// The response to `assignment` among `comments`, if any: the operator's
/// newest comment created after it, unedited or edited by a writer only,
/// else an unassignment, else a close.
pub(super) fn response<'c>(
    assignment: &Assignment,
    comments: &'c [plan::Comment],
    (operator, writers): (&str, &[&str]),
) -> Option<Response<'c>> {
    let answer = comments
        .iter()
        .filter(|c| {
            c.created > assignment.since
                && is(c.author.as_deref(), &[operator])
                && ((!c.edited && c.editor.is_none()) || is(c.editor.as_deref(), writers))
        })
        .max_by_key(|c| c.created);
    match answer {
        Some(c) => Some(Response::Comment(c)),
        None if assignment.unassigned => Some(Response::Unassigned),
        None => assignment.closed.then_some(Response::Closed),
    }
}

/// The writes of a [`Scheduler::release_harness`]: the unassignment or label
/// removal, and its comment.
const RELEASE_WRITES: usize = 2;

/// What the comment of a release says after [`RELEASE_MARKER`].
const RELEASE_TEXT: &str = "The board pass released this hold: it is our own infrastructure and not an operator decision (credentials, budget, rulesets, first upstream posting or bounds), so the pass plans the work.";

/// The hold a [`Scheduler::release_harness`] takes off.
#[derive(Clone, Copy)]
enum Release {
    Assignment,
    Label,
}

/// What [`Scheduler::operator_wait`] made of a candidate.
enum Waited {
    Held(Hold),
    /// Not waiting: considered as before, its line ending with the note.
    Go(Option<String>),
    /// [`Go`](Waited::Go), its [`NEEDS_HUMAN`] label taken off.
    Unlabelled(String),
}

/// An issue held because what its wait needs could not be read.
fn unreadable(error: anyhow::Error) -> Hold {
    let why = format!("its assignments could not be read: {error:#}");
    Hold::new(HoldKind::Unreadable, why)
}

/// An issue held on the bot's assignment of `operator` since `since`.
fn waiting(operator: &str, since: &str) -> Hold {
    let why = format!("assigned to {operator} since {since}; any response of theirs releases it");
    Hold::new(HoldKind::NeedsHuman, why)
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    /// [`Self::consider`], first held on or released from the operator's
    /// wait where [`Config::assign_holds`] is set. A write that fails is the
    /// candidate's failure, not a hold.
    pub(super) fn consider_waiting(
        &self,
        facts: &Facts,
        board: &BoardRead,
        slots: &Slots,
        snapshot: Option<&Value>,
        target: &Target,
    ) -> Result<Step> {
        let (note, unlabelled) = match self.config.assign_holds && facts.open {
            false => (None, false),
            true => match self.operator_wait(facts)? {
                Waited::Held(held) => return Ok(Step::Hold(held)),
                Waited::Go(note) => (note, false),
                Waited::Unlabelled(note) => (Some(note), true),
            },
        };
        // A label taken off no longer holds it.
        let released;
        let facts = match unlabelled {
            true => {
                released = Facts {
                    labels: (facts.labels.iter())
                        .filter(|l| !l.eq_ignore_ascii_case(NEEDS_HUMAN))
                        .cloned()
                        .collect(),
                    ..facts.clone()
                };
                &released
            }
            false => facts,
        };
        let mut step = self.consider(facts, board, slots, snapshot, target)?;
        if let Some(note) = note {
            step.note(&note);
        }
        Ok(step)
    }

    /// Whether `facts` waits on the operator: labelled [`NEEDS_HUMAN`], which
    /// moves to an assignment, or the bot's assignment standing with no
    /// response since. A response takes the assignment off, one write under
    /// `--max-writes`, and the issue goes on.
    fn operator_wait(&self, facts: &Facts) -> Result<Waited> {
        let config = self.config;
        let operator = config.operator.as_str();
        let issue = &facts.issue;
        if facts.labels_complete && facts.labelled(NEEDS_HUMAN) {
            if self.harness_item(facts) {
                let comments = match plan::issue_comments(&self.client, issue) {
                    Ok(comments) => comments,
                    Err(error) => return Ok(Waited::Held(unreadable(error))),
                };
                if self.releasable(facts, &comments) {
                    let why = format!("labelled {NEEDS_HUMAN}");
                    return self.release_harness(issue, why, Release::Label);
                }
            }
            return self.migrate(issue).map(Waited::Held);
        }
        let (assigned, comments) = match self.standing(issue) {
            Ok(Some(standing)) => standing,
            Ok(None) => return Ok(Waited::Go(None)),
            Err(held) => return Ok(Waited::Held(held)),
        };
        let since = assigned.since.format(&Rfc3339).unwrap_or_default();
        let Some(response) = response(&assigned, &comments, (operator, &self.writers())) else {
            if self.releasable(facts, &comments) {
                let why = format!("assigned to {operator} since {since}");
                return self.release_harness(issue, why, Release::Assignment);
            }
            return Ok(Waited::Held(waiting(operator, &since)));
        };
        let said = response.said(operator);
        if let Some(held) = self.caps.check(Kind::Write) {
            let why = format!("{said}; {held}: unassign {operator}");
            return Ok(Waited::Held(Hold::new(HoldKind::NeedsHuman, why)));
        }
        self.caps.count(Kind::Write);
        if self.apply {
            self.unassign(issue)?;
        }
        Ok(Waited::Go(Some(format!(
            "; {said} (assigned {since}), so unassigned {operator}"
        ))))
    }

    /// The wait `why` on harness work released (agent-board#778): the
    /// operator unassigned or the label taken off, and one short comment
    /// saying so, two writes under `--max-writes`, at most
    /// [`RELEASES_PER_PASS`] a pass. The issue then goes on, its triage
    /// verdict planned again rather than asked ([`Self::follow_up`]), so that
    /// the next pass does not hold it again. The bot's own unassignment is
    /// no response, nor its comment an answer
    /// ([`Self::verdict_answers`]).
    fn release_harness(&self, issue: &ItemRef, why: String, what: Release) -> Result<Waited> {
        let operator = &self.config.operator;
        let act = match what {
            Release::Assignment => format!("unassign {operator}"),
            Release::Label => format!("take off {NEEDS_HUMAN}"),
        };
        let held = |reason: String| Ok(Waited::Held(Hold::new(HoldKind::NeedsHuman, reason)));
        if self.released.get() >= RELEASES_PER_PASS {
            return held(format!(
                "{why}; harness work, released in a later pass (at most {RELEASES_PER_PASS} a pass): {act}"
            ));
        }
        if let Some(cap) = self.caps.check_many(Kind::Write, RELEASE_WRITES) {
            return held(format!("{why}; {cap}: {act}"));
        }
        self.caps.count_many(Kind::Write, RELEASE_WRITES);
        self.released.set(self.released.get() + 1);
        if self.apply {
            match what {
                Release::Assignment => self.unassign(issue)?,
                Release::Label => {
                    let url = format!(
                        "{API_ROOT}repos/{}/issues/{}/labels/{NEEDS_HUMAN}",
                        issue.repository, issue.number
                    );
                    // 404: someone took it off meanwhile.
                    plan::send(&self.client, "DELETE", &url, &Value::Null, 404)?;
                }
            }
            let body = format!(
                "{RELEASE_MARKER}\n{RELEASE_TEXT}\n\n{}",
                crate::Footer::default()
            );
            let url = format!(
                "{API_ROOT}repos/{}/issues/{}/comments",
                issue.repository, issue.number
            );
            plan::send(&self.client, "POST", &url, &json!({"body": body}), 0)?;
        }
        let note = format!("; {why}, harness work: {act}, released for planning");
        Ok(match what {
            Release::Assignment => Waited::Go(Some(note)),
            Release::Label => Waited::Unlabelled(note),
        })
    }

    /// The bot's standing assignment of the operator on `issue`, with the
    /// issue's comments, or a hold where either could not be read.
    fn standing(&self, issue: &ItemRef) -> Result<Option<(Assignment, Vec<plan::Comment>)>, Hold> {
        let config = self.config;
        let read = self
            .issue_read(issue, WAIT_QUERY)
            .and_then(|node| assignment(&node, &config.operator, &config.bot));
        let assigned = match read {
            Ok(Some(assigned)) if assigned.standing => assigned,
            Ok(_) => return Ok(None),
            Err(error) => return Err(unreadable(error)),
        };
        match plan::issue_comments(&self.client, issue) {
            Ok(comments) => Ok(Some((assigned, comments))),
            Err(error) => Err(unreadable(error)),
        }
    }

    /// Whether `facts`, an issue the pass would start work on other than
    /// as a candidate (its plan's destination, or either at a plan's
    /// start's recheck; a revisit restates a held question), waits on the operator's response to the bot's assignment,
    /// where [`Config::assign_holds`] is set. Only read: the wait is taken
    /// off when the issue is itself a candidate.
    pub(super) fn assignment_hold(&self, facts: &Facts) -> Option<Hold> {
        if !self.config.assign_holds || !facts.open {
            return None;
        }
        let operator = self.config.operator.as_str();
        let (assigned, comments) = match self.standing(&facts.issue) {
            Ok(standing) => standing?,
            Err(held) => return Some(held),
        };
        if response(&assigned, &comments, (operator, &self.writers())).is_some() {
            return None;
        }
        let since = assigned.since.format(&Rfc3339).unwrap_or_default();
        Some(waiting(operator, &since))
    }

    /// A candidate labelled [`NEEDS_HUMAN`] moved to the operator's
    /// assignment: assigned first, so that a failed removal leaves the label
    /// for the next pass to move again, three writes under `--max-writes`.
    fn migrate(&self, issue: &ItemRef) -> Result<Hold> {
        let operator = &self.config.operator;
        let act = format!("assign {operator} in its place and take it off");
        let why = format!("labelled {NEEDS_HUMAN}");
        if let Some(held) = self.caps.check_many(Kind::Write, ASSIGN_WRITES + 1) {
            return Ok(Hold::new(
                HoldKind::NeedsHuman,
                format!("{why}; {held}: {act}"),
            ));
        }
        self.caps.count_many(Kind::Write, ASSIGN_WRITES + 1);
        if self.apply {
            self.assign(issue)?;
            let url = format!(
                "{API_ROOT}repos/{}/issues/{}/labels/{NEEDS_HUMAN}",
                issue.repository, issue.number
            );
            // 404: someone took it off meanwhile.
            plan::send(&self.client, "DELETE", &url, &Value::Null, 404)?;
        }
        Ok(Hold::new(HoldKind::NeedsHuman, format!("{why}; {act}")))
    }

    fn assignees_url(issue: &ItemRef) -> String {
        format!(
            "{API_ROOT}repos/{}/issues/{}/assignees",
            issue.repository, issue.number
        )
    }

    fn unassign(&self, issue: &ItemRef) -> Result<()> {
        let operator = &self.config.operator;
        let body = json!({"assignees": [operator]});
        plan::send(
            &self.client,
            "DELETE",
            &Self::assignees_url(issue),
            &body,
            0,
        )?;
        Ok(())
    }

    /// Assigns the operator to `issue`, [`ASSIGN_WRITES`] writes: unassigned
    /// first, as GitHub records no event for a login already assigned, and
    /// only the bot's event is a wait. GitHub answers a login it may not
    /// assign by leaving it out rather than with an error, so that is one.
    pub(super) fn assign(&self, issue: &ItemRef) -> Result<()> {
        let operator = &self.config.operator;
        self.unassign(issue)?;
        let body = json!({"assignees": [operator]});
        let url = Self::assignees_url(issue);
        let (_, answer) = plan::send(&self.client, "POST", &url, &body, 0)?;
        let assignees = answer["assignees"]
            .as_array()
            .map_or(&[][..], Vec::as_slice);
        ensure!(
            assignees
                .iter()
                .any(|a| is(a["login"].as_str(), &[operator])),
            "{operator} could not be assigned to {}: not assignable there",
            issue.display()
        );
        Ok(())
    }

    /// The response to the bot's assignment for the verdict `verdict`,
    /// among the `comments` after it: none where the bot's newest assignment
    /// of the operator came before the verdict.
    pub(super) fn assignment_answer<'c>(
        &self,
        issue: &ItemRef,
        verdict: &plan::Comment,
        comments: &'c [plan::Comment],
    ) -> Result<Option<Response<'c>>> {
        let config = self.config;
        let node = self.issue_read(issue, WAIT_QUERY)?;
        let Some(assigned) = assignment(&node, &config.operator, &config.bot)? else {
            return Ok(None);
        };
        if assigned.since < verdict.changed {
            return Ok(None);
        }
        Ok(response(
            &assigned,
            comments,
            (&config.operator, &self.writers()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> OffsetDateTime {
        OffsetDateTime::parse(s, &Rfc3339).unwrap()
    }

    fn event(kind: &str, at: &str, actor: &str, assignee: &str) -> Value {
        json!({"__typename": kind, "createdAt": at,
            "actor": {"login": actor}, "assignee": {"login": assignee}})
    }

    fn comment(author: &str, created: &str, editor: Option<&str>) -> plan::Comment {
        plan::Comment {
            url: format!("https://github.com/o/r/issues/1#issuecomment-{created}"),
            body: "ok".into(),
            author: Some(author.into()),
            edited: editor.is_some(),
            editor: editor.map(str::to_owned),
            created: at(created),
            changed: at(created),
        }
    }

    /// Each case: the events, the assignees (and their total) and the
    /// assignment read, or `None`.
    #[test]
    fn assignment_reads_the_bots_newest() {
        let assigned = |by: &str, when: &str| event("AssignedEvent", when, by, "op");
        let unassigned = |by: &str, when: &str| event("UnassignedEvent", when, by, "op");
        let closed = |by: &str, when: &str| json!({"__typename": "ClosedEvent", "createdAt": when, "actor": {"login": by}});
        let wait = |since: &str, standing, unassigned, closed| {
            Some(Assignment {
                since: at(since),
                standing,
                unassigned,
                closed,
            })
        };
        let t1 = "2026-11-01T00:00:00Z";
        let t2 = "2026-11-02T00:00:00Z";
        let t3 = "2026-11-03T00:00:00Z";
        for (name, events, assignees, total, expected) in [
            ("none", vec![], vec![], 0, None),
            (
                "the operator's own",
                vec![assigned("op", t1)],
                vec!["op"],
                1,
                None,
            ),
            (
                "someone else's",
                vec![event("AssignedEvent", t1, "bot", "other")],
                vec!["other"],
                1,
                None,
            ),
            (
                "standing",
                vec![assigned("bot", t1)],
                vec!["op"],
                1,
                wait(t1, true, false, false),
            ),
            (
                "case",
                vec![assigned("BOT", t1)],
                vec!["OP"],
                1,
                wait(t1, true, false, false),
            ),
            (
                "unassigned by the operator",
                vec![assigned("bot", t1), unassigned("op", t2)],
                vec![],
                0,
                wait(t1, false, true, false),
            ),
            (
                "unassigned by the bot",
                vec![assigned("bot", t1), unassigned("bot", t2)],
                vec![],
                0,
                wait(t1, false, false, false),
            ),
            (
                "taken over by the operator",
                vec![
                    assigned("bot", t1),
                    unassigned("op", t2),
                    assigned("op", t3),
                ],
                vec!["op"],
                1,
                wait(t1, false, true, false),
            ),
            (
                "assigned again",
                vec![
                    assigned("bot", t1),
                    unassigned("bot", t2),
                    assigned("bot", t3),
                ],
                vec!["op"],
                1,
                wait(t3, true, false, false),
            ),
            (
                "out of order",
                vec![
                    assigned("bot", t3),
                    unassigned("op", t2),
                    assigned("bot", t1),
                ],
                vec!["op"],
                1,
                wait(t3, true, false, false),
            ),
            (
                "closed since",
                vec![assigned("bot", t1), closed("op", t2)],
                vec!["op"],
                1,
                wait(t1, true, false, true),
            ),
            (
                "closed before",
                vec![closed("op", t1), assigned("bot", t2)],
                vec!["op"],
                1,
                wait(t2, true, false, false),
            ),
            (
                "closed by the bot",
                vec![assigned("bot", t1), closed("bot", t2)],
                vec!["op"],
                1,
                wait(t1, true, false, false),
            ),
            (
                "closed by its author",
                vec![assigned("bot", t1), closed("visitor", t2)],
                vec!["op"],
                1,
                wait(t1, true, false, false),
            ),
            (
                "assignees unread",
                vec![assigned("bot", t1)],
                vec!["a"],
                21,
                wait(t1, true, false, false),
            ),
            (
                "not assigned",
                vec![assigned("bot", t1)],
                vec![],
                0,
                wait(t1, false, false, false),
            ),
        ] {
            let nodes: Vec<_> = assignees.iter().map(|l| json!({"login": l})).collect();
            let node = json!({"assignees": {"nodes": nodes, "totalCount": total},
                "assignments": {"pageInfo": {"hasPreviousPage": false}, "nodes": events}});
            assert_eq!(assignment(&node, "op", "bot").unwrap(), expected, "{name}");
        }
        // Older events unread: an assigned operator whose bot assignment may
        // be among them cannot be told from one who is not waiting.
        let truncated = |events: Vec<Value>, assignees: Vec<&str>| {
            let nodes: Vec<_> = assignees.iter().map(|l| json!({"login": l})).collect();
            let node = json!({"assignees": {"nodes": nodes, "totalCount": assignees.len()},
                "assignments": {"pageInfo": {"hasPreviousPage": true}, "nodes": events}});
            assignment(&node, "op", "bot")
        };
        assert!(truncated(vec![closed("visitor", t2); 100], vec!["op"]).is_err());
        assert_eq!(
            truncated(vec![closed("visitor", t2)], vec![]).unwrap(),
            None
        );
        assert_eq!(
            truncated(vec![assigned("bot", t1), closed("visitor", t2)], vec!["op"]).unwrap(),
            wait(t1, true, false, false)
        );
        let odd = json!({"assignees": {"nodes": [], "totalCount": 0},
            "assignments": {"nodes": [event("LabeledEvent", t1, "bot", "op")]}});
        assert!(assignment(&odd, "op", "bot").is_err());
        assert!(assignment(&json!({}), "op", "bot").is_err());
    }

    /// Each case: the comments, whether someone unassigned the operator or
    /// closed the issue since, and what answers: a comment's index, or the
    /// unassignment or close.
    #[test]
    fn any_operator_response_releases() {
        let since = "2026-11-02T00:00:00Z";
        let before = "2026-11-01T00:00:00Z";
        let after = "2026-11-03T00:00:00Z";
        let later = "2026-11-04T00:00:00Z";
        for (name, comments, (unassigned, closed), expected) in [
            ("nothing", vec![], (false, false), None),
            (
                "before",
                vec![comment("op", before, None)],
                (false, false),
                None,
            ),
            (
                "at the assignment",
                vec![comment("op", since, None)],
                (false, false),
                None,
            ),
            (
                "answered",
                vec![comment("op", after, None)],
                (false, false),
                Some("0"),
            ),
            (
                "the newest",
                vec![comment("op", after, None), comment("OP", later, None)],
                (false, false),
                Some("1"),
            ),
            (
                "the bot's",
                vec![comment("bot", after, None)],
                (false, false),
                None,
            ),
            (
                "a visitor's",
                vec![comment("visitor", after, None)],
                (false, false),
                None,
            ),
            (
                "edited by a visitor",
                vec![comment("op", after, Some("visitor"))],
                (false, false),
                None,
            ),
            (
                "edited by the bot",
                vec![comment("op", after, Some("bot"))],
                (false, false),
                Some("0"),
            ),
            ("unassigned", vec![], (true, false), Some("unassigned")),
            ("closed", vec![], (false, true), Some("closed")),
            (
                "unassigned and closed",
                vec![],
                (true, true),
                Some("unassigned"),
            ),
            (
                "all",
                vec![comment("op", after, None)],
                (true, true),
                Some("0"),
            ),
        ] {
            let assigned = Assignment {
                since: at(since),
                standing: !unassigned,
                unassigned,
                closed,
            };
            let got = match response(&assigned, &comments, ("op", &["op", "bot"])) {
                Some(Response::Comment(c)) => {
                    let n = comments.iter().position(|o| std::ptr::eq(o, c)).unwrap();
                    Some(n.to_string())
                }
                Some(Response::Unassigned) => Some("unassigned".into()),
                Some(Response::Closed) => Some("closed".into()),
                None => None,
            };
            assert_eq!(got.as_deref(), expected, "{name}");
        }
    }
}
