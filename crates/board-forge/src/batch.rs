//! Batched implement runs (cgwalters-forge/agent-board#86): one run and one
//! pull request for an issue and up to N small, ready sibling sub-issues of
//! the same epic, so small fixes stop paying a runner, a review and a merge
//! round each and stop conflicting with each other while they wait.
//!
//! Off unless the caller's configuration asks for it (`dispatch --batch N`).
//! An extra issue is taken only with its own accepted implement plan, whose
//! brief becomes its part of the run's task, and gets the same dispatch record
//! as a primary, so whatever skips recorded plans skips it while the run is
//! out. See docs/job-plans.md#batching.
use crate::github::Transport;
use crate::plan::{Comment, JobPlan, issue_comments, newest_plan, plan_id};
use crate::poll::validate_repository;
use anyhow::{Context, Result, ensure};
use board_core::ItemRef;
use serde_json::{Value, json};

/// Extra issues a caller may add to one run. Each adds a brief to the task
/// and a record to write, so a configuration cannot make a run unbounded.
pub const BATCH_LIMIT: usize = 5;
/// The label an issue's planner or the operator puts on an issue small
/// enough to share a run. Unlabelled issues, large or P0 ones among them,
/// run alone: the label is the only size this reads.
pub const SMALL_LABEL: &str = "small";
/// The label that keeps an issue out of every batch, as primary or extra.
pub const NO_BATCH_LABEL: &str = "no-batch";
/// The tag of the hidden line in an extra issue's dispatch record that names
/// the batch's primary issue: `<!-- agent-board-batch: OWNER/REPO#N -->`.
pub const BATCH_MARKER: &str = "agent-board-batch";
/// Eligible siblings whose comments are read for a plan, at most, so an epic
/// of a hundred sub-issues without plans costs a bounded number of reads.
pub const BATCH_READS: usize = 10;
/// Characters of a batched run's whole task. GitHub caps a dispatch's inputs
/// at 65,535 characters together; extras that would pass this are left out.
pub const BATCH_BRIEF_LIMIT: usize = 32_000;
/// Text the scheduler also takes for an unrecorded dependency.
const BLOCKED_WORDS: &[&str] = &["blocked by", "depends on", "waiting on", "waiting for"];
const LABEL_PAGE: u64 = 100;

const FAMILY_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      ...Candidate
      parent {
        subIssues(first: 100) { nodes { ...Candidate } pageInfo { hasNextPage } }
      }
    }
  }
}
fragment Candidate on Issue {
  number
  state
  body
  repository { nameWithOwner }
  labels(first: 100) { nodes { name } totalCount }
  issueDependenciesSummary { blockedBy }
  closedByPullRequestsReferences(first: 1, includeClosedPrs: true) { totalCount }
}"#;

const STATE_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) { issue(number: $number) { state } }
}"#;

/// Open pull requests that close an issue; ten is plenty to find one.
const OPEN_PULLS_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) { issue(number: $number) {
    closedByPullRequestsReferences(first: 10, includeClosedPrs: false) { nodes { state } }
  } }
}"#;

/// An issue as far as choosing a batch needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub repo: String,
    pub number: u64,
    pub open: bool,
    pub labels: Vec<String>,
    /// Whether every label was read: a hidden `no-batch` must not be missed.
    pub labels_complete: bool,
    /// Open issues GitHub records as blocking this one.
    pub blockers: u64,
    pub body: String,
    /// Pull requests that close it, open, merged or closed.
    pub pulls: u64,
}

impl Candidate {
    fn from_node(node: &Value) -> Result<Self> {
        let count = |pointer: &str| {
            node.pointer(pointer)
                .and_then(Value::as_u64)
                .with_context(|| format!("issue has no {pointer}"))
        };
        let labels = node
            .pointer("/labels/nodes")
            .and_then(Value::as_array)
            .context("issue has no labels")?
            .iter()
            .map(|l| l["name"].as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .context("label has no name")?;
        let total = count("/labels/totalCount")?;
        Ok(Self {
            repo: node
                .pointer("/repository/nameWithOwner")
                .and_then(Value::as_str)
                .context("issue has no repository")?
                .into(),
            number: count("/number")?,
            open: node["state"].as_str().context("issue has no state")? == "OPEN",
            labels_complete: total <= LABEL_PAGE && total == labels.len() as u64,
            labels,
            blockers: count("/issueDependenciesSummary/blockedBy")?,
            body: node["body"].as_str().unwrap_or_default().into(),
            pulls: count("/closedByPullRequestsReferences/totalCount")?,
        })
    }

    /// Why this issue may not share a run, or `None`. Besides opting out
    /// with a label, each reason is work a batch could duplicate or do too
    /// early: a pull request already closes it, or something it waits on is
    /// open. Batch members cannot depend on each other, since a blocked issue
    /// is never taken and an open primary or extra counts as a blocker.
    pub fn refusal(&self) -> Option<&'static str> {
        let labelled = |name: &str| self.labels.iter().any(|l| l.eq_ignore_ascii_case(name));
        let body = self.body.to_lowercase();
        if !self.open {
            Some("closed")
        } else if !self.labels_complete {
            Some("has too many labels to read")
        } else if labelled(NO_BATCH_LABEL) {
            Some("labelled no-batch")
        } else if !labelled(SMALL_LABEL) {
            Some("not labelled small")
        } else if self.blockers > 0 || BLOCKED_WORDS.iter().any(|w| body.contains(w)) {
            Some("blocked")
        } else if self.pulls > 0 {
            Some("has a pull request")
        } else {
            None
        }
    }
}

/// An issue and its epic's other sub-issues; `siblings` is empty for an
/// issue with no parent.
pub struct Family {
    pub primary: Candidate,
    pub siblings: Vec<Candidate>,
}

pub fn family<T: Transport>(transport: &T, issue: &ItemRef) -> Result<Family> {
    let (owner, name) = issue
        .repository
        .split_once('/')
        .context("issue repository")?;
    let data = transport.graphql(
        FAMILY_QUERY,
        json!({"owner": owner, "name": name, "number": issue.number}),
    )?;
    let node = data
        .pointer("/repository/issue")
        .filter(|n| n.is_object())
        .with_context(|| format!("{} is not an issue", issue.display()))?;
    let mut siblings = Vec::new();
    if let Some(subs) = node["parent"].get("subIssues") {
        // GitHub allows 100 sub-issues per parent; more is not the API this reads.
        ensure!(
            subs.pointer("/pageInfo/hasNextPage") == Some(&Value::Bool(false)),
            "the epic of {} has more sub-issues than one page",
            issue.display()
        );
        for sub in subs["nodes"]
            .as_array()
            .context("sub-issues have no nodes")?
        {
            siblings.push(Candidate::from_node(sub)?);
        }
    }
    Ok(Family {
        primary: Candidate::from_node(node)?,
        siblings,
    })
}

/// The sibling issues that may join `primary`'s run, oldest first, or why
/// none may. They must be in the same repository: one run checks out one.
pub fn eligible(family: &Family) -> Result<Vec<u64>, String> {
    let primary = &family.primary;
    if let Some(why) = primary.refusal() {
        return Err(format!("{}#{} {why}", primary.repo, primary.number));
    }
    if family.siblings.is_empty() {
        return Err(format!(
            "{}#{} is in no epic with other sub-issues",
            primary.repo, primary.number
        ));
    }
    let mut numbers: Vec<u64> = family
        .siblings
        .iter()
        .filter(|s| {
            s.number != primary.number
                && s.repo.eq_ignore_ascii_case(&primary.repo)
                && s.refusal().is_none()
        })
        .map(|s| s.number)
        .collect();
    numbers.sort_unstable();
    Ok(numbers)
}

/// An issue that joins a batch, with the plan that gives its brief.
pub struct Extra {
    pub issue: ItemRef,
    pub plan: JobPlan,
    pub from: Comment,
    pub id: String,
}

/// What a batch came to: the extras, and why candidates were left out.
#[derive(Default)]
pub struct Batch {
    pub extras: Vec<Extra>,
    pub notes: Vec<String>,
}

/// Choose up to `limit` extras for the run `plan` on `issue` would start.
///
/// Each needs an accepted implement plan of its own, by the rules primary
/// plans follow, for the same issue, runner and pool, and no record that
/// still holds it ([`held`]). An epic or a sibling that cannot be read, like
/// an unusable plan, only leaves extras out: the issue then runs alone, as
/// it would without batching.
pub fn select<T: Transport>(
    transport: &T,
    issue: &ItemRef,
    plan: &JobPlan,
    writers: &[&str],
    limit: usize,
) -> Result<Batch> {
    ensure!(
        limit <= BATCH_LIMIT,
        "a batch has at most {BATCH_LIMIT} extra issues"
    );
    let mut batch = Batch::default();
    if limit == 0 {
        return Ok(batch);
    }
    if plan.kind != "implement" || !targets(plan, issue) {
        batch
            .notes
            .push("the plan is not an implement plan for its own issue".into());
        return Ok(batch);
    }
    let family = match family(transport, issue) {
        Ok(family) => family,
        Err(why) => {
            batch
                .notes
                .push(format!("the epic could not be read: {why:#}"));
            return Ok(batch);
        }
    };
    let numbers = match eligible(&family) {
        Ok(numbers) => numbers,
        Err(why) => {
            batch.notes.push(why);
            return Ok(batch);
        }
    };
    // The whole task an extra would join, preamble included: without extras
    // the bare brief is shorter still.
    let mut length = batched(plan).chars().count();
    for number in numbers.into_iter().take(BATCH_READS) {
        if batch.extras.len() == limit {
            break;
        }
        let extra = ItemRef {
            number,
            ..issue.clone()
        };
        let comments = issue_comments(transport, &extra);
        let found = comments.and_then(|comments| {
            let (p, from) = newest_plan(&comments, writers)?;
            ensure!(
                p.kind == "implement" && targets(&p, &extra),
                "not an implement plan for this issue"
            );
            ensure!(
                p.runner == plan.runner && p.pool == plan.pool,
                "plan asks for another runner or pool"
            );
            // Its run starts from another branch than the primary's.
            ensure!(p.stacks_on.is_none(), "plan stacks on another issue");
            let id = plan_id(from)?;
            if let Some(record) = held(transport, &comments, writers, &id, &extra)?.first() {
                anyhow::bail!("recorded already, {}", record.1.url);
            }
            Ok((p, from.clone(), id))
        });
        let (p, from, id) = match found {
            Ok(found) => found,
            Err(why) => {
                batch.notes.push(format!("{}: {why:#}", extra.display()));
                continue;
            }
        };
        let added = section(&extra, &p).chars().count();
        if length + added > BATCH_BRIEF_LIMIT {
            batch.notes.push(format!(
                "{}: brief would pass {BATCH_BRIEF_LIMIT} characters",
                extra.display()
            ));
            continue;
        }
        length += added;
        batch.extras.push(Extra {
            issue: extra,
            plan: p,
            from,
            id,
        });
    }
    Ok(batch)
}

fn targets(plan: &JobPlan, issue: &ItemRef) -> bool {
    plan.repo.eq_ignore_ascii_case(&issue.repository) && plan.item == issue.number
}

fn section(issue: &ItemRef, plan: &JobPlan) -> String {
    format!("\n\n## {}\n\n{}", issue.display(), plan.brief)
}

/// A batched run's task before the extras' sections: the primary plan's
/// brief and what a batched run must do.
fn batched(plan: &JobPlan) -> String {
    format!(
        "{}\n\n---\n\nBatched run: this run also does the issues below, each from its own plan.\n\
         - Make one commit per issue, so review and a revert stay per issue.\n\
         - Open one pull request. Its body has a `Closes #N` line for each issue you finished, #{} included.\n\
         - Name in the body each listed issue you did not do, and why; it is left for a later run.",
        plan.brief, plan.item
    )
}

/// The run's task: the primary plan's brief, unchanged without extras, else
/// followed by what a batched run must do and each extra's brief.
pub fn brief(plan: &JobPlan, extras: &[&Extra]) -> String {
    if extras.is_empty() {
        return plan.brief.clone();
    }
    let mut task = batched(plan);
    for extra in extras {
        task.push_str(&section(&extra.issue, &extra.plan));
    }
    task
}

/// The hidden line that marks a record as an extra's in `primary`'s batch.
pub(crate) fn batch_line(primary: &ItemRef) -> String {
    format!("<!-- {BATCH_MARKER}: {} -->", primary.display())
}

/// The records of `plan` on `issue` that still hold it, oldest first.
///
/// An extra's record is released once the batch's primary issue is closed
/// and no open pull request closes the extra: the run's pull request merged
/// or the operator closed the primary, and an extra still open was not
/// finished, so a later run may take it (the partial-completion reset). An
/// operator who closes the primary early, while the batch's pull request is
/// still open, frees nothing that pull request closes, so it is not done
/// twice. An unreadable primary or extra holds the record.
pub(crate) fn held<'a, T: Transport>(
    transport: &T,
    comments: &'a [Comment],
    writers: &[&str],
    plan: &str,
    issue: &ItemRef,
) -> Result<Vec<(u64, &'a Comment)>> {
    let mut out = Vec::new();
    for record in crate::plan::records(comments, writers, plan) {
        let primary = record.1.body.lines().nth(1).and_then(|line| {
            let named = line
                .trim_end()
                .strip_prefix(&format!("<!-- {BATCH_MARKER}: "))?
                .strip_suffix(" -->")?;
            let (repo, number) = named.rsplit_once('#')?;
            validate_repository(repo).ok()?;
            Some(ItemRef {
                repository: repo.into(),
                number: number.parse().ok()?,
                ..issue.clone()
            })
        });
        let released = match primary {
            Some(primary) if primary != *issue => {
                closed(transport, &primary)? && !open_pull(transport, issue)?
            }
            _ => false,
        };
        if !released {
            out.push(record);
        }
    }
    Ok(out)
}

fn issue_node<T: Transport>(transport: &T, query: &str, issue: &ItemRef) -> Result<Value> {
    let (owner, name) = issue
        .repository
        .split_once('/')
        .context("issue repository")?;
    let data = transport.graphql(
        query,
        json!({"owner": owner, "name": name, "number": issue.number}),
    )?;
    data.pointer("/repository/issue")
        .filter(|n| n.is_object())
        .cloned()
        .with_context(|| format!("{} is not an issue", issue.display()))
}

fn closed<T: Transport>(transport: &T, issue: &ItemRef) -> Result<bool> {
    let state = issue_node(transport, STATE_QUERY, issue)?["state"]
        .as_str()
        .map(str::to_owned)
        .with_context(|| format!("{} has no state", issue.display()))?;
    Ok(state == "CLOSED")
}

fn open_pull<T: Transport>(transport: &T, issue: &ItemRef) -> Result<bool> {
    let node = issue_node(transport, OPEN_PULLS_QUERY, issue)?;
    let pulls = node
        .pointer("/closedByPullRequestsReferences/nodes")
        .and_then(Value::as_array)
        .with_context(|| format!("{} has no closing pull requests", issue.display()))?;
    Ok(pulls.iter().any(|p| p["state"] == "OPEN"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(number: u64) -> Candidate {
        Candidate {
            repo: "example/work".into(),
            number,
            open: true,
            labels: vec!["Small".into()],
            labels_complete: true,
            blockers: 0,
            body: "Fix the typo.".into(),
            pulls: 0,
        }
    }

    #[test]
    fn candidate_from_node() {
        let node = json!({"number": 7, "state": "OPEN", "body": null,
            "repository": {"nameWithOwner": "example/work"},
            "labels": {"nodes": [{"name": "small"}], "totalCount": 1},
            "issueDependenciesSummary": {"blockedBy": 0},
            "closedByPullRequestsReferences": {"totalCount": 0}});
        let read = Candidate::from_node(&node).unwrap();
        assert_eq!(read.refusal(), None);
        let mut hidden = node.clone();
        hidden["labels"]["totalCount"] = json!(101);
        let read = Candidate::from_node(&hidden).unwrap();
        assert_eq!(read.refusal(), Some("has too many labels to read"));
        let mut missing = node;
        missing["issueDependenciesSummary"] = Value::Null;
        assert!(Candidate::from_node(&missing).is_err());
    }

    #[test]
    fn batch_selection_table() {
        type Edit = fn(&mut Candidate);
        // Name, an edit of sibling 12, and whether it may still join.
        let cases: &[(&str, Edit, bool)] = &[
            ("ready", |_| {}, true),
            (
                "other repository",
                |c| c.repo = "example/other".into(),
                false,
            ),
            ("closed", |c| c.open = false, false),
            ("not small", |c| c.labels = vec!["P2".into()], false),
            ("unlabelled", |c| c.labels.clear(), false),
            ("opted out", |c| c.labels.push("no-batch".into()), false),
            ("labels unread", |c| c.labels_complete = false, false),
            ("dependency open", |c| c.blockers = 1, false),
            (
                "blocked in text",
                |c| c.body = "Depends on #3.".into(),
                false,
            ),
            (
                "waiting in text",
                |c| c.body = "Waiting for D1.".into(),
                false,
            ),
            ("pull request", |c| c.pulls = 1, false),
        ];
        for (name, edit, joins) in cases {
            let mut sibling = candidate(12);
            edit(&mut sibling);
            let family = Family {
                primary: candidate(10),
                // The primary is among its epic's sub-issues and never its own extra.
                siblings: vec![sibling, candidate(10), candidate(11)],
            };
            let expected = if *joins { vec![11, 12] } else { vec![11] };
            assert_eq!(eligible(&family), Ok(expected), "{name}");
        }
        // A primary that may not share a run takes no extras.
        let cases: &[(&str, Edit, &str)] = &[
            ("not small", |c| c.labels.clear(), "not labelled small"),
            (
                "opted out",
                |c| c.labels.push("NO-BATCH".into()),
                "no-batch",
            ),
            ("blocked", |c| c.blockers = 2, "blocked"),
            ("pull request", |c| c.pulls = 1, "has a pull request"),
        ];
        for (name, edit, error) in cases {
            let mut primary = candidate(10);
            edit(&mut primary);
            let family = Family {
                primary,
                siblings: vec![candidate(11)],
            };
            let why = eligible(&family).unwrap_err();
            assert!(why.contains(error), "{name}: {why}");
        }
        let alone = Family {
            primary: candidate(10),
            siblings: vec![],
        };
        assert_eq!(
            eligible(&alone),
            Err("example/work#10 is in no epic with other sub-issues".into())
        );
    }
}
