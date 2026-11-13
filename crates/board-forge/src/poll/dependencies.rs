//! A single durable marker, separate from round claims. Interrupted writes keep
//! the PR held; they never authorize a fix or a second branch update.
use super::*;
use serde::{Deserialize, Serialize};

const TAG: &str = "<!-- agent-board-dependencies: ";
const BLOCKED: &str = "blocked";
const SUPERVISOR: &str = "needs-supervisor";
pub(super) const DRAFT: &str = "mutation($id: ID!) { convertPullRequestToDraft(input: {pullRequestId: $id}) { clientMutationId } }";
const READY: &str = "mutation($id: ID!) { markPullRequestReadyForReview(input: {pullRequestId: $id}) { clientMutationId } }";
type PullStep = (String, bool, Option<Wait>, Option<Stepped>);

#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Held {
    /// Empty until the one branch update is reserved. Ambiguous update failures
    /// do not retry: a person may update the draft branch without a fix round.
    update_head: String,
    /// Only a draft created by this hold may be made ready automatically.
    /// Older markers without ownership are deliberately conservative.
    #[serde(default)]
    owns_draft: bool,
    released: bool,
    reason: String,
}

struct Admission {
    previous: Option<(u64, Held)>,
    failed: Vec<String>,
    blockers: Vec<String>,
}

fn marker(comments: &[Value], bot: &str) -> Result<Option<(u64, Held)>> {
    for comment in comments.iter().rev() {
        if text(comment, "/user/login") != bot {
            continue;
        }
        if let Some(state) = text(comment, "/body").strip_prefix(TAG) {
            let line = state.lines().next().unwrap_or_default();
            let state = line
                .strip_suffix(" -->")
                .context("dependency marker suffix")?;
            return Ok(Some((
                comment["id"].as_u64().context("dependency marker id")?,
                serde_json::from_str(state).context("dependency marker state")?,
            )));
        }
    }
    Ok(None)
}

/// A PR is satisfied only by merge, not closure. An abandoned issue is not a
/// completed prerequisite either. Missing facts cannot authorize readiness.
fn satisfied(value: &Value, pull: bool) -> Result<Option<bool>> {
    match (pull, value["state"].as_str()) {
        (_, Some("open")) => Ok(None),
        (true, Some("closed")) => Ok(Some(
            value["merged"]
                .as_bool()
                .context("dependency PR has no merged fact")?,
        )),
        (false, Some("closed")) => match value["state_reason"].as_str() {
            Some("completed") => Ok(Some(true)),
            Some("not_planned") => Ok(Some(false)),
            _ => bail!("closed dependency issue has no completion reason"),
        },
        _ => bail!("dependency has no recognized state"),
    }
}

impl<R: Rest, G: Transport> Pass<'_, R, G> {
    fn dependency_marker(
        &mut self,
        repo: &str,
        number: u64,
        id: Option<u64>,
        held: &Held,
    ) -> Result<()> {
        let body = format!(
            "{TAG}{} -->\n\n{}\n\n{}",
            serde_json::to_string(held)?,
            held.reason,
            self.config.policy.footer
        );
        let (method, path) = match id {
            Some(id) => ("PATCH", format!("repos/{repo}/issues/comments/{id}")),
            None => ("POST", format!("repos/{repo}/issues/{number}/comments")),
        };
        let rest = self.rest;
        self.send(rest, method, &path, json!({"body": body}), 0)?;
        Ok(())
    }

    /// The single admission predicate for every path that can spend work on a
    /// PR. An unreleased marker holds even after prerequisites land: only the
    /// poll's release protocol may finish the branch update and draft release.
    fn dependency_admission(
        &mut self,
        repo: &str,
        number: u64,
        pull: &Value,
        comments: &[Value],
    ) -> Result<Option<Admission>> {
        if !self.config.policy.hold_dependencies {
            return Ok(None);
        }
        let previous = marker(comments, &self.config.bot)?;
        let refs = (|| -> Result<_> {
            ensure!(pull["body"].is_string(), "dependency PR body not read");
            let refs = crate::dependencies::parse(text(pull, "/body"), repo)?;
            let (native, next) = self.get_url(
                &format!(
                    "{API_ROOT}repos/{repo}/issues/{number}/dependencies/blocked_by?per_page={}",
                    crate::dependencies::LIMIT + 1
                ),
                Value::clone,
                true,
            )?;
            ensure!(next.is_none(), "native dependencies not read whole");
            crate::dependencies::with_native(refs, &native)
        })();
        if refs.as_ref().is_ok_and(BTreeSet::is_empty) && previous.is_none() {
            return Ok(None);
        }
        ensure!(
            pull.pointer("/labels/pageInfo/hasNextPage") == Some(&Value::Bool(false)),
            "dependency PR labels not read whole"
        );
        let held = previous
            .as_ref()
            .map(|(_, h)| h.clone())
            .unwrap_or_default();
        let mut failed = Vec::new();
        let mut blockers = Vec::new();
        // Treat malformed declarations and unreadable dependencies as holds,
        // not permission to spend a recovery round on an impossible head.
        match refs {
            Err(error) => failed.push(format!("dependency declaration: {error:#}")),
            Ok(refs) => {
                if refs.is_empty() && !held.released {
                    failed.push("dependency declarations removed during hold".into());
                }
                for (repository, n) in refs {
                    let target = format!("{repository}#{n}");
                    let result = (|| -> Result<Option<bool>> {
                        ensure!(
                            repository != repo.to_ascii_lowercase() || n != number,
                            "self dependency"
                        );
                        let (issue, _) = self.get_url(
                            &format!("{API_ROOT}repos/{repository}/issues/{n}"),
                            Value::clone,
                            true,
                        )?;
                        ensure!(issue["number"] == n, "dependency identity mismatch");
                        if issue["pull_request"].is_object() {
                            let (p, _) = self.get_url(
                                &format!("{API_ROOT}repos/{repository}/pulls/{n}"),
                                Value::clone,
                                true,
                            )?;
                            ensure!(p["number"] == n, "dependency PR identity mismatch");
                            satisfied(&p, true)
                        } else {
                            satisfied(&issue, false)
                        }
                    })();
                    match result {
                        Ok(Some(true)) => {}
                        Ok(Some(false)) => {
                            failed.push(format!("{target} closed without completion/merge"))
                        }
                        Ok(None) => blockers.push(target),
                        Err(error) => failed.push(format!("{target}: {error:#}")),
                    }
                }
            }
        }
        if blockers.is_empty() && failed.is_empty() && (held.released || previous.is_none()) {
            return Ok(None);
        }
        Ok(Some(Admission {
            previous,
            failed,
            blockers,
        }))
    }

    pub(super) fn dependency_work_hold(
        &mut self,
        repo: &str,
        pull: &Value,
    ) -> Result<Option<String>> {
        if !self.config.policy.hold_dependencies {
            return Ok(None);
        }
        let number = pull["number"].as_u64().context("dependency PR number")?;
        let comments = self.paged(
            &format!("repos/{repo}/issues/{number}/comments?per_page=100"),
            true,
        )?;
        Ok(self
            .dependency_admission(repo, number, pull, &comments)?
            .map(|state| {
                if !state.failed.is_empty() {
                    format!("dependency hold: {}", state.failed.join(", "))
                } else if !state.blockers.is_empty() {
                    format!("blocked by {}", state.blockers.join(", "))
                } else {
                    "dependency hold awaits branch update and draft release".into()
                }
            }))
    }

    pub(super) fn dependency_hold(
        &mut self,
        repo: &str,
        number: u64,
        pull: &Value,
        comments: &[Value],
    ) -> Result<Option<PullStep>> {
        let Some(Admission {
            previous,
            failed,
            blockers,
        }) = self.dependency_admission(repo, number, pull, comments)?
        else {
            return Ok(None);
        };
        let mut held = previous
            .as_ref()
            .map(|(_, h)| h.clone())
            .unwrap_or_default();
        if previous.as_ref().is_none_or(|(_, h)| h.released) {
            held.owns_draft = pull["isDraft"] == false;
        }
        let mut action = "blocked";
        let mut update = false;
        let mut ready = false;
        if !failed.is_empty() {
            action = "needs-human";
            held.released = false;
            held.reason = failed.join(", ");
        } else if !blockers.is_empty() {
            held.released = false;
            held.reason = format!("blocked by {}", blockers.join(", "));
        } else if !held.owns_draft && pull["isDraft"] != false {
            held.reason =
                "dependencies satisfied; pre-existing draft needs a person to release it".into();
        } else if checkpoint(pull)?.is_some()
            || text(pull, "/title").starts_with(PARTIAL_TITLE_PREFIX)
            || text(pull, "/body").starts_with(PARTIAL_BODY_PREFIX)
            || text(pull, "/title").starts_with(PLACEHOLDER_PREFIX)
        {
            held.reason = "dependencies satisfied; unfinished checkpoint stays draft".into();
        } else if text(pull, "/baseRefName") != text(pull, "/baseRepository/defaultBranchRef/name")
        {
            action = "needs-human";
            held.reason =
                "dependencies satisfied; non-default base needs retargeting before release".into();
        } else {
            let head = text(pull, "/headRefOid");
            let base = text(pull, "/baseRefName");
            let encoded: String = url::form_urlencoded::byte_serialize(base.as_bytes()).collect();
            let (comparison, _) = self.get_url(
                &format!("{API_ROOT}repos/{repo}/compare/{encoded}...{head}"),
                Value::clone,
                true,
            )?;
            match comparison["status"].as_str() {
                Some("ahead" | "identical") => {
                    held.reason =
                        "dependencies satisfied; head includes default branch, ready for review"
                            .into();
                    action = "dependency-ready";
                    ready = true;
                    held.released = true;
                }
                Some("behind" | "diverged")
                    if held.update_head.is_empty() && pull["mergeable"] == "MERGEABLE" =>
                {
                    held.update_head = head.into();
                    held.reason =
                        "dependencies satisfied; reserved one rebase onto default branch".into();
                    action = "dependency-update";
                    update = true;
                }
                _ => {
                    action = "needs-human";
                    held.reason = "dependencies satisfied; draft needs a branch update (conflicting, unreadable, or already attempted)".into();
                }
            }
        }
        if update
            && self
                .updated
                .iter()
                .any(|(r, _)| r.eq_ignore_ascii_case(repo))
        {
            update = false;
            held.update_head = previous
                .as_ref()
                .map(|(_, h)| h.update_head.clone())
                .unwrap_or_default();
            action = "blocked";
            held.reason =
                "dependencies satisfied; another PR was updated in this repository this pass"
                    .into();
        }
        if (ready || update) && has_label(pull, SUPERVISOR) {
            ready = false;
            update = false;
            held.update_head = previous
                .as_ref()
                .map(|(_, h)| h.update_head.clone())
                .unwrap_or_default();
            held.released = false;
            action = "needs-human";
            held.reason =
                "dependencies satisfied; needs-supervisor must be released before update or readiness".into();
        }
        let changed = previous.as_ref().is_none_or(|(_, old)| old != &held);
        let caps = self.caps;
        let planned = changed
            || update
            || (!ready && (pull["isDraft"] == false || !has_label(pull, BLOCKED)))
            || (ready && (pull["isDraft"] == true || has_label(pull, BLOCKED)))
            || (action == "needs-human" && !has_label(pull, SUPERVISOR));
        // A no-op may need repair after the fresh read (draft status or labels
        // can change), so even that path must check the write cap first.
        let capped = caps.and_then(|c| c.check(Kind::Write));
        let before = self.writes;
        let allowed = self.config.ours(repo) && self.config.writes_in(repo);
        let written = (|| -> Result<()> {
            if self.apply.is_some() && capped.is_none() && allowed {
                self.rest.start_kind(action);
                let (owner, name) = repo.split_once('/').context("repository")?;
                let current = self.read_pull(owner, name, number)?;
                ensure!(
                    current["state"] == "OPEN"
                        && current["headRefOid"] == pull["headRefOid"]
                        && current["body"] == pull["body"]
                        && current["title"] == pull["title"]
                        && current["isDraft"] == pull["isDraft"]
                        && current["baseRefName"] == pull["baseRefName"]
                        && current["baseRepository"]["defaultBranchRef"]
                            == pull["baseRepository"]["defaultBranchRef"]
                        && current.pointer("/labels/pageInfo/hasNextPage")
                            == Some(&Value::Bool(false))
                        && is_harness(&current, self.config)
                        && !is_held(&current)
                        && !labelled(&current)
                        && review_hold(self.config, &current).is_none(),
                    "dependency PR changed or held before write"
                );
                let id = previous.as_ref().map(|(id, _)| *id);
                // Persist the update reservation before the mutation. Ready is
                // recorded afterwards: a retry sees a ready PR and only records it.
                if changed && !ready {
                    self.dependency_marker(repo, number, id, &held)?;
                }
                if !ready && current["isDraft"] == false {
                    self.mutate(DRAFT, json!({"id": current["id"]}))?;
                }
                let rest = self.rest;
                if !ready && !has_label(&current, BLOCKED) {
                    self.send(
                        rest,
                        "POST",
                        &format!("repos/{repo}/issues/{number}/labels"),
                        json!({"labels": [BLOCKED]}),
                        0,
                    )?;
                }
                if action == "needs-human" && !has_label(&current, SUPERVISOR) {
                    self.send(
                        rest,
                        "POST",
                        &format!("repos/{repo}/issues/{number}/labels"),
                        json!({"labels": [SUPERVISOR]}),
                        0,
                    )?;
                }
                if update {
                    ensure!(
                        !has_label(&current, SUPERVISOR),
                        "dependency stop needs supervisor release"
                    );
                    self.updated
                        .insert((repo.to_owned(), text(pull, "/baseRefName").to_owned()));
                    self.mutate(
                        UPDATE,
                        json!({"id": current["id"], "head": current["headRefOid"]}),
                    )?;
                }
                if ready {
                    ensure!(
                        !has_label(&current, SUPERVISOR),
                        "dependency stop needs supervisor release"
                    );
                    if current["isDraft"] == true {
                        self.mutate(READY, json!({"id": current["id"]}))?;
                    }
                    if has_label(&current, BLOCKED) {
                        self.send(
                            rest,
                            "DELETE",
                            &format!("repos/{repo}/issues/{number}/labels/{BLOCKED}"),
                            Value::Null,
                            0,
                        )?;
                    }
                    if changed {
                        self.dependency_marker(repo, number, id, &held)?;
                    }
                }
            }
            Ok(())
        })();
        if capped.is_none()
            && ((self.apply.is_none() && allowed && planned) || self.writes > before)
            && let Some(caps) = caps
        {
            caps.count(Kind::Write);
        }
        written?;
        if update && self.apply.is_none() && capped.is_none() && allowed {
            self.updated
                .insert((repo.to_owned(), text(pull, "/baseRefName").to_owned()));
        }
        let reason = held.reason;
        let wait = (action == "needs-human").then(|| Wait {
            repository: repo.into(),
            number,
            head: text(pull, "/headRefOid").into(),
            kind: "needs-human",
            reason: reason.clone(),
            review_bucket: None,
            approval: None,
        });
        Ok(Some((
            format!(
                "{repo}#{number} {action}: {reason}{}{}",
                if allowed {
                    ""
                } else {
                    "; not taken outside own space"
                },
                capped
                    .as_ref()
                    .map(|s| format!("; {s}"))
                    .unwrap_or_default()
            ),
            false,
            wait,
            Some(Stepped {
                repository: repo.into(),
                number,
                head: text(pull, "/headRefOid").into(),
                action,
                reason,
                moved_at: moved_at(pull),
                waiting: WaitReason::Followed { since: None },
                ran: planned
                    && capped.is_none()
                    && allowed
                    && (self.apply.is_none() || self.writes > before),
            }),
        )))
    }
}

fn has_label(pull: &Value, label: &str) -> bool {
    nodes(pull, "/labels/nodes").iter().any(|l| {
        l["name"]
            .as_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(label))
    })
}
