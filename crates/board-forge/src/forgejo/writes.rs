//! Direct, bounded deterministic writes. No scheduler or agent-output entry point.
use super::*;
use crate::epic_apply::APPLY_MARKER;
use crate::plan::DISPATCH_MARKER;
use crate::{BoardOperation, BoardWrite, PlanClaim};
use serde_json::json;
use time::format_description::well_known::Rfc3339;

const WRITE_READ_LIMIT: usize = 32;
const COMMENT_LIMIT: usize = 16_000;
/// The first line of a claim's receipt: `<!-- agent-board-receipt: CLAIM-ID -->`.
pub const RECEIPT_MARKER: &str = "agent-board-receipt";
/// What a claim may claim: a job plan's dispatch or an epic plan's apply.
const CLAIM_MARKERS: [&str; 2] = [DISPATCH_MARKER, APPLY_MARKER];

fn marker_line(marker: &str, value: impl std::fmt::Display) -> String {
    format!("<!-- {marker}: {value} -->")
}

fn first_line(body: &str) -> &str {
    body.lines().next().unwrap_or_default().trim_end()
}

/// Whether `body` starts with a claim or receipt line, which only their
/// own operations write.
fn reserved(body: &str) -> bool {
    let head = first_line(body);
    CLAIM_MARKERS
        .iter()
        .chain([&RECEIPT_MARKER])
        .any(|marker| head.starts_with(&format!("<!-- {marker}:")))
}

fn checked_body(body: &str) -> Result<()> {
    ensure!(
        !body.trim().is_empty() && body.chars().count() <= COMMENT_LIMIT,
        "invalid comment size"
    );
    Ok(())
}

/// Whether the recorder `user` already has an unedited comment saying `body`.
fn posted(comments: &[Value], user: u64, body: &str) -> bool {
    comments
        .iter()
        .any(|c| c["body"].as_str() == Some(body) && c["user"]["id"].as_u64() == Some(user))
}

/// The plan comment's identity (`ID@CREATED`) and its earliest recorder
/// claim with that claim's receipts, from a whole thread whose recorder
/// comments are all unedited ([`Reads::thread`]).
fn find_claim(
    comments: &[Value],
    recorders: &[u64],
    marker: &str,
    plan: u64,
) -> Result<(String, Option<PlanClaim>)> {
    ensure!(CLAIM_MARKERS.contains(&marker), "unsupported claim marker");
    let author = |c: &Value| c["user"]["id"].as_u64().filter(|id| recorders.contains(id));
    let source = comments
        .iter()
        .find(|c| c["id"].as_u64() == Some(plan))
        .context("plan comment missing; hold")?;
    ensure!(
        author(source).is_some() && !reserved(text(source, "body")?),
        "plan comment not a recorder's plan; hold"
    );
    // In UTC, so a change of the server's time zone cannot unclaim a plan.
    let created = time::OffsetDateTime::parse(text(source, "created_at")?, &Rfc3339)
        .context("plan comment has no valid time; hold")?;
    let id = format!(
        "{plan}@{}",
        created.to_offset(time::UtcOffset::UTC).format(&Rfc3339)?
    );
    let line = marker_line(marker, &id);
    let earliest = comments
        .iter()
        .filter(|c| first_line(c["body"].as_str().unwrap_or_default()) == line)
        .filter_map(|c| Some((c["id"].as_u64()?, author(c)?)))
        .min();
    let Some((claim, recorder)) = earliest else {
        return Ok((id, None));
    };
    let receipt = marker_line(RECEIPT_MARKER, claim);
    let mut receipts: Vec<(u64, String)> = comments
        .iter()
        .filter(|c| author(c) == Some(recorder) && c["id"].as_u64() > Some(claim))
        .filter_map(|c| {
            let (head, rest) = c["body"].as_str()?.split_once('\n')?;
            (head.trim_end() == receipt)
                .then(|| (c["id"].as_u64().unwrap_or_default(), rest.into()))
        })
        .collect();
    receipts.sort();
    let claim = PlanClaim {
        plan: id.clone(),
        claim,
        recorder,
        receipts: receipts.into_iter().map(|(_, text)| text).collect(),
    };
    Ok((id, Some(claim)))
}

/// One operation's reads of one configured issue, at most [`WRITE_READ_LIMIT`].
struct Reads<'a, T> {
    forge: &'a Forgejo<T>,
    budget: Cell<usize>,
    api: Url,
    target: &'a ItemRef,
    issue_url: Url,
}

impl<T: Transport> Reads<'_, T> {
    fn get(&self, url: &Url) -> Result<Value> {
        ensure!(self.budget.get() > 0, "Forgejo write read budget exhausted");
        self.budget.set(self.budget.get() - 1);
        Ok(self.forge.transport.get(url)?.body)
    }

    /// The target issue, still the same issue and a board member, and its
    /// labels. Open membership is checked where a write needs it.
    fn issue(&self) -> Result<(Value, BTreeSet<String>)> {
        let issue = self.get(&self.issue_url)?;
        ensure!(
            issue["number"].as_u64() == Some(self.target.number)
                && issue["pull_request"].is_null()
                && text(&issue["repository"], "full_name")? == self.target.repository,
            "Forgejo target changed repository, number or kind"
        );
        let labels = issue["labels"]
            .as_array()
            .context("missing issue labels")?
            .iter()
            .map(|label| Ok(text(label, "name")?.to_owned()))
            .collect::<Result<BTreeSet<_>>>()?;
        if let Membership::Label(member) = &self.forge.config.membership {
            ensure!(
                labels.contains(member),
                "target lacks board membership label"
            );
        }
        Ok((issue, labels))
    }

    fn comments_url(&self) -> Result<Url> {
        Ok(self.api.join(&format!(
            "repos/{}/issues/{}/comments",
            self.target.repository, self.target.number
        ))?)
    }

    /// The whole thread of `issue`. Every recorder comment in it must be
    /// unedited, so nothing, a replay included, succeeds past an edited record.
    fn thread(&self, issue: &Value) -> Result<Vec<Value>> {
        // This endpoint returns the whole thread, not offset pages.
        let comments = self.get(&self.comments_url()?)?;
        let comments = comments.as_array().context("comments not an array")?;
        ensure!(
            issue["comments"].as_u64() == Some(comments.len() as u64),
            "incomplete comment read; hold"
        );
        let mut seen = BTreeSet::new();
        for comment in comments {
            let id = comment["id"].as_u64().context("missing comment id")?;
            ensure!(seen.insert(id), "comment {id} listed twice; hold");
            if comment["user"]["id"]
                .as_u64()
                .is_some_and(|id| self.forge.config.recorders.contains(&id))
            {
                ensure!(unedited(comment), "edited recorder record; hold");
            }
        }
        Ok(comments.clone())
    }

    /// The comment `id` as written, by `user` and unedited.
    fn read_back(&self, answer: &Value, user: u64, body: &str) -> Result<u64> {
        let id = answer["id"]
            .as_u64()
            .filter(|id| *id > 0)
            .context("comment write ambiguous; hold")?;
        let after = self.get(&self.api.join(&format!(
            "repos/{}/issues/comments/{id}",
            self.target.repository
        ))?)?;
        ensure!(
            after["id"].as_u64() == Some(id)
                && after["body"].as_str() == Some(body)
                && after["user"]["id"].as_u64() == Some(user)
                && unedited(&after),
            "comment read-back mismatch; hold"
        );
        Ok(id)
    }
}

impl<T: Transport> Forgejo<T> {
    /// A complete, unedited recorder thread, using the claim protocol's reads.
    pub(crate) fn plan_thread(&self, target: &ItemRef) -> Result<Vec<Value>> {
        let reads = self.reads(target)?;
        let (issue, _) = reads.issue()?;
        reads.thread(&issue)
    }

    fn reads<'a>(&'a self, target: &'a ItemRef) -> Result<Reads<'a, T>> {
        self.config.validate()?;
        target.validate()?;
        ensure!(
            target.host == self.config.url.host_str().unwrap_or_default()
                && self.config.repositories.contains(&target.repository),
            "Forgejo request outside configured host/repository"
        );
        let api = self.config.api_root()?;
        let issue_url = api.join(&format!(
            "repos/{}/issues/{}",
            target.repository, target.number
        ))?;
        Ok(Reads {
            forge: self,
            budget: Cell::new(WRITE_READ_LIMIT),
            api,
            target,
            issue_url,
        })
    }

    pub(super) fn read_plan_claim(
        &self,
        target: &ItemRef,
        marker: &str,
        plan: u64,
    ) -> Result<Option<PlanClaim>> {
        let reads = self.reads(target)?;
        let (issue, _) = reads.issue()?;
        let comments = reads.thread(&issue)?;
        Ok(find_claim(&comments, &self.config.recorders, marker, plan)?.1)
    }

    pub(super) fn write_board(
        &self,
        write: &BoardWrite,
        apply: bool,
        remaining: &mut usize,
    ) -> Result<()> {
        let observed = &write.observed;
        ensure!(
            observed.content_kind == "issue",
            "Forgejo write outside configured host/repository/kind"
        );
        let reads = self.reads(&observed.identity)?;
        let target = reads.target;
        let api = &reads.api;
        // A copied login cannot authorize writes; only the authenticated id can.
        let user = reads.get(&api.join("user")?)?["id"]
            .as_u64()
            .filter(|id| *id > 0 && self.config.recorders.contains(id))
            .context("Forgejo authenticated user is not a configured recorder")?;
        let (current, before) = reads.issue()?;
        let recorders = &self.config.recorders;
        // The reads each operation's verification needs after its write.
        let mut verify = 1;
        let (method, url, body, latest) = match &write.operation {
            BoardOperation::Label { scope, name } => {
                ensure!(
                    [PRIORITY_SCOPE, TURN_SCOPE, STATUS_SCOPE].contains(&scope.as_str())
                        && name.starts_with(scope)
                        && name.len() > scope.len(),
                    "unsupported label scope"
                );
                if scope == STATUS_SCOPE {
                    ensure!(
                        self.config.status_labels && parse_status(&name[scope.len()..]).is_some(),
                        "Status labels disabled or unsupported Status"
                    );
                } else if scope == PRIORITY_SCOPE {
                    ensure!(
                        serde_json::from_value::<Priority>(name[scope.len()..].into()).is_ok(),
                        "unsupported Priority"
                    );
                } else {
                    let value = &name[scope.len()..];
                    ensure!(
                        ["coordinator", "worker", "operator", "external", "none"].contains(&value)
                            || write.policy.agents.contains_key(value),
                        "unsupported Turn"
                    );
                }
                let in_scope: Vec<_> = before
                    .iter()
                    .filter(|label| label.starts_with(scope))
                    .collect();
                if in_scope.len() == 1 && in_scope[0] == name {
                    return Ok(());
                }
                ensure!(
                    before == observed.labels.iter().cloned().collect(),
                    "stale issue labels; hold"
                );
                // Repository and organization labels are both needed: names in
                // the issue API are silently dropped if resolution is missing.
                let mut available = Vec::new();
                let mut endpoints = vec![api.join(&format!("repos/{}/labels", target.repository))?];
                let owner = target
                    .repository
                    .split('/')
                    .next()
                    .context("missing owner")?;
                ensure!(
                    reads.budget.get() > 0,
                    "Forgejo write read budget exhausted"
                );
                reads.budget.set(reads.budget.get() - 1);
                // Repository.owner is an API User, with no `type` field.
                // Only a real 404 means there is no organization. Auth and
                // rate-limit errors must not masquerade as a personal owner.
                match self.transport.get(&api.join(&format!("orgs/{owner}"))?) {
                    Ok(_) => endpoints.push(api.join(&format!("orgs/{owner}/labels"))?),
                    Err(error) if error.is::<NotFound>() => {}
                    Err(error) => return Err(error),
                }
                for endpoint in endpoints {
                    let (page, complete) = self.pages(&reads.budget, endpoint)?;
                    ensure!(complete, "incomplete label listing; hold");
                    available.extend(page);
                }
                let candidates: Vec<_> = available
                    .iter()
                    .filter(|label| label["name"].as_str() == Some(name))
                    .collect();
                ensure!(
                    candidates.len() == 1 && candidates[0]["exclusive"] == true,
                    "label missing, ambiguous or not exclusive"
                );
                let id = candidates[0]["id"]
                    .as_u64()
                    .filter(|id| *id > 0)
                    .context("invalid label id")?;
                // Re-read immediately before writing; no If-Match exists.
                // This read's membership is the one the write relies on.
                let (fresh, labels) = reads.issue()?;
                ensure!(labels == before, "labels changed before write; hold");
                (
                    "POST",
                    api.join(&format!(
                        "repos/{}/issues/{}/labels",
                        target.repository, target.number
                    ))?,
                    json!({"labels": [id]}),
                    fresh,
                )
            }
            BoardOperation::State { state } => {
                ensure!(
                    matches!(state.as_str(), "open" | "closed"),
                    "unsupported issue state"
                );
                if current["state"].as_str() == Some(state) {
                    return Ok(());
                }
                ensure!(
                    current["state"].as_str() == Some(&observed.state),
                    "stale issue state; hold"
                );
                (
                    "PATCH",
                    reads.issue_url.clone(),
                    json!({"state": state}),
                    current,
                )
            }
            BoardOperation::Comment { body } => {
                checked_body(body)?;
                ensure!(
                    !reserved(body),
                    "claim and receipt records need their own operations"
                );
                let comments = reads.thread(&current)?;
                if posted(&comments, user, body) {
                    return Ok(());
                }
                (
                    "POST",
                    reads.comments_url()?,
                    json!({"body": body}),
                    current,
                )
            }
            BoardOperation::Receipt { marker, plan, body } => {
                let comments = reads.thread(&current)?;
                let (id, claim) = find_claim(&comments, recorders, marker, *plan)?;
                let claim = claim.with_context(|| format!("plan {id} has no claim; hold"))?;
                ensure!(
                    claim.recorder == user,
                    "plan {id} claimed by another recorder; hold"
                );
                ensure!(!body.trim().is_empty(), "invalid comment size");
                let body = format!("{}\n{body}", marker_line(RECEIPT_MARKER, claim.claim));
                checked_body(&body)?;
                if posted(&comments, user, &body) {
                    return Ok(());
                }
                (
                    "POST",
                    reads.comments_url()?,
                    json!({"body": body}),
                    current,
                )
            }
            BoardOperation::Claim { marker, plan } => {
                let comments = reads.thread(&current)?;
                let (id, claim) = find_claim(&comments, recorders, marker, *plan)?;
                // A repeated or ambiguous claim is never made again: the
                // earlier one may have started its run. Recovery reads it.
                if let Some(claim) = claim {
                    bail!(
                        "plan {id} already claimed by comment {}; recover from its receipts, never claim it again",
                        claim.claim
                    );
                }
                verify = 3;
                let body = format!(
                    "{}\nClaimed this plan; its outcome follows in a later comment.",
                    marker_line(marker, &id)
                );
                (
                    "POST",
                    reads.comments_url()?,
                    json!({"body": body}),
                    current,
                )
            }
        };
        ensure!(
            self.config.membership != Membership::Open || latest["state"].as_str() == Some("open"),
            "target outside open board membership; hold"
        );
        ensure!(
            reads.budget.get() >= verify,
            "no read-back budget remaining; hold"
        );
        if !apply {
            return Ok(());
        }
        ensure!(*remaining > 0, "Forgejo write cap exhausted");
        // A failed or ambiguous request consumes the cap as well.
        *remaining -= 1;
        let answer = self.transport.write(method, &url, &body)?;
        let posted_body = body["body"].as_str().unwrap_or_default();
        match &write.operation {
            BoardOperation::Label { scope, name } => {
                let (_, after) = reads.issue()?;
                let mut expected = before;
                expected.retain(|label| !label.starts_with(scope));
                expected.insert(name.clone());
                ensure!(after == expected, "label read-back mismatch; hold");
            }
            BoardOperation::State { state } => {
                let (after, _) = reads.issue()?;
                ensure!(
                    after["state"].as_str() == Some(state),
                    "state read-back mismatch; hold"
                );
            }
            BoardOperation::Comment { .. } | BoardOperation::Receipt { .. } => {
                reads.read_back(&answer, user, posted_body)?;
            }
            BoardOperation::Claim { marker, plan } => {
                let id = reads.read_back(&answer, user, posted_body)?;
                let (issue, _) = reads.issue()?;
                let comments = reads.thread(&issue)?;
                let (_, claim) = find_claim(&comments, recorders, marker, *plan)?;
                ensure!(
                    claim.is_some_and(|c| c.claim == id && c.recorder == user),
                    "an earlier claim of the plan won; hold, do not act on it"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Forge;
    use std::cell::RefCell;

    /// The plan comment the claim tests seed, by the bot recorder.
    const PLAN: u64 = 400;

    struct Replay {
        issue: RefCell<Value>,
        comments: RefCell<Vec<Value>>,
        writes: Cell<usize>,
        issue_reads: Cell<usize>,
        fault: &'static str,
    }

    impl Replay {
        fn label(&self) -> &str {
            if self.fault == "custom-turn" {
                "turn/custom"
            } else {
                "status/in-progress"
            }
        }

        fn add_comment(&self, id: u64, user: u64, body: &str) {
            self.comments
                .borrow_mut()
                .push(json!({"id": id, "body": body, "user": {"id": user},
                "created_at": "2026-10-02T08:00:00Z", "updated_at": "2026-10-02T08:00:00Z"}));
            self.issue.borrow_mut()["comments"] = self.comments.borrow().len().into();
        }
    }

    impl Transport for Replay {
        fn get(&self, url: &Url) -> Result<Response> {
            ensure!(url.host_str() == Some("forge.example"), "foreign host");
            if matches!(self.fault, "auth" | "rate-limit") {
                bail!("{}", self.fault);
            }
            let path = url.path();
            let (body, total) = if path.ends_with("/user") {
                (
                    json!({"id": if self.fault == "foreign-user" { 11 } else { 9 }}),
                    None,
                )
            } else if path.ends_with("/issues/1") {
                // The final pre-write read sees a change made after the first.
                self.issue_reads.set(self.issue_reads.get() + 1);
                if self.issue_reads.get() == 2 {
                    let mut issue = self.issue.borrow_mut();
                    match self.fault {
                        "fresh-membership" => issue["labels"]
                            .as_array_mut()
                            .unwrap()
                            .retain(|label| label["name"] != "agent-board"),
                        "fresh-closed" => issue["state"] = "closed".into(),
                        _ => {}
                    }
                }
                (self.issue.borrow().clone(), None)
            } else if path.ends_with("/issues/1/comments") {
                (json!(*self.comments.borrow()), None)
            } else if let Some(id) =
                path.strip_prefix("/api/v1/repos/sample/board/issues/comments/")
            {
                let id: u64 = id.parse()?;
                let comments = self.comments.borrow();
                let found = comments.iter().find(|c| c["id"] == id);
                (found.context("no such comment")?.clone(), None)
            } else if path.ends_with("/labels") {
                if self.fault == "organization" && path.contains("/repos/") {
                    return Ok(Response {
                        body: json!([]),
                        total: Some(0),
                    });
                }
                if matches!(self.fault, "budget-exact" | "budget-fit") {
                    let total = if self.fault == "budget-exact" { 28 } else { 27 };
                    let page: u64 = url
                        .query_pairs()
                        .find(|(key, _)| key == "page")
                        .unwrap()
                        .1
                        .parse()
                        .unwrap();
                    return Ok(Response {
                        body: if page == total {
                            json!([{"id": 7, "name": "status/in-progress", "exclusive": true}])
                        } else {
                            json!([{"id": page + 100, "name": format!("dummy/{page}")}])
                        },
                        total: Some(total),
                    });
                }
                (
                    json!([{"id": 7, "name": self.label(), "exclusive": self.fault != "nonexclusive"}]),
                    if self.fault == "paging" {
                        None
                    } else {
                        Some(1)
                    },
                )
            } else if path.ends_with("/orgs/sample") {
                if self.fault == "organization" {
                    (json!({"id": 12, "name": "sample"}), None)
                } else {
                    return Err(NotFound.into());
                }
            } else {
                bail!("unexpected GET {url}");
            };
            Ok(Response { body, total })
        }

        fn write(&self, method: &str, url: &Url, body: &Value) -> Result<Value> {
            self.writes.set(self.writes.get() + 1);
            if self.fault == "partial" {
                bail!("ambiguous write; hold");
            }
            if url.path().ends_with("/labels") {
                assert_eq!(method, "POST");
                assert_eq!(body, &json!({"labels": [7]}));
                let mut issue = self.issue.borrow_mut();
                issue["labels"].as_array_mut().unwrap().retain(|label| {
                    !label["name"]
                        .as_str()
                        .unwrap()
                        .starts_with(if self.fault == "custom-turn" {
                            TURN_SCOPE
                        } else {
                            STATUS_SCOPE
                        })
                });
                issue["labels"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"name": self.label(), "id": 7}));
            } else if url.path().ends_with("/comments") {
                assert_eq!(method, "POST");
                let body = body["body"].as_str().unwrap();
                // Another pass's claim of the same plan lands first.
                if self.fault == "race" {
                    self.add_comment(450, 10, body);
                }
                let id = 501 + self.comments.borrow().len() as u64;
                self.add_comment(id, 9, body);
                if self.fault == "lost-response" {
                    bail!("lost response; hold");
                }
                return Ok(self.comments.borrow().last().unwrap().clone());
            } else {
                assert_eq!(method, "PATCH");
                self.issue.borrow_mut()["state"] = body["state"].clone();
            }
            if self.fault == "lost-response" {
                bail!("lost response; hold");
            }
            Ok(Value::Null)
        }
    }

    fn setup(fault: &'static str) -> (Forgejo<Replay>, Item) {
        let mut config = super::super::tests::config(json!({"statusLabels": true}));
        let mut issue = super::super::tests::issues()[0].clone();
        issue["comments"] = 0.into();
        issue["labels"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name": "status/todo"}));
        let observed = item(
            "forge.example",
            "sample/board",
            &issue,
            None,
            &config,
            &board_core::AgentPolicy::parse(board_core::DEFAULT_AGENT_POLICY).unwrap(),
            "2026-10-20T00:00:00Z",
        )
        .unwrap();
        if fault == "wrong-kind" {
            issue["pull_request"] = json!({});
        }
        if fault == "stale" {
            issue["labels"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name": "human"}));
        }
        if fault == "fresh-closed" {
            config.membership = Membership::Open;
        }
        (
            Forgejo {
                config,
                transport: Replay {
                    issue: RefCell::new(issue),
                    comments: RefCell::new(vec![]),
                    writes: Cell::new(0),
                    issue_reads: Cell::new(0),
                    fault,
                },
            },
            observed,
        )
    }

    fn policy() -> board_core::AgentPolicy {
        board_core::AgentPolicy::parse(board_core::DEFAULT_AGENT_POLICY).unwrap()
    }

    fn claim(marker: &str) -> BoardOperation {
        BoardOperation::Claim {
            marker: marker.into(),
            plan: PLAN,
        }
    }

    fn receipt(marker: &str) -> BoardOperation {
        BoardOperation::Receipt {
            marker: marker.into(),
            plan: PLAN,
            body: "Dispatched: https://forge.example/sample/board/actions/runs/3".into(),
        }
    }

    #[test]
    fn operation_replay_table() {
        for operation in [
            BoardOperation::Label {
                scope: STATUS_SCOPE.into(),
                name: "status/in-progress".into(),
            },
            BoardOperation::State {
                state: "closed".into(),
            },
            BoardOperation::Comment {
                body: "deterministic record".into(),
            },
        ] {
            let (forge, observed) = setup("");
            let write = BoardWrite {
                policy: policy(),
                observed,
                operation,
            };
            let mut remaining = 1;
            forge.board_write(&write, false, &mut remaining).unwrap();
            assert_eq!(forge.transport.writes.get(), 0);
            forge.board_write(&write, true, &mut remaining).unwrap();
            forge.board_write(&write, true, &mut remaining).unwrap();
            assert_eq!(forge.transport.writes.get(), 1);
            assert_eq!(remaining, 0);
            let observed_after = item(
                "forge.example",
                "sample/board",
                &forge.transport.issue.borrow(),
                None,
                &forge.config,
                &board_core::AgentPolicy::parse(board_core::DEFAULT_AGENT_POLICY).unwrap(),
                "2026-10-20T00:00:00Z",
            )
            .unwrap();
            if matches!(write.operation, BoardOperation::Label { .. }) {
                assert_eq!(observed_after.status, Some(Status::InProgress));
                assert_eq!(observed_after.priority, write.observed.priority);
                assert_eq!(observed_after.turn, write.observed.turn);
            }
        }
    }

    #[test]
    fn refusal_table() {
        for fault in [
            "foreign-user",
            "wrong-kind",
            "stale",
            "paging",
            "nonexclusive",
            "auth",
            "rate-limit",
            "partial",
            "cap",
            "host",
            "repo",
            "fresh-membership",
            "fresh-closed",
        ] {
            let (forge, mut observed) = setup(fault);
            if fault == "host" {
                observed.identity.host = "foreign.example".into();
            }
            if fault == "repo" {
                observed.identity.repository = "foreign/board".into();
            }
            let write = BoardWrite {
                policy: policy(),
                observed,
                operation: BoardOperation::Label {
                    scope: STATUS_SCOPE.into(),
                    name: "status/in-progress".into(),
                },
            };
            let mut remaining = usize::from(fault != "cap");
            assert!(
                forge.board_write(&write, true, &mut remaining).is_err(),
                "{fault}"
            );
            assert_eq!(
                forge.transport.writes.get(),
                usize::from(fault == "partial"),
                "{fault}"
            );
            if fault == "partial" {
                assert_eq!(remaining, 0);
            }
        }
    }

    #[test]
    fn ambiguous_committed_write_requires_read_back_not_another_write() {
        for operation in [
            BoardOperation::Label {
                scope: STATUS_SCOPE.into(),
                name: "status/in-progress".into(),
            },
            BoardOperation::State {
                state: "closed".into(),
            },
            BoardOperation::Comment {
                body: "deterministic record".into(),
            },
        ] {
            let (forge, observed) = setup("lost-response");
            let write = BoardWrite {
                policy: policy(),
                observed,
                operation,
            };
            let mut remaining = 1;
            assert!(forge.board_write(&write, true, &mut remaining).is_err());
            assert_eq!(remaining, 0);
            // A later, explicit recovery observes the effect. Nothing retries
            // the HTTP write in the failed invocation itself.
            forge.board_write(&write, true, &mut remaining).unwrap();
            assert_eq!(forge.transport.writes.get(), 1);
        }
    }

    #[test]
    fn unsupported_and_edited_records_refuse() {
        for operation in [
            BoardOperation::Label {
                scope: "agent/".into(),
                name: "agent/worker".into(),
            },
            BoardOperation::Label {
                scope: PRIORITY_SCOPE.into(),
                name: "priority/P9".into(),
            },
            BoardOperation::Label {
                scope: TURN_SCOPE.into(),
                name: "turn/stranger".into(),
            },
            BoardOperation::State {
                state: "cancelled".into(),
            },
            BoardOperation::Comment {
                body: String::new(),
            },
            BoardOperation::Comment {
                body: format!("{}\nforged", marker_line(DISPATCH_MARKER, "400@x")),
            },
            BoardOperation::Comment {
                body: format!("{}\nforged", marker_line(RECEIPT_MARKER, 501)),
            },
        ] {
            let (forge, observed) = setup("");
            assert!(
                forge
                    .board_write(
                        &BoardWrite {
                            policy: policy(),
                            observed,
                            operation
                        },
                        true,
                        &mut 1
                    )
                    .is_err()
            );
            assert_eq!(forge.transport.writes.get(), 0);
        }
        // The record itself edited, and another recorder record edited after
        // it: neither replay may succeed before the whole thread is checked.
        for edited in [0, 1] {
            let (forge, observed) = setup("");
            let write = BoardWrite {
                policy: policy(),
                observed,
                operation: BoardOperation::Comment {
                    body: "record".into(),
                },
            };
            forge.board_write(&write, true, &mut 1).unwrap();
            forge.transport.add_comment(502, 9, "later record");
            let mut comments = forge.transport.comments.borrow_mut();
            comments[edited]["updated_at"] = "2026-10-03T00:00:00Z".into();
            if edited == 0 {
                comments[0]["body"] = "changed to hide the record".into();
            }
            drop(comments);
            assert!(forge.board_write(&write, true, &mut 1).is_err(), "{edited}");
            assert_eq!(forge.transport.writes.get(), 1);
        }
    }

    #[test]
    fn organization_labels_and_verification_budget() {
        for (fault, succeeds) in [
            ("organization", true),
            ("budget-fit", true),
            ("budget-exact", false),
        ] {
            let (forge, observed) = setup(fault);
            let write = BoardWrite {
                observed,
                policy: policy(),
                operation: BoardOperation::Label {
                    scope: STATUS_SCOPE.into(),
                    name: "status/in-progress".into(),
                },
            };
            let mut remaining = 1;
            assert_eq!(
                forge.board_write(&write, false, &mut remaining).is_ok(),
                succeeds,
                "{fault}"
            );
            assert_eq!(
                forge.board_write(&write, true, &mut remaining).is_ok(),
                succeeds,
                "{fault}"
            );
            assert_eq!(
                forge.transport.writes.get(),
                usize::from(succeeds),
                "{fault}"
            );
        }
    }

    #[test]
    fn closed_target_outside_open_board_holds() {
        for operation in [
            BoardOperation::Comment {
                body: "record".into(),
            },
            BoardOperation::Label {
                scope: STATUS_SCOPE.into(),
                name: "status/in-progress".into(),
            },
        ] {
            let (mut forge, observed) = setup("");
            forge.config.membership = Membership::Open;
            forge.transport.issue.borrow_mut()["state"] = "closed".into();
            let write = BoardWrite {
                observed,
                policy: policy(),
                operation,
            };
            assert!(forge.board_write(&write, true, &mut 1).is_err());
            assert_eq!(forge.transport.writes.get(), 0);
        }
    }

    #[test]
    fn custom_turn_uses_active_policy_for_read_and_write() {
        let (forge, observed) = setup("custom-turn");
        let active =
            board_core::AgentPolicy::parse("[agents.custom]\nenabled=true\nfields=[]\noutputs=[]")
                .unwrap();
        let write = BoardWrite {
            observed,
            policy: active,
            operation: BoardOperation::Label {
                scope: TURN_SCOPE.into(),
                name: "turn/custom".into(),
            },
        };
        forge.board_write(&write, true, &mut 1).unwrap();
        let after = item(
            "forge.example",
            "sample/board",
            &forge.transport.issue.borrow(),
            None,
            &forge.config,
            &write.policy,
            "2026-10-20T00:00:00Z",
        )
        .unwrap();
        assert_eq!(after.turn, Some(Turn::Worker));
        assert!(after.fields_complete);
    }

    #[test]
    fn claim_then_receipts_replay() {
        for marker in CLAIM_MARKERS {
            let (forge, observed) = setup("");
            forge
                .transport
                .add_comment(PLAN, 9, "```agent-board-plan\n{}\n```");
            let target = observed.identity.clone();
            let run = |operation, apply, remaining: &mut usize| {
                let write = BoardWrite {
                    policy: policy(),
                    observed: observed.clone(),
                    operation,
                };
                forge.board_write(&write, apply, remaining)
            };
            let mut remaining = 2;
            assert_eq!(forge.plan_claim(&target, marker, PLAN).unwrap(), None);
            // A receipt needs a claim, and a preview claims nothing.
            assert!(run(receipt(marker), true, &mut remaining).is_err());
            run(claim(marker), false, &mut remaining).unwrap();
            assert_eq!(forge.transport.writes.get(), 0);
            run(claim(marker), true, &mut remaining).unwrap();
            // The claim is the plan's for good: claiming again holds.
            assert!(run(claim(marker), true, &mut remaining).is_err());
            let found = forge.plan_claim(&target, marker, PLAN).unwrap().unwrap();
            assert_eq!(found.plan, format!("{PLAN}@2026-10-02T08:00:00Z"));
            assert_eq!((found.claim, found.recorder), (502, 9));
            assert!(found.receipts.is_empty());
            run(receipt(marker), true, &mut remaining).unwrap();
            run(receipt(marker), true, &mut remaining).unwrap();
            assert_eq!(forge.transport.writes.get(), 2);
            assert_eq!(remaining, 0);
            let found = forge.plan_claim(&target, marker, PLAN).unwrap().unwrap();
            assert_eq!(
                found.receipts,
                ["Dispatched: https://forge.example/sample/board/actions/runs/3"]
            );
        }
    }

    #[test]
    fn claim_refusal_table() {
        let line = marker_line(DISPATCH_MARKER, format!("{PLAN}@2026-10-02T08:00:00Z"));
        for (case, comments, operation, writes) in [
            ("no plan", vec![], claim(DISPATCH_MARKER), 0),
            (
                "plan by a stranger",
                vec![(PLAN, 11, "plan".to_owned())],
                claim(DISPATCH_MARKER),
                0,
            ),
            (
                "unsupported marker",
                vec![(PLAN, 9, "plan".to_owned())],
                claim(RECEIPT_MARKER),
                0,
            ),
            (
                "claimed already",
                vec![(PLAN, 9, "plan".to_owned()), (450, 9, line.clone())],
                claim(DISPATCH_MARKER),
                0,
            ),
            (
                "claimed by another recorder",
                vec![(PLAN, 9, "plan".to_owned()), (450, 10, line.clone())],
                receipt(DISPATCH_MARKER),
                0,
            ),
            (
                "claim lost a race",
                vec![(PLAN, 9, "plan".to_owned())],
                claim(DISPATCH_MARKER),
                1,
            ),
            (
                "plan is a claim",
                vec![(PLAN, 9, marker_line(APPLY_MARKER, "1@x"))],
                claim(DISPATCH_MARKER),
                0,
            ),
            (
                "claimed under another time zone",
                vec![(PLAN, 9, "plan".to_owned()), (450, 9, line.clone())],
                claim(DISPATCH_MARKER),
                0,
            ),
        ] {
            let (mut forge, observed) = setup(if case == "claim lost a race" {
                "race"
            } else {
                ""
            });
            forge.config.recorders = vec![9, 10];
            for (id, user, body) in comments {
                forge.transport.add_comment(id, user, &body);
            }
            if case == "claimed under another time zone" {
                let plan = &mut forge.transport.comments.borrow_mut()[0];
                plan["created_at"] = "2026-10-02T10:00:00+02:00".into();
                plan["updated_at"] = plan["created_at"].clone();
            }
            let write = BoardWrite {
                policy: policy(),
                observed,
                operation,
            };
            assert!(forge.board_write(&write, true, &mut 1).is_err(), "{case}");
            assert_eq!(forge.transport.writes.get(), writes, "{case}");
        }
        // A claim that lost its response is held, not made again, and
        // recovery finds it with no outcome.
        let (forge, observed) = setup("lost-response");
        forge.transport.add_comment(PLAN, 9, "plan");
        let write = BoardWrite {
            policy: policy(),
            observed: observed.clone(),
            operation: claim(DISPATCH_MARKER),
        };
        let mut remaining = 2;
        assert!(forge.board_write(&write, true, &mut remaining).is_err());
        assert!(forge.board_write(&write, true, &mut remaining).is_err());
        assert_eq!((forge.transport.writes.get(), remaining), (1, 1));
        let found = forge
            .plan_claim(&observed.identity, DISPATCH_MARKER, PLAN)
            .unwrap()
            .unwrap();
        assert!(found.receipts.is_empty());
    }
}
