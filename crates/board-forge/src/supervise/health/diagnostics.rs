//! Additional alarms, not recovery authority. All reads share Collector's bounds.
use super::*;

/// Inclusive queue residence alarm. GitHub supplies no state transition time.
const QUEUE_RESIDENCE_MS: i64 = HOUR_MS;
/// Conservative grace from the last PR update, including head/draft changes.
const REVIEW_GRACE_MS: i64 = 15 * 60_000;
const REPORT_BYTES: usize = 4 * 1024;
/// Runs read for current-head reviews that cite them, newest first.
const CITED_RUN_READS: usize = 3;

impl Collector<'_> {
    pub(super) fn aged_queue(&self) -> Result<Vec<Value>> {
        let mut result = vec![];
        for p in self.open_pulls()? {
            ensure!(
                p["isInMergeQueue"].is_boolean(),
                "queue membership unavailable"
            );
            if p["isInMergeQueue"] != true {
                continue;
            }
            let entry = &p["mergeQueueEntry"];
            let at = millis(&entry["enqueuedAt"]).context("queue enqueue time")?;
            ensure!(at <= self.now, "future queue enqueue time");
            let state = entry["state"].as_str().context("queue state")?;
            let head = entry["headCommit"]["oid"].as_str().context("queue head")?;
            ensure!(!head.is_empty(), "empty queue head");
            let repo = p["baseRepository"]["nameWithOwner"]
                .as_str()
                .context("queue repository")?;
            let runs = self.cached_list(&format!(
                "/repos/{repo}/actions/runs?event=merge_group&head_sha={}",
                encode(head)
            ))?;
            let mut matching = vec![];
            for run in &runs {
                if run["event"] == "merge_group" && run["head_sha"] == head {
                    let created = millis(&run["created_at"]).context("merge group run time")?;
                    matching.push((created, run["run_attempt"].as_u64().unwrap_or(1), run));
                }
            }
            matching.sort_by_key(|(at, attempt, _)| (*at, *attempt));
            let run = matching.last().map(|(_, _, r)| json!({"html_url":r["html_url"], "status":r["status"], "conclusion":r["conclusion"], "created_at":r["created_at"], "run_attempt":r["run_attempt"]}));
            result.push(json!({"html_url":p["url"], "state":state,
                "age_basis":"queue residence (not unchanged state)", "since":iso(at),
                "age_minutes":(self.now-at)/60_000, "observed_at":iso(self.now),
                "merge_group_head":head, "latest_merge_group_run":run,
                "problem":self.now-at >= QUEUE_RESIDENCE_MS}));
        }
        problems(result, self.s.limits.max_items)
    }

    pub(super) fn lost_reviews(&self) -> Result<Vec<Value>> {
        let mut result = vec![];
        for p in self.open_pulls()? {
            // The existing merge/review callers cover these authors. Drafts
            // and other contributors are not assumed to have opted in.
            let author = login(&p["author"]);
            if p["isDraft"] == true
                || !(author == self.s.bot || self.s.merge_authors.contains(&author))
            {
                continue;
            }
            ensure!(p["isDraft"] == false, "draft status unavailable");
            let workflow = self
                .s
                .review_workflow
                .as_deref()
                .context("reviewWorkflow not configured")?;
            let head = p["headRefOid"]
                .as_str()
                .filter(|h| !h.is_empty())
                .context("review head")?;
            let number = p["number"].as_u64().context("review PR number")?;
            let repo = p["baseRepository"]["nameWithOwner"]
                .as_str()
                .context("review repository")?;
            let updated = millis(&p["updatedAt"]).context("review grace time")?;
            ensure!(updated <= self.now, "future PR update");
            // The head's own runs, and dispatches since the lookback, which
            // run on the default branch SHA and name the head in their title.
            // A full head page or more dispatches than maxPages is unavailable,
            // never a claim of no review.
            let path = format!("/repos/{repo}/actions/workflows/{workflow}/runs");
            let mut runs = self.cached_list(&format!(
                "{path}?head_sha={}&event=pull_request_target",
                encode(head)
            ))?;
            runs.extend(self.review_dispatches(&path)?);
            let matching: Vec<&Value> = runs
                .iter()
                .filter(|r| crate::poll::review_head_run(&p, r, Some(workflow), true))
                .filter(|r| {
                    r["event"] == "workflow_dispatch"
                        || r["pull_requests"].as_array().is_none_or(|prs| {
                            prs.is_empty()
                                || prs.iter().any(|pr| {
                                    pr["number"] == number
                                        && pr["head"]["sha"] == head
                                        && pr["base"]["repo"]["full_name"] == repo
                                })
                        })
                })
                .collect();
            // Fork runs often have pull_requests: []; the head SHA they were
            // listed and matched by is their association then.
            let satisfied = matching.iter().any(|run| {
                RUNNING.contains(&run["status"].as_str().unwrap_or_default())
                    || (run["status"] == "completed" && run["conclusion"] == "success")
            }) || self.cited_review(&p, &runs, workflow)?;
            let examples: Vec<&Value> = matching
                .iter()
                .filter_map(|r| r.get("html_url"))
                .take(self.s.limits.max_evidence)
                .collect();
            result.push(
                json!({"html_url":p["url"], "head":head, "workflow":workflow,
                "grace_basis":"last PR update", "since":iso(updated), "examples":examples,
                "problem":!satisfied && self.now-updated >= REVIEW_GRACE_MS}),
            );
        }
        problems(result, self.s.limits.max_items)
    }

    /// The review workflow's dispatched runs in the lookback, once a repository.
    fn review_dispatches(&self, path: &str) -> Result<Vec<Value>> {
        let value = self.once(&format!("review dispatches {path}"), || {
            let since = encode(&format!(">={}", iso(self.lookback())));
            Ok(Value::Array(self.run_pages(
                &format!("{path}?event=workflow_dispatch&created={since}"),
                "review dispatches",
            )?))
        })?;
        Ok(value.as_array().cloned().unwrap_or_default())
    }

    /// Whether a current-head review by the Actions bot (as agentic-job's
    /// apply posts them), the bot or the operator cites a review run for that
    /// head, run to success and submitted during its latest attempt, as the
    /// poll counts a review: a run older than the reads. The newest
    /// [`CITED_RUN_READS`] citing reviews are checked.
    fn cited_review(&self, pull: &Value, runs: &[Value], workflow: &str) -> Result<bool> {
        let repo = pull["baseRepository"]["nameWithOwner"]
            .as_str()
            .context("review repository")?;
        let number = pull["number"].as_u64().context("review PR number")?;
        let reviews = self.once(&format!("reviews {repo}#{number}"), || {
            Ok(Value::Array(self.paged(
                &format!("/repos/{repo}/pulls/{number}/reviews"),
                self.s.limits.max_pages,
            )?))
        })?;
        let cited = reviews
            .as_array()
            .context("reviews")?
            .iter()
            .rev()
            .filter(|review| {
                let user = &review["user"];
                let who = user["login"].as_str().unwrap_or_default();
                let actions = user["type"] == "Bot"
                    && user["id"].as_u64() == Some(crate::poll::ACTIONS_BOT_ID);
                review["commit_id"] == pull["headRefOid"]
                    && (actions || [self.s.bot.as_str(), self.s.operator.as_str()].contains(&who))
            })
            .filter_map(|review| {
                let (r, id) = crate::poll::cited_run(review["body"].as_str()?)?;
                (r == repo).then(|| (millis(&review["submitted_at"]), id))
            })
            .take(CITED_RUN_READS);
        for (submitted, id) in cited {
            let run = match runs.iter().find(|r| r["id"] == id) {
                Some(run) => run.clone(),
                None => self.cached_get(&format!("/repos/{repo}/actions/runs/{id}"))?,
            };
            let attempt = millis(&run["run_started_at"]).zip(millis(&run["updated_at"]));
            if run["status"] == "completed"
                && run["conclusion"] == "success"
                && submitted
                    .zip(attempt)
                    .is_some_and(|(at, (start, end))| start <= at && at <= end)
                && crate::poll::review_head_run(pull, &run, Some(workflow), true)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Only problems are reported, so healthy items on a busy repository do not
/// exhaust the bounds.
fn problems(items: Vec<Value>, max: usize) -> Result<Vec<Value>> {
    bounded(
        items.into_iter().filter(|i| i["problem"] == true).collect(),
        max,
    )
}

/// Reports must be whole, not silently trimmed into a healthy subset.
pub(super) fn bounded(items: Vec<Value>, max: usize) -> Result<Vec<Value>> {
    ensure!(items.len() <= max, "diagnostic items truncated");
    ensure!(
        serde_json::to_vec(&items)?.len() <= REPORT_BYTES,
        "diagnostic byte bound"
    );
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::{Bounded, tests::settings};

    struct Recorded {
        data: Value,
        next_page: bool,
        reads: RefCell<Vec<String>>,
    }

    impl Recorded {
        fn new() -> Self {
            Self {
                data: serde_json::from_str(include_str!(
                    "../../../../../fixtures/supervise/health-diagnostics.json"
                ))
                .unwrap(),
                next_page: false,
                reads: RefCell::new(vec![]),
            }
        }
    }

    impl Api for Recorded {
        fn get(&self, path: &str) -> Result<Value> {
            self.reads.borrow_mut().push(path.to_owned());
            if path.contains("event=merge_group") {
                return Ok(json!({"workflow_runs":self.data["queue_runs"]}));
            }
            let history = |key: &str| self.data[key].as_array().cloned().unwrap_or_default();
            let mut reviews = vec![self.data["review"].clone()];
            reviews.extend(history("review_history"));
            if path.contains("/workflows/review.yml/runs?") {
                return Ok(listed(reviews, path));
            }
            if let Some(id) = path.strip_prefix("/repos/example/project/actions/runs/") {
                return reviews
                    .into_iter()
                    .find(|r| r["id"].as_u64() == id.parse().ok())
                    .context("unrecorded run");
            }
            if path.contains("/pulls/7/reviews?") {
                return Ok(self.data.get("reviews").cloned().unwrap_or(json!([])));
            }
            if path.contains("/workflows/dispatch.yml/runs?") {
                let mut runs = history("dispatches");
                if runs.is_empty() {
                    runs = vec![self.data["dispatch"].clone(), self.data["dispatch"].clone()];
                }
                runs.extend(history("dispatch_history"));
                return Ok(listed(runs, path));
            }
            if [20, 21, 22]
                .iter()
                .any(|id| path.contains(&format!("/runs/{id}/attempts/2/jobs?")))
            {
                return Ok(self.data["jobs"].clone());
            }
            bail!("unrecorded read {path}")
        }

        fn graphql(&self, query: &str, _: Value) -> Result<Value> {
            ensure!(query == OPEN_PULLS, "unrecorded query");
            let mut pulls = vec![self.data["pull"].clone()];
            pulls.extend(
                self.data["other_pulls"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            );
            Ok(
                json!({"repository":{"pullRequests":{"nodes":pulls,"pageInfo":{"hasNextPage":self.next_page}}}}),
            )
        }

        fn download(&self, path: &str, limit: u64) -> Result<Vec<u8>> {
            self.reads.borrow_mut().push(path.to_owned());
            let log = self.data["log"]
                .as_str()
                .context("recorded log")?
                .as_bytes()
                .to_vec();
            ensure!(log.len() as u64 <= limit, "download bound");
            Ok(log)
        }

        fn send(&self, _: &str, _: &str, _: &Value) -> Result<Value> {
            bail!("read-only")
        }
    }

    /// A run listing as GitHub filters and pages it; a run without a
    /// recorded creation time passes the `created` filter.
    fn listed(runs: Vec<Value>, path: &str) -> Value {
        let query: HashMap<String, String> =
            url::form_urlencoded::parse(path.split_once('?').unwrap().1.as_bytes())
                .into_owned()
                .collect();
        let field = |r: &Value, key: &str, field: &str| {
            query.get(key).is_none_or(|want| r[field] == want.as_str())
        };
        let since = query
            .get("created")
            .map(|c| millis(&json!(c.strip_prefix(">=").unwrap())).unwrap());
        let per: usize = query["per_page"].parse().unwrap();
        let page: usize = query.get("page").map_or(1, |p| p.parse().unwrap());
        let runs: Vec<Value> = runs
            .into_iter()
            .filter(|r| {
                field(r, "event", "event")
                    && field(r, "head_sha", "head_sha")
                    && (field(r, "status", "status") || field(r, "status", "conclusion"))
                    && since
                        .zip(millis(&r["created_at"]))
                        .is_none_or(|(since, at)| at >= since)
            })
            .skip((page - 1) * per)
            .take(per)
            .collect();
        json!({"workflow_runs": runs})
    }

    fn collector<'a>(api: &'a dyn Api, s: &'a Settings, now: i64) -> Collector<'a> {
        Collector {
            api,
            s,
            branch: "main".into(),
            now,
            root: "/repos/example/control".into(),
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// How many current heads are reported without a review.
    fn lost(api: &dyn Api, s: &Settings, now: i64) -> usize {
        collector(api, s, now).lost_reviews().unwrap().len()
    }

    fn configured() -> Settings {
        let mut s = settings();
        s.follow = vec!["example/project".into()];
        s.bot = "worker".into();
        s.review_workflow = Some("review.yml".into());
        s.dispatch_workflow = "dispatch.yml".into();
        s
    }

    fn epoch() -> i64 {
        millis(&json!("2026-11-03T00:00:00Z")).unwrap()
    }

    #[test]
    fn recorded_queue_residence_boundary_changed_state_and_matching_run() {
        let mut api = Recorded::new();
        let s = configured();
        for (age, problem) in [
            (QUEUE_RESIDENCE_MS - 1, false),
            (QUEUE_RESIDENCE_MS, true),
            (QUEUE_RESIDENCE_MS + 1, true),
        ] {
            let got = collector(&api, &s, epoch() + age).aged_queue().unwrap();
            assert_eq!(got.len() == 1, problem);
            if !problem {
                continue;
            }
            assert_eq!(got[0]["latest_merge_group_run"]["run_attempt"], 2);
            assert_eq!(got[0]["latest_merge_group_run"]["conclusion"], "failure");
            assert_eq!(got[0]["age_basis"], "queue residence (not unchanged state)");
        }
        api.data["pull"]["mergeQueueEntry"]["state"] = json!("MERGEABLE");
        let got = collector(&api, &s, epoch() + 2 * HOUR_MS)
            .aged_queue()
            .unwrap();
        assert_eq!(got[0]["state"], "MERGEABLE");
        assert_eq!(got[0]["age_basis"], "queue residence (not unchanged state)");
        api.data["pull"]["mergeQueueEntry"]["headCommit"]["oid"] = json!("new-head");
        assert!(
            collector(&api, &s, epoch() + 2 * HOUR_MS)
                .aged_queue()
                .unwrap()[0]["latest_merge_group_run"]
                .is_null()
        );
        api.data["pull"]["mergeQueueEntry"]["enqueuedAt"] = Value::Null;
        assert!(
            collector(&api, &s, epoch() + 2 * HOUR_MS)
                .aged_queue()
                .is_err()
        );
    }

    #[test]
    fn recorded_lost_review_head_workflow_pr_and_grace() {
        let mut api = Recorded::new();
        let s = configured();
        for (age, problem) in [
            (REVIEW_GRACE_MS - 1, false),
            (REVIEW_GRACE_MS, true),
            (REVIEW_GRACE_MS + 1, true),
        ] {
            assert_eq!(
                collector(&api, &s, epoch() + age)
                    .lost_reviews()
                    .unwrap()
                    .len()
                    == 1,
                problem
            );
        }
        for (status, conclusion, problem) in [
            ("in_progress", "", false),
            ("queued", "", false),
            ("completed", "success", false),
            ("completed", "cancelled", true),
            ("completed", "failure", true),
        ] {
            api.data["review"]["status"] = json!(status);
            api.data["review"]["conclusion"] = json!(conclusion);
            assert_eq!(
                collector(&api, &s, epoch() + HOUR_MS)
                    .lost_reviews()
                    .unwrap()
                    .len()
                    == 1,
                problem
            );
        }
        api.data["review"]["conclusion"] = json!("success");
        let original = api.data["review"].clone();
        for (path, value) in [
            ("/head_sha", json!("old-head")),
            ("/path", json!(".github/workflows/ci.yml")),
            ("/event", json!("push")),
            ("/pull_requests/0/number", json!(8)),
            ("/pull_requests/0/head/sha", json!("old-head")),
            ("/repository/full_name", json!("other/repo")),
        ] {
            *api.data["review"].pointer_mut(path).unwrap() = value;
            assert_eq!(lost(&api, &s, epoch() + HOUR_MS), 1, "{path}");
            api.data["review"] = original.clone();
        }
        api.data["pull"]["headRefOid"] = json!("new-head");
        assert_eq!(lost(&api, &s, epoch() + HOUR_MS), 1);
        api.data["pull"]["isDraft"] = json!(true);
        assert!(
            collector(&api, &s, epoch() + HOUR_MS)
                .lost_reviews()
                .unwrap()
                .is_empty()
        );
        api.data["pull"]["isDraft"] = json!(false);
        api.data["pull"]["updatedAt"] = Value::Null;
        assert!(
            collector(&api, &s, epoch() + HOUR_MS)
                .lost_reviews()
                .is_err()
        );
    }

    #[test]
    fn recorded_apply_attempt_deduplicated_and_reuses_error_signature() {
        let mut api = Recorded::new();
        // A run from days ago rerun today must not be lost to the lookback,
        // nor to a lifetime of successful runs; a rerun of a month-old run
        // is outside the read failed-run window.
        let s = configured();
        api.data["dispatch"]["created_at"] = json!("2026-10-01T00:00:00Z");
        assert!(
            collector(&api, &s, epoch() + HOUR_MS)
                .apply_failures()
                .unwrap()
                .is_empty()
        );
        api.data["dispatch"]["created_at"] = json!("2026-10-30T00:00:00Z");
        api.data["dispatch_history"] = json!(
            (0..2 * s.limits.max_pages as u64 * s.limits.page_size)
                .map(
                    |n| json!({"id": 1000 + n, "status": "completed", "conclusion": "success",
                    "created_at": "2026-10-31T00:00:00Z"})
                )
                .collect::<Vec<_>>()
        );
        let c = collector(&api, &s, epoch() + HOUR_MS);
        let apply = c.apply_failures().unwrap();
        assert_eq!(apply.len(), 1);
        assert_eq!(apply[0]["count"], 1);
        assert_eq!(apply[0]["text"], "##[error]E# refused item #");
        assert_eq!(c.failure_signatures().unwrap(), apply);
        assert_eq!(
            api.reads
                .borrow()
                .iter()
                .filter(|p| p.ends_with("/logs"))
                .count(),
            1
        );
        drop(c);
        api.data["jobs"]["jobs"][0]["conclusion"] = json!("cancelled");
        assert!(
            collector(&api, &s, epoch() + HOUR_MS)
                .apply_failures()
                .unwrap()
                .is_empty()
        );
        api.data["jobs"]["jobs"][0]["conclusion"] = json!("success");
        api.data["jobs"]["jobs"][1]["completed_at"] = Value::Null;
        assert!(
            collector(&api, &s, epoch() + HOUR_MS)
                .apply_failures()
                .is_err()
        );
        api.data["log"] = json!("2026-11-03T00:00:00Z no error recorded");
        assert!(
            collector(&api, &s, epoch() + HOUR_MS)
                .apply_failures()
                .is_err()
        );
    }

    #[test]
    fn fork_review_empty_associations_and_supported_dispatches() {
        let mut api = Recorded::new();
        let s = configured();
        // Real GitHub fork pull_request_target runs have this shape: the run
        // listing has no PR association, even for a successful review; the
        // head SHA it was listed by associates it, active or successful.
        api.data["review"]["pull_requests"] = json!([]);
        for (status, conclusion) in [("in_progress", ""), ("completed", "success")] {
            api.data["review"]["status"] = json!(status);
            api.data["review"]["conclusion"] = json!(conclusion);
            assert_eq!(lost(&api, &s, epoch() + HOUR_MS), 0);
        }
        // A review citing it cannot make a cancelled latest attempt count.
        api.data["review"]["conclusion"] = json!("cancelled");
        api.data["review"]["run_started_at"] = json!("2026-11-03T00:00:00Z");
        api.data["review"]["updated_at"] = json!("2026-11-03T00:30:00Z");
        api.data["reviews"] = json!([{"user":{"login":"worker"},"commit_id":"current-head", "submitted_at":"2026-11-03T00:20:00Z",
            "body":"<!-- gh-aw-agentic-workflow: review, id: 10, run: https://github.com/example/project/actions/runs/10 -->"}]);
        assert_eq!(lost(&api, &s, epoch() + HOUR_MS), 1);
        api.data["review"]["event"] = json!("workflow_dispatch");
        api.data["review"]["head_sha"] = json!("default-branch-sha");
        api.data["review"]["head_branch"] = json!("main");
        api.data["review"]["display_title"] = json!("review example/project#7 current-head (pool)");
        for (status, conclusion) in [("in_progress", ""), ("completed", "success")] {
            api.data["review"]["status"] = json!(status);
            api.data["review"]["conclusion"] = json!(conclusion);
            assert_eq!(lost(&api, &s, epoch() + HOUR_MS), 0);
        }
        for (key, value) in [
            ("display_title", "review example/project#7 old-head (pool)"),
            (
                "display_title",
                "review example/project#8 current-head (pool)",
            ),
            ("head_branch", "other"),
        ] {
            let original = api.data["review"][key].clone();
            api.data["review"][key] = json!(value);
            assert_eq!(lost(&api, &s, epoch() + HOUR_MS), 1);
            api.data["review"][key] = original;
        }
    }

    #[test]
    fn recorded_review_history_volume_and_old_cited_dispatch() {
        let mut api = Recorded::new();
        let s = configured();
        let now = epoch() + HOUR_MS;
        let run = |n: u64, event: &str, head: &str, created: &str| {
            json!({"id": 100 + n, "event": event, "head_sha": head, "created_at": created,
                "path": ".github/workflows/review.yml", "repository": {"full_name": "example/project"},
                "head_branch": "main", "display_title": format!("review example/project#{} other (pool)", 100 + n),
                "status": "completed", "conclusion": "success"})
        };
        // Pages of other heads' runs and recent dispatches for other PRs.
        let page = s.limits.page_size;
        let mut history: Vec<Value> = (0..3 * page)
            .map(|n| {
                run(
                    n,
                    "pull_request_target",
                    "other-head",
                    "2026-11-02T12:00:00Z",
                )
            })
            .collect();
        history.extend((0..2 * page).map(|n| {
            run(
                1000 + n,
                "workflow_dispatch",
                "main-sha",
                "2026-11-02T12:00:00Z",
            )
        }));
        api.data["review_history"] = json!(history);
        let got = collector(&api, &s, now).lost_reviews().unwrap();
        assert_eq!(got[0]["examples"], json!([api.data["review"]["html_url"]]));
        // A successful dispatch older than the lookback counts only through a
        // current-head review citing it.
        let dispatched = &mut api.data["review"];
        dispatched["event"] = json!("workflow_dispatch");
        dispatched["head_sha"] = json!("main-sha");
        dispatched["head_branch"] = json!("main");
        dispatched["created_at"] = json!("2026-10-31T00:00:00Z");
        dispatched["conclusion"] = json!("success");
        dispatched["display_title"] = json!("review example/project#7 current-head");
        dispatched["run_started_at"] = json!("2026-10-31T00:00:00Z");
        dispatched["updated_at"] = json!("2026-10-31T00:30:00Z");
        assert_eq!(lost(&api, &s, now), 1);
        let body = "<!-- gh-aw-agentic-workflow: review, id: 10, run: https://github.com/example/project/actions/runs/10 -->";
        api.data["reviews"] = json!([{"user": {"login": "github-actions[bot]", "type": "Bot", "id": 41898282},
                "commit_id": "current-head", "submitted_at": "2026-10-31T00:20:00Z", "body": body}]);
        assert_eq!(lost(&api, &s, now), 0);
        assert!(
            api.reads
                .borrow()
                .iter()
                .any(|p| p == "/repos/example/project/actions/runs/10")
        );
        for (key, value) in [
            ("/reviews/0/commit_id", json!("old-head")),
            ("/reviews/0/user/id", json!(1)),
            ("/reviews/0/user/type", json!("User")),
            // A later attempt, rerun after the review was posted.
            ("/review/run_started_at", json!("2026-11-01T00:00:00Z")),
            ("/review/conclusion", json!("cancelled")),
            (
                "/review/display_title",
                json!("review example/project#8 current-head"),
            ),
        ] {
            let original = api.data.pointer(key).unwrap().clone();
            *api.data.pointer_mut(key).unwrap() = value;
            assert_eq!(lost(&api, &s, now), 1, "{key}");
            *api.data.pointer_mut(key).unwrap() = original;
        }
        // Many healthy queued and reviewed PRs stay within the report bounds.
        let mut healthy = api.data["pull"].clone();
        healthy["mergeQueueEntry"]["enqueuedAt"] = json!(iso(now - 1));
        api.data["other_pulls"] = json!(vec![healthy; 2 * s.limits.max_items]);
        let c = collector(&api, &s, now);
        assert!(c.lost_reviews().unwrap().is_empty());
        assert_eq!(c.aged_queue().unwrap().len(), 1);
        api.data["other_pulls"] = json!([]);
        // More dispatches in the lookback than maxPages hold is unavailable.
        api.data["review_history"] = json!(
            (0..s.limits.max_pages as u64 * page)
                .map(|n| run(n, "workflow_dispatch", "main-sha", "2026-11-02T12:00:00Z"))
                .collect::<Vec<_>>()
        );
        api.data["reviews"] = json!([]);
        assert!(collector(&api, &s, now).lost_reviews().is_err());
    }

    #[test]
    fn recorded_stall_collection_uses_completion_not_run_update() {
        let mut api = Recorded::new();
        api.data["jobs"]["jobs"][0]["conclusion"] = json!("failure");
        api.data["jobs"]["jobs"][1]["conclusion"] = json!("skipped");
        api.data["log"] =
            json!("2026-11-03T00:00:00Z ##[error]the agent stalled after its reminder");
        let s = configured();
        assert!(
            collector(&api, &s, epoch() + HOUR_MS)
                .stall_groups()
                .unwrap()
                .is_empty()
        );
        let mut runs = vec![];
        for n in 0..3 {
            let mut run = api.data["dispatch"].clone();
            run["id"] = json!(20 + n);
            run["html_url"] = json!(format!(
                "https://github.com/example/control/actions/runs/{}",
                20 + n
            ));
            run["display_title"] = json!("implement example/project#7 (observed-pool)");
            runs.push(run);
        }
        api.data["dispatches"] = json!(runs);
        let got = collector(&api, &s, epoch() + HOUR_MS)
            .stall_groups()
            .unwrap();
        assert_eq!(got[0]["count"], 3);
        assert_eq!(got[0]["first_seen"], "2026-11-03T00:20:00.000Z");
        assert_eq!(got[0]["observed_pools"], json!(["observed-pool"]));
        for (offset, count) in [(-1, 0), (0, 1), (1, 1)] {
            api.data["jobs"]["jobs"][0]["completed_at"] =
                json!(iso(epoch() + HOUR_MS - 24 * HOUR_MS + offset));
            assert_eq!(
                collector(&api, &s, epoch() + HOUR_MS)
                    .stall_groups()
                    .unwrap()
                    .len(),
                count
            );
        }
        api.data["log"] = json!(
            "2026-11-03T00:00:00Z task: fix stall detection\n2026-11-03T00:00:01Z ##[error]unrelated failure"
        );
        assert!(
            collector(&api, &s, epoch() + HOUR_MS)
                .stall_groups()
                .unwrap()
                .is_empty()
        );
        api.data["log"] =
            json!("2026-11-03T00:00:00Z ##[error]the agent stalled after its reminder");
        api.data["jobs"]["jobs"][0]["completed_at"] = Value::Null;
        assert!(
            collector(&api, &s, epoch() + HOUR_MS)
                .stall_groups()
                .is_err()
        );
    }

    #[test]
    fn recorded_incomplete_and_budget_exhaustion_never_healthy() {
        let mut api = Recorded::new();
        let mut s = configured();
        api.next_page = true;
        assert!(collector(&api, &s, epoch() + HOUR_MS).aged_queue().is_err());
        assert!(
            collector(&api, &s, epoch() + HOUR_MS)
                .lost_reviews()
                .is_err()
        );
        api.next_page = false;
        s.limits.page_size = 1;
        assert!(
            collector(&api, &s, epoch() + HOUR_MS)
                .lost_reviews()
                .is_err()
        );
        s.limits.max_pages = 1;
        assert!(
            collector(&api, &s, epoch() + HOUR_MS)
                .apply_failures()
                .is_err()
        );
        s.limits = Limits::default();
        api.data["jobs"]["total_count"] = json!(101);
        assert!(
            collector(&api, &s, epoch() + HOUR_MS)
                .apply_failures()
                .is_err()
        );
        s.limits.max_requests = 1;
        let clock = || epoch() + HOUR_MS;
        let api = Bounded::new(&api, &s.limits, &clock);
        let snap = collect(&api, &s, "main", clock(), None);
        for id in ["queue-aged", "reviews", "apply-failures", "stalls"] {
            let report = evaluate(&snap, clock(), &s.limits)
                .into_iter()
                .find(|c| c["id"] == id)
                .unwrap();
            assert_ne!(report["status"], "ok");
        }
    }

    #[test]
    fn recorded_complete_collection_reports_failures_and_healthy_cases() {
        let mut api = Recorded::new();
        let s = configured();
        let now = epoch() + HOUR_MS;
        let snap = collect(&api, &s, "main", now, None);
        let report = evaluate(&snap, now, &s.limits);
        for (id, want) in [
            ("queue-aged", "fail"),
            ("reviews", "fail"),
            ("apply-failures", "fail"),
            ("stalls", "ok"),
        ] {
            let check = report.iter().find(|c| c["id"] == id).unwrap();
            assert_eq!(check["status"], want, "{id}");
            if want == "fail" {
                assert!(!check["evidence"].as_array().unwrap().is_empty());
            }
        }
        assert_eq!(
            api.reads
                .borrow()
                .iter()
                .filter(|p| p.ends_with("/logs"))
                .count(),
            1
        );
        api.data["pull"]["mergeQueueEntry"]["enqueuedAt"] = json!(iso(now - 1));
        api.data["review"]["conclusion"] = json!("success");
        api.data["jobs"]["jobs"][1]["conclusion"] = json!("success");
        let snap = collect(&api, &s, "main", now, None);
        let report = evaluate(&snap, now, &s.limits);
        for id in ["queue-aged", "reviews", "apply-failures", "stalls"] {
            assert_eq!(
                report.iter().find(|c| c["id"] == id).unwrap()["status"],
                "ok",
                "{id}"
            );
        }
        api.data["pull"]["updatedAt"] = Value::Null;
        let report = evaluate(&collect(&api, &s, "main", now, None), now, &s.limits);
        assert_eq!(
            report.iter().find(|c| c["id"] == "reviews").unwrap()["status"],
            "warn"
        );
        assert!(bounded(vec![json!({}), json!({})], 1).is_err());
        assert!(bounded(vec![json!({"text":"x".repeat(REPORT_BYTES)})], 1).is_err());
        let mut s = s;
        s.limits.max_download_bytes = 1;
        api.data["jobs"]["jobs"][1]["conclusion"] = json!("failure");
        assert!(collector(&api, &s, now).apply_failures().is_err());
    }
}
