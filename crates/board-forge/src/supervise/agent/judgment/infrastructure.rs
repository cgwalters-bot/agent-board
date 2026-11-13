//! One capped action owns the infrastructure hold lifecycle. It cannot touch a
//! human hold, choose API paths or publish an agent-written execution brief.
use super::*;
use crate::stop::{Cause, SUPERVISOR_LABEL, SUPERVISOR_PREFIX};

const BUG_LIMIT: usize = 100;
const BUG_LABEL: &str = "agent-board-infrastructure";
const FIX_QUERY: &str = r#"query($owner:String!,$name:String!,$number:Int!) {
  repository(owner:$owner,name:$name) { pullRequest(number:$number) {
    merged mergedAt closingIssuesReferences(first:100) {
      nodes { number repository { nameWithOwner } } pageInfo { hasNextPage }
    }
  } }
}"#;

struct Hold {
    comment: u64,
    marker: String,
    reason: String,
    cause: &'static str,
    tracking: Option<String>,
    since: i64,
}

fn hold(api: &dyn Api, p: &Policy, repo: &str, item: u64) -> Result<Hold> {
    let issue = api.get(&format!("/repos/{repo}/issues/{item}"))?;
    ensure!(
        issue["state"] == "open" && issue["labels"].is_array(),
        "Not an open held item"
    );
    ensure!(
        has_label(&issue, SUPERVISOR_LABEL),
        "Not held for the supervisor"
    );
    ensure!(
        !has_label(&issue, NEEDS_HUMAN)
            && !has_label(&issue, "needs-design")
            && !has_label(&issue, "epic")
            && !has_label(&issue, "question")
            && !OPERATOR_ONLY.iter().any(|l| has_label(&issue, l)),
        "The item is held for the operator"
    );
    let comments = paged(api, repo, item, "comments")?;
    let stopped = crate::poll::find_marker(&comments, &p.settings.bot);
    ensure!(stopped.stopped, "No current poll stop");
    let comment = comments
        .iter()
        .find(|c| c["id"].as_u64() == stopped.id)
        .context("Missing stop comment")?;
    let body = comment["body"].as_str().context("Missing stop body")?;
    let (reason, tracking) = body
        .lines()
        .find_map(|l| l.strip_prefix(SUPERVISOR_PREFIX))
        .and_then(|l| l.rsplit_once("; tracking "))
        .context("Not an infrastructure stop")?;
    let tracking = tracking
        .strip_suffix('.')
        .context("Malformed tracking reference")?;
    ensure!(
        reason.len() <= 2000 && !reason.contains(['\r', '\n', '`', '<', '>']),
        "Malformed stop reason"
    );
    Ok(Hold {
        comment: stopped.id.context("Missing stop ID")?,
        marker: body.lines().next().context("Missing marker")?.into(),
        reason: reason.into(),
        cause: Cause::from_tag(reason)
            .context("Not an infrastructure cause")?
            .name(),
        tracking: (tracking != "pending").then(|| tracking.to_owned()),
        since: millis(&comment["created_at"]).context("Missing stop creation time")?,
    })
}

fn bug_marker(cause: &str) -> String {
    format!("<!-- agent-board:infrastructure/{cause} -->")
}

/// One complete bounded page for new holds. Closed bugs are not actionable for
/// a recurrence; an already linked hold instead keeps its recorded bug identity.
/// Refuse a full page rather than guessing that a duplicate is not on page two.
fn bug(api: &dyn Api, p: &Policy, owner: &str, cause: &str) -> Result<Option<u64>> {
    let issues = api.get(&format!(
        "/repos/{owner}/issues?state=all&labels={BUG_LABEL}&per_page={BUG_LIMIT}"
    ))?;
    let issues = issues
        .as_array()
        .context("Missing infrastructure bug list")?;
    ensure!(
        issues.len() < BUG_LIMIT,
        "Infrastructure bug lookup is incomplete"
    );
    let marker = bug_marker(cause);
    let mut found = None;
    for issue in issues {
        if issue["pull_request"].is_object()
            || issue["state"] != "open"
            || issue["user"]["login"] != p.settings.bot
            || !issue["body"]
                .as_str()
                .is_some_and(|b| b.lines().any(|l| l == marker))
        {
            continue;
        }
        ensure!(
            found.is_none(),
            "Duplicate infrastructure bugs need reconciliation"
        );
        found = Some(
            issue["number"]
                .as_u64()
                .filter(|n| *n > 0)
                .context("Missing bug number")?,
        );
    }
    Ok(found)
}

fn linked_bug(
    api: &dyn Api,
    p: &Policy,
    owner: &str,
    held: &Hold,
    releasing: bool,
) -> Result<Option<u64>> {
    if let Some(tracking) = &held.tracking {
        let prefix = format!("https://github.com/{owner}/issues/");
        let number: u64 = tracking
            .strip_prefix(&prefix)
            .context("Cannot change the recorded infrastructure owner")?
            .parse()
            .context("Malformed tracking issue")?;
        ensure!(number > 0, "Invalid tracking issue");
        let issue = api.get(&format!("/repos/{owner}/issues/{number}"))?;
        ensure!(
            !issue["pull_request"].is_object()
                && issue["number"].as_u64() == Some(number)
                && issue["user"]["login"] == p.settings.bot
                && issue["body"]
                    .as_str()
                    .is_some_and(|b| b.lines().any(|l| l == bug_marker(held.cause))),
            "Recorded bug no longer matches this infrastructure hold"
        );
        return Ok(Some(number));
    }
    ensure!(
        !releasing,
        "Link the infrastructure bug before releasing its hold"
    );
    bug(api, p, owner, held.cause)
}

fn landed(api: &dyn Api, owner: &str, fix: u64, bug: u64, since: i64) -> Result<()> {
    let (org, name) = owner.split_once('/').context("Invalid owner repository")?;
    let data = api.graphql(FIX_QUERY, json!({"owner":org,"name":name,"number":fix}))?;
    let pull = &data["repository"]["pullRequest"];
    ensure!(pull["merged"] == true, "Infrastructure fix has not merged");
    ensure!(
        millis(&pull["mergedAt"]).context("Missing fix merge time")? >= since,
        "Infrastructure fix predates this hold"
    );
    let closing = &pull["closingIssuesReferences"];
    ensure!(
        closing["pageInfo"]["hasNextPage"] == false,
        "Incomplete fix references"
    );
    ensure!(
        closing["nodes"]
            .as_array()
            .context("Missing fix references")?
            .iter()
            .any(|i| i["number"].as_u64() == Some(bug)
                && i["repository"]["nameWithOwner"]
                    .as_str()
                    .is_some_and(|r| r.eq_ignore_ascii_case(owner))),
        "Merged fix does not close the infrastructure bug"
    );
    Ok(())
}

pub(super) fn ready(
    api: &dyn Api,
    p: &Policy,
    repo: &str,
    item: u64,
    owner: &str,
    fix: Option<u64>,
) -> Result<()> {
    let held = hold(api, p, repo, item)?;
    let bug = linked_bug(api, p, owner, &held, fix.is_some())?;
    if let Some(fix) = fix {
        landed(
            api,
            owner,
            fix,
            bug.context("No infrastructure bug to release")?,
            held.since,
        )?;
    }
    Ok(())
}

pub(super) fn apply(
    api: &dyn Api,
    p: &Policy,
    repo: &str,
    item: u64,
    owner: &str,
    fix: Option<u64>,
) -> Result<Value> {
    // Serialized caller; repeat discovery after admission and before creation.
    let held = hold(api, p, repo, item)?;
    let existing = linked_bug(api, p, owner, &held, fix.is_some())?;
    if let Some(fix) = fix {
        landed(
            api,
            owner,
            fix,
            existing.context("No infrastructure bug to release")?,
            held.since,
        )?;
    }
    let number = match existing {
        Some(number) => number,
        None => {
            match api.get(&format!("/repos/{owner}/labels/{BUG_LABEL}")) {
                Ok(label) => ensure!(
                    label["name"] == BUG_LABEL,
                    "Unexpected infrastructure label"
                ),
                Err(error) if error.is::<crate::github::NotFound>() => {
                    api.send("POST", &format!("/repos/{owner}/labels"), &json!({"name":BUG_LABEL,"color":"d93f0b","description":"Framework failures tracked by the supervisor"}))?;
                }
                Err(error) => return Err(error).context("Read infrastructure label"),
            }
            let receipt = api.send("POST", &format!("/repos/{owner}/issues"), &json!({
                "title": format!("Infrastructure: {}", held.cause),
                "labels": [BUG_LABEL],
                "body": format!("{}\n\nInvestigate and fix the framework failure holding https://github.com/{repo}/issues/{item}.\n\nCause: {}.\n\nRequest a bounded fix in this owning repository; link its pull request here. Do not change operator posture or target bounds.\n\n{}", bug_marker(held.cause), held.reason, p.settings.footer),
            }))?;
            receipt["number"]
                .as_u64()
                .filter(|n| *n > 0)
                .context("Created bug has no number")?
        }
    };
    let tracking = format!("https://github.com/{owner}/issues/{number}");
    // Preserve the poll protocol and edit its one stop comment, not a new note.
    let current = hold(api, p, repo, item)?;
    ensure!(
        current.comment == held.comment
            && current.reason == held.reason
            && current.tracking == held.tracking,
        "Infrastructure hold changed"
    );
    api.send("PATCH", &format!("/repos/{repo}/issues/comments/{}", held.comment), &json!({
        "body": format!("{}\n{}{}; tracking {}.\n\n{}", held.marker, SUPERVISOR_PREFIX, held.reason, tracking, p.settings.footer),
    }))?;
    if let Some(fix) = fix {
        landed(api, owner, fix, number, held.since)?;
        let current = hold(api, p, repo, item)?;
        ensure!(
            current.comment == held.comment
                && current.reason == held.reason
                && current.tracking.as_deref() == Some(&tracking),
            "Infrastructure hold changed"
        );
        api.send(
            "DELETE",
            &format!("/repos/{repo}/issues/{item}/labels/{SUPERVISOR_LABEL}"),
            &Value::Null,
        )?;
    }
    Ok(json!({"tracking":tracking,"released":fix.is_some()}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Forge {
        labels: Value,
        comments: RefCell<Value>,
        bugs: RefCell<Value>,
        fix: Value,
        writes: RefCell<Vec<(String, Value)>>,
        rehold: bool,
    }

    impl Default for Forge {
        fn default() -> Self {
            Self {
                labels: json!([{"name":SUPERVISOR_LABEL}]),
                comments: RefCell::new(
                    json!([{"id":7,"created_at":"2026-10-16T10:00:00Z","user":{"login":"worker"},"body":format!("<!-- {}: {{\"rounds\":1,\"stopped\":true}} -->\n{SUPERVISOR_PREFIX}[stale-base] rebase rounds exhausted; tracking pending.", crate::poll::MARKER)}]),
                ),
                bugs: RefCell::new(json!([])),
                fix: json!({"repository":{"pullRequest":{"merged":true,"mergedAt":"2026-10-16T11:00:00Z","closingIssuesReferences":{"nodes":[{"number":9,"repository":{"nameWithOwner":"example/control"}}],"pageInfo":{"hasNextPage":false}}}}}),
                writes: RefCell::new(vec![]),
                rehold: false,
            }
        }
    }

    impl Api for Forge {
        fn get(&self, path: &str) -> Result<Value> {
            match path {
                "/repos/example/project/issues/3" => {
                    Ok(json!({"state":"open","labels":self.labels}))
                }
                "/repos/example/project/issues/3/comments?per_page=100&page=1" => {
                    Ok(self.comments.borrow().clone())
                }
                "/repos/example/control/issues?state=all&labels=agent-board-infrastructure&per_page=100" => {
                    Ok(self.bugs.borrow().clone())
                }
                "/repos/example/control/labels/agent-board-infrastructure" => {
                    Ok(json!({"name":BUG_LABEL}))
                }
                "/repos/example/control/issues/9" => Ok(self.bugs.borrow()[0].clone()),
                _ => bail!("Unexpected GET {path}"),
            }
        }

        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            assert_eq!(query, FIX_QUERY);
            assert_eq!(
                variables,
                json!({"owner":"example","name":"control","number":10})
            );
            Ok(self.fix.clone())
        }

        fn send(&self, method: &str, path: &str, body: &Value) -> Result<Value> {
            self.writes
                .borrow_mut()
                .push((format!("{method} {path}"), body.clone()));
            if method == "POST" {
                self.bugs
                    .replace(json!([{"number":9,"state":"open","user":{"login":"worker"},"body":body["body"]}]));
                return Ok(json!({"number":9}));
            }
            if method == "PATCH" {
                self.comments.borrow_mut()[0]["body"] = body["body"].clone();
                if self.rehold {
                    self.comments.borrow_mut()[0]["id"] = json!(8);
                }
            }
            Ok(Value::Null)
        }

        fn download(&self, _: &str, _: u64) -> Result<Vec<u8>> {
            bail!("No downloads")
        }
    }

    fn settings(cap: u64) -> Settings {
        let mut cfg = crate::supervise::tests::supervisor();
        cfg["agent"]["dailyCaps"] = json!({"supervisor_hold":cap});
        Settings::from_json(&crate::supervise::tests::board(), &cfg, None).unwrap()
    }

    #[test]
    fn supervisor_hold_is_opt_in_scoped_and_has_no_free_text() {
        let request = json!({"type":"supervisor_action","action":"supervisor_hold","repository":"example/project","item":3,"owner":"example/control"});
        for (cap, field, value, valid) in [
            (0, "item", json!(3), false),
            (3, "item", json!(3), true),
            (3, "repository", json!("outside/project"), false),
            (3, "owner", json!("outside/control"), false),
            (3, "fix", json!(0), false),
            (3, "body", json!("Do something else"), false),
        ] {
            let s = settings(cap);
            let p = Policy::new(&s, &json!({"small":{},"large":{}}))
                .unwrap()
                .unwrap();
            let mut request = request.clone();
            request[field] = value;
            assert_eq!(
                parse_batch(&request.to_string(), &p).is_ok(),
                valid,
                "{request}"
            );
        }
    }

    #[test]
    fn supervisor_hold_groups_links_edits_and_releases_only_a_landed_fix() {
        let s = settings(3);
        let p = Policy::new(&s, &json!({"small":{},"large":{}}))
            .unwrap()
            .unwrap();
        let api = Forge::default();
        ready(&api, &p, "example/project", 3, "example/control", None).unwrap();
        assert!(api.writes.borrow().is_empty());
        for _ in 0..2 {
            apply(&api, &p, "example/project", 3, "example/control", None).unwrap();
        }
        assert_eq!(
            api.writes
                .borrow()
                .iter()
                .filter(|(m, _)| m.starts_with("POST "))
                .count(),
            1
        );
        let body = api.comments.borrow()[0]["body"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(body.contains("Held for the supervisor: [stale-base] rebase rounds exhausted; tracking https://github.com/example/control/issues/9."));
        assert!(
            crate::poll::find_marker(api.comments.borrow().as_array().unwrap(), "worker").stopped
        );
        apply(&api, &p, "example/project", 3, "example/control", Some(10)).unwrap();
        assert_eq!(
            api.writes.borrow().last().unwrap().0,
            "DELETE /repos/example/project/issues/3/labels/needs-supervisor"
        );
    }

    #[test]
    fn supervisor_hold_refuses_posture_partial_evidence_and_reholds() {
        let s = settings(3);
        let p = Policy::new(&s, &json!({"small":{},"large":{}}))
            .unwrap()
            .unwrap();
        for label in [
            "needs-human",
            "needs-design",
            "credentials",
            "bounds",
            "security",
            "budget",
        ] {
            let api = Forge {
                labels: json!([{"name":SUPERVISOR_LABEL},{"name":label}]),
                ..Forge::default()
            };
            assert!(
                ready(&api, &p, "example/project", 3, "example/control", None).is_err(),
                "{label}"
            );
            assert!(api.writes.borrow().is_empty());
        }
        let api = Forge::default();
        api.bugs.replace(
            json!([{"number":9,"state":"open","user":{"login":"worker"},"body":bug_marker("stale-base")}]),
        );
        apply(&api, &p, "example/project", 3, "example/control", None).unwrap();
        api.writes.borrow_mut().clear();
        for (pointer, value) in [
            ("/repository/pullRequest/merged", json!(false)),
            (
                "/repository/pullRequest/mergedAt",
                json!("2026-10-16T09:00:00Z"),
            ),
            (
                "/repository/pullRequest/closingIssuesReferences/pageInfo/hasNextPage",
                json!(true),
            ),
            (
                "/repository/pullRequest/closingIssuesReferences/nodes/0/number",
                json!(8),
            ),
        ] {
            let mut api = Forge::default();
            api.bugs.replace(
                json!([{"number":9,"state":"open","user":{"login":"worker"},"body":bug_marker("stale-base")}]),
            );
            apply(&api, &p, "example/project", 3, "example/control", None).unwrap();
            api.writes.borrow_mut().clear();
            *api.fix.pointer_mut(pointer).unwrap() = value;
            assert!(apply(&api, &p, "example/project", 3, "example/control", Some(10)).is_err());
            assert!(api.writes.borrow().is_empty());
        }
        let api = Forge {
            rehold: true,
            ..api
        };
        assert!(apply(&api, &p, "example/project", 3, "example/control", Some(10)).is_err());
        assert!(
            !api.writes
                .borrow()
                .iter()
                .any(|(m, _)| m.starts_with("DELETE "))
        );
        let api = Forge::default();
        api.bugs.replace(json!(vec![json!({}); BUG_LIMIT]));
        assert!(ready(&api, &p, "example/project", 3, "example/control", None).is_err());
    }

    #[test]
    fn supervisor_hold_binds_release_and_groups_only_open_occurrences() {
        let s = settings(3);
        let p = Policy::new(&s, &json!({"small":{},"large":{}}))
            .unwrap()
            .unwrap();
        let api = Forge::default();
        // A new failure after the old fix needs an actionable new occurrence.
        api.bugs.replace(json!([{"number":8,"state":"closed","user":{"login":"worker"},"body":bug_marker("stale-base")}]));
        assert!(apply(&api, &p, "example/project", 3, "example/control", Some(10)).is_err());
        assert!(api.writes.borrow().is_empty());
        apply(&api, &p, "example/project", 3, "example/control", None).unwrap();
        assert_eq!(
            api.writes
                .borrow()
                .iter()
                .filter(|(m, _)| m.starts_with("POST "))
                .count(),
            1
        );
        api.writes.borrow_mut().clear();
        // Even another allowed repository cannot substitute its same-cause bug.
        assert!(apply(&api, &p, "example/project", 3, "example/other", Some(10)).is_err());
        assert!(api.writes.borrow().is_empty());
        apply(&api, &p, "example/project", 3, "example/control", Some(10)).unwrap();
        assert!(api.writes.borrow().last().unwrap().0.starts_with("DELETE "));
    }
}
