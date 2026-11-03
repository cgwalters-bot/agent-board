//! Recurring failures, grouped over a day. Text here is untrusted evidence.
use super::*;
use sha2::{Digest, Sha256};

const DAY: i64 = 24 * HOUR_MS;
const TEXT_LIMIT: usize = 240;
const EXAMPLES: usize = 3;
const REPORT_BYTES: usize = 4 * 1024;

/// Strip volatile numbers and abbreviated/full SHAs before grouping. Keep
/// punctuation and words so distinct failures do not collapse to a job name.
pub(crate) fn normalize(text: &str) -> String {
    let clean = strip_colours(text);
    let mut out = String::new();
    for word in clean.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        let mut token = String::new();
        let flush = |token: &mut String, out: &mut String| {
            if token.len() >= 7 && token.bytes().all(|b| b.is_ascii_hexdigit()) {
                out.push_str("<sha>");
            } else {
                let mut number = false;
                for c in token.chars() {
                    if c.is_ascii_digit() {
                        if !number {
                            out.push('#');
                        }
                        number = true;
                    } else {
                        out.push(c);
                        number = false;
                    }
                }
            }
            token.clear();
        };
        for c in word.chars() {
            if c.is_ascii_alphanumeric() {
                token.push(c);
            } else {
                flush(&mut token, &mut out);
                if !c.is_control() {
                    out.push(c);
                }
            }
        }
        flush(&mut token, &mut out);
    }
    out
}

fn grouped(events: Vec<Value>, max: usize) -> Result<Vec<Value>> {
    let mut groups = BTreeMap::<String, Value>::new();
    let mut seen = BTreeSet::new();
    for event in events {
        let kind = event["kind"].as_str().context("signature kind")?;
        let text = normalize(event["text"].as_str().context("signature text")?);
        ensure!(!text.is_empty(), "empty signature");
        // A scheduling observation must never suppress or inflate recurrence
        // of the same reason on a stranded pull request.
        let problem = event["problem"] != false;
        let signature = format!("{:x}", Sha256::digest(format!("{kind}\n{problem}\n{text}")));
        let url = event["url"].as_str().context("signature example")?;
        let at = millis(&event["at"]).context("signature time")?;
        let first = millis(&event["first_at"]).unwrap_or(at).min(at);
        // Repeated snapshots/log lines must not inflate one item into a class.
        let identity = event["identity"].as_str().unwrap_or(url);
        if !seen.insert((signature.clone(), identity.to_owned())) {
            continue;
        }
        let group = groups.entry(signature.clone()).or_insert_with(|| json!({
            "signature": signature, "kind": kind, "text": text.chars().take(TEXT_LIMIT).collect::<String>(),
            "count": 0, "first_seen": iso(first), "last_seen": iso(at),
            "examples": [], "problem": problem
        }));
        group["count"] =
            json!(group["count"].as_u64().unwrap_or(0) + event["count"].as_u64().unwrap_or(1));
        group["first_seen"] = json!(iso(millis(&group["first_seen"])
            .unwrap_or(first)
            .min(first)));
        group["last_seen"] = json!(iso(millis(&group["last_seen"]).unwrap_or(at).max(at)));
        if let Some(examples) = group["examples"].as_array_mut()
            && examples.len() < EXAMPLES
            && !examples.contains(&json!(url))
        {
            examples.push(json!(url));
        }
    }
    ensure!(groups.len() <= max, "signature report truncated");
    let groups: Vec<Value> = groups.into_values().collect();
    ensure!(
        serde_json::to_vec(&groups)?.len() <= REPORT_BYTES,
        "signature report exceeds byte bound"
    );
    Ok(groups)
}

impl Collector<'_> {
    pub(super) fn failure_signatures(&self) -> Result<Vec<Value>> {
        let runs: Vec<Value> = self
            .dispatch_runs()?
            .into_iter()
            .filter(|r| {
                millis(&r["updated_at"]).is_some_and(|at| at >= self.now - DAY && at <= self.now)
                    && FAILED.contains(&r["conclusion"].as_str().unwrap_or_default())
            })
            .collect();
        let mut events = vec![];
        for run in runs {
            let id = run["id"].as_u64().context("run id")?;
            let jobs = self.cached_get(&format!(
                "{}/actions/runs/{id}/jobs?per_page=100",
                self.root
            ))?;
            let nodes = jobs["jobs"].as_array().context("failed jobs")?;
            ensure!(
                jobs["total_count"].as_u64().is_some_and(|n| n <= 100),
                "jobs truncated"
            );
            for job in nodes
                .iter()
                .filter(|j| FAILED.contains(&j["conclusion"].as_str().unwrap_or_default()))
            {
                let id = job["id"].as_u64().context("job id")?;
                let log = self.api.download(
                    &format!("{}/actions/jobs/{id}/logs", self.root),
                    self.s.limits.max_download_bytes,
                )?;
                let text = strip_colours(&String::from_utf8_lossy(&log));
                let error = text
                    .lines()
                    .filter_map(log_line)
                    .find(|l| {
                        let lower = l.to_ascii_lowercase();
                        lower.contains("##[error]")
                            || lower.starts_with("error:")
                            || lower.starts_with("error[")
                    })
                    .context("failed job has no error line")?;
                events.push(json!({"kind": "step", "text": error, "url": run["html_url"], "at": run["updated_at"]}));
            }
        }
        grouped(events, self.s.limits.max_items)
    }

    fn holds_snapshot(&self) -> Result<(Value, String)> {
        use crate::pass::holds::{HOLDS_MARKER, HOLDS_SCHEMA};
        let usage = self.s.usage.as_ref().context("no holds location")?;
        let comments = self.comments(&usage.issue.repository, usage.issue.issue)?;
        let comment = own_comments(&comments, &usage.bot)
            .into_iter()
            .filter(|c| {
                c["body"]
                    .as_str()
                    .is_some_and(|b| b.starts_with(HOLDS_MARKER))
            })
            .max_by_key(|c| millis(&c["updatedAt"]))
            .context("no holds snapshot")?;
        let snap =
            json_block(comment["body"].as_str().context("holds body")?).context("holds JSON")?;
        let at = millis(&snap["at"]).context("holds time")?;
        ensure!(
            snap["schema"] == HOLDS_SCHEMA
                && snap["truncated"].as_u64() == Some(0)
                && at <= self.now
                && at >= self.now - DAY,
            "incomplete or stale holds"
        );
        Ok((
            snap,
            comment["url"].as_str().context("holds URL")?.to_owned(),
        ))
    }

    pub(super) fn hold_signatures(&self, quality: bool) -> Result<Vec<Value>> {
        let (snap, url) = self.holds_snapshot()?;
        let mut events = vec![];
        if quality {
            let list = &snap["needs_you"];
            let asks = list["items"]
                .as_array()
                .context("needs-you not published")?;
            for ask in asks {
                let question = ask["ask"].as_str().context("ask text")?;
                // This is a quality signal, not a semantic verdict: label-only
                // placeholders do not contain a concrete question.
                let real = question.contains('?')
                    && !question
                        .to_ascii_lowercase()
                        .contains("labelled needs-human");
                events.push(json!({"kind": "needs-you", "text": if real {"question present"} else {"no concrete question"}, "url": ask["target"], "identity": ask["key"], "at": snap["at"], "problem": !real}));
            }
            let omitted = list["omitted"].as_u64().context("omitted count")?;
            if omitted > 0 {
                events.push(json!({"kind": "needs-you", "text": "omitted asks", "count": omitted, "url": url, "at": snap["at"]}));
            }
            if list["complete"] != true {
                events.push(json!({"kind": "needs-you", "text": "incomplete sources", "url": url, "at": snap["at"]}));
            }
        } else {
            let pulls = self.open_pulls()?;
            let pull_urls: BTreeSet<String> = pulls
                .iter()
                .filter_map(|p| p["url"].as_str())
                .map(|u| u.replace("/pull/", "/issues/"))
                .collect();
            for pull in &pulls {
                let comments: Vec<Value> = pull["comments"]["nodes"].as_array().into_iter().flatten().map(|c| json!({"id":c["databaseId"], "body":c["body"], "user":c["author"], "created_at":c["createdAt"], "updated_at":c["updatedAt"]})).collect();
                let marker = crate::poll::find_marker(&comments, &self.s.bot);
                if let Some(comment) = comments.iter().find(|c| {
                    marker.stopped && marker.id.is_some() && c["id"].as_u64() == marker.id
                }) {
                    let at = millis(&comment["updated_at"]).context("poll stop time")?;
                    if at <= self.now && at >= self.now - DAY {
                        let reason = comment["body"]
                            .as_str()
                            .and_then(|b| b.lines().nth(1))
                            .and_then(|b| {
                                b.strip_prefix("The poll stopped following this pull request: ")
                            })
                            .and_then(|b| b.split_once(". It is labelled"))
                            .map(|(r, _)| r)
                            .context("poll stop reason")?;
                        events.push(json!({"kind": "poll-stop", "text": reason, "url": pull["url"], "at": comment["updated_at"]}));
                    }
                }
            }
            for hold in snap["holds"].as_array().context("holds")? {
                use crate::pass::holds::HoldKind;
                let kind: HoldKind =
                    serde_json::from_value(hold["kind"].clone()).context("hold kind")?;
                let reason = hold["reason"].as_str().context("hold reason")?;
                let target = hold["url"].as_str().unwrap_or(&url);
                // Normal scheduling waits are observations, not bugs. Reuse
                // the pass's explicit alarms rather than inventing timeouts.
                let problem = matches!(
                    kind,
                    HoldKind::Stalled
                        | HoldKind::Blocking
                        | HoldKind::Unreadable
                        | HoldKind::NeedsOperator
                ) || (kind == HoldKind::NeedsHuman && pull_urls.contains(target));
                events.push(json!({"kind": format!("hold/{}", kind.name()), "text": reason, "url": target, "at": snap["at"], "first_at": hold["since"], "problem": problem}));
                if hold["kind"] == "needs-human" {
                    let origin = if pull_urls.contains(target) {
                        "stranded pull request"
                    } else if hold["question"].is_string() {
                        "operator question"
                    } else {
                        "issue hold (no question recorded)"
                    };
                    events.push(json!({"kind": "needs-human-origin", "text": origin, "url": target, "at": snap["at"], "first_at": hold["since"], "problem": false}));
                }
            }
        }
        grouped(events, self.s.limits.max_items)
    }

    pub(super) fn main_signatures(&self) -> Result<Vec<Value>> {
        let mut events = vec![];
        for repo in &self.s.follow {
            let (owner, name) = repo.split_once('/').context("repository")?;
            let data = self.query(MAIN, json!({"owner": owner, "name": name}))?;
            let commit = &data["repository"]["defaultBranchRef"]["target"];
            ensure!(commit.is_object(), "default branch unavailable");
            let state = commit["statusCheckRollup"]["state"].as_str();
            if matches!(state, Some("FAILURE" | "ERROR")) {
                events.push(json!({"kind": "main", "text": format!("{repo}: red default branch"), "url": commit["url"], "at": iso(self.now)}));
            }
        }
        grouped(events, self.s.limits.max_items)
    }
}

const MAIN: &str = "query($owner:String!,$name:String!){repository(owner:$owner,name:$name){defaultBranchRef{target{... on Commit{url statusCheckRollup{state}}}}}}";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_and_grouping() {
        for (a, b) in [
            (
                "Error: run 123 abcdef0123456789 refused",
                "Error: run 456 fedcba9876543210 refused",
            ),
            ("\x1b[31merror: item #12\x1b[0m", "error: item #99"),
        ] {
            assert_eq!(normalize(a), normalize(b));
        }
        let events = (0..5).map(|n| json!({"kind":"step", "text":format!("Error: run {n} refused"), "url":format!("https://example.com/{n}"), "at":iso(n * HOUR_MS)})).collect::<Vec<_>>();
        let groups = grouped(events.clone(), 1).unwrap();
        assert_eq!(groups[0]["count"], 5);
        assert_eq!(groups[0]["examples"].as_array().unwrap().len(), 3);
        assert_eq!(groups[0]["first_seen"], iso(0));
        assert_eq!(groups[0]["last_seen"], iso(4 * HOUR_MS));
        assert!(grouped(events, 0).is_err());
        assert_ne!(
            normalize("Error: apply refused"),
            normalize("Error: check refused")
        );
    }

    #[test]
    fn healthy_and_problem_holds_are_separate_in_either_order() {
        let events = vec![
            json!({"kind":"hold/needs-human", "text":"operator needed", "url":"https://example.com/issues/1", "at":iso(0), "problem":false}),
            json!({"kind":"hold/needs-human", "text":"operator needed", "url":"https://example.com/pull/2", "at":iso(0), "problem":true}),
            json!({"kind":"hold/needs-human", "text":"operator needed", "url":"https://example.com/pull/3", "at":iso(HOUR_MS), "problem":true}),
        ];
        let forward = grouped(events.clone(), 2).unwrap();
        let reverse = grouped(events.into_iter().rev().collect(), 2).unwrap();
        for groups in [forward, reverse] {
            assert_eq!(groups.len(), 2);
            let failures = groups.iter().find(|g| g["problem"] == true).unwrap();
            let observations = groups.iter().find(|g| g["problem"] == false).unwrap();
            assert_eq!(failures["count"], 2);
            assert_eq!(observations["count"], 1);
            assert_ne!(failures["signature"], observations["signature"]);
        }
    }

    struct Forge {
        snapshot: Value,
        truncated_jobs: bool,
    }

    impl Api for Forge {
        fn get(&self, path: &str) -> Result<Value> {
            if path.contains("/runs?") {
                return Ok(json!({"workflow_runs":[
                    {"id":1,"conclusion":"failure","updated_at":iso(HOUR_MS),"html_url":"https://github.com/example/control/actions/runs/1"},
                    {"id":2,"conclusion":"failure","updated_at":iso(2 * HOUR_MS),"html_url":"https://github.com/example/control/actions/runs/2"}
                ]}));
            }
            if path.ends_with("/jobs?per_page=100") {
                return Ok(
                    json!({"total_count":if self.truncated_jobs {101} else {1}, "jobs":[{"id":3,"conclusion":"failure"}]}),
                );
            }
            bail!("unexpected read {path}")
        }

        fn graphql(&self, query: &str, _: Value) -> Result<Value> {
            if query == MAIN {
                return Ok(
                    json!({"repository":{"defaultBranchRef":{"target":{"url":"https://github.com/example/project/commit/abc", "statusCheckRollup":{"state":"FAILURE"}}}}}),
                );
            }
            if query == COMMENTS_QUERY {
                return Ok(
                    json!({"repository":{"issue":{"comments":{"pageInfo":{"hasNextPage":false},"nodes":[{
                        "author":{"login":"worker"},"url":"https://github.com/example/control/issues/9#issuecomment-1", "updatedAt":iso(2 * HOUR_MS),
                        "body":format!("{}\n```json\n{}\n```",crate::pass::holds::HOLDS_MARKER,self.snapshot)
                    }]}}}}),
                );
            }
            if query == OPEN_PULLS {
                return Ok(
                    json!({"repository":{"pullRequests":{"pageInfo":{"hasNextPage":false},"nodes":[{"url":"https://github.com/example/project/pull/3"}]}}}),
                );
            }
            bail!("unexpected query")
        }

        fn download(&self, _: &str, _: u64) -> Result<Vec<u8>> {
            Ok(b"2026-11-03T00:00:00Z setup\n2026-11-03T00:00:01Z ##[error]Unchecked replacement series 123 abcdef0123456789\n2026-11-03T00:00:02Z ##[error]Process completed with exit code 1".to_vec())
        }

        fn send(&self, _: &str, _: &str, _: &Value) -> Result<Value> {
            bail!("collector must not write")
        }
    }

    #[test]
    fn collector_groups_errors_holds_origins_quality_and_red_main() {
        let mut snapshot = serde_json::to_value(crate::pass::holds::snapshot(
            &[],
            iso(2 * HOUR_MS),
            (Default::default(), Default::default()),
        ))
        .unwrap();
        let fields = json!({
            "holds":[
                {"kind":"needs-human","reason":"a draft with no checkpoint","since":iso(0),"url":"https://github.com/example/project/issues/3"},
                {"kind":"needs-human","reason":"a draft with no checkpoint","since":iso(HOUR_MS),"url":"https://github.com/example/project/issues/4"}
            ],
            "needs_you":{"complete":false,"omitted":5,"items":[
                {"key":"one","target":"https://github.com/example/project/issues/3","ask":"labelled needs-human"},
                {"key":"two","target":"https://github.com/example/project/issues/4","ask":"Should we retry the release?"}
            ]}
        });
        for (key, value) in fields.as_object().unwrap() {
            snapshot[key] = value.clone();
        }
        let api = Forge {
            truncated_jobs: false,
            snapshot,
        };
        let mut s = crate::supervise::tests::settings();
        s.usage = Some(super::super::super::Usage {
            issue: super::super::super::IssueRef {
                repository: "example/control".into(),
                issue: 9,
            },
            bot: "worker".into(),
        });
        let c = Collector {
            api: &api,
            s: &s,
            branch: "main".into(),
            now: 3 * HOUR_MS,
            root: "/repos/example/control".into(),
            cache: RefCell::new(HashMap::new()),
        };
        let failures = c.failure_signatures().unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0]["count"], 2);
        assert_eq!(
            failures[0]["text"],
            "##[error]Unchecked replacement series # <sha>"
        );
        let holds = c.hold_signatures(false).unwrap();
        let draft = holds
            .iter()
            .find(|g| g["kind"] == "hold/needs-human" && g["problem"] == true)
            .unwrap();
        assert_eq!(draft["count"], 1);
        assert_eq!(draft["first_seen"], iso(0));
        assert!(holds.iter().any(|g| g["text"] == "stranded pull request"));
        let quality = c.hold_signatures(true).unwrap();
        assert!(
            quality
                .iter()
                .any(|g| g["text"] == "omitted asks" && g["count"] == 5)
        );
        assert!(
            quality
                .iter()
                .any(|g| g["text"] == "question present" && g["problem"] == false)
        );
        assert!(
            quality
                .iter()
                .any(|g| g["text"] == "no concrete question" && g["count"] == 1)
        );
        assert!(!c.main_signatures().unwrap().is_empty());
    }

    #[test]
    fn truncated_job_evidence_is_not_a_healthy_report() {
        let api = Forge {
            truncated_jobs: true,
            snapshot: Value::Null,
        };
        let s = crate::supervise::tests::settings();
        let c = Collector {
            api: &api,
            s: &s,
            branch: "main".into(),
            now: 3 * HOUR_MS,
            root: "/repos/example/control".into(),
            cache: RefCell::new(HashMap::new()),
        };
        assert!(c.failure_signatures().is_err());
        assert!(c.hold_signatures(true).is_err());
    }

    #[test]
    fn ordinary_holds_are_observations_not_failures() {
        let mut snapshot = serde_json::to_value(crate::pass::holds::snapshot(
            &[],
            iso(2 * HOUR_MS),
            (Default::default(), Default::default()),
        ))
        .unwrap();
        snapshot["holds"] = json!([
            {"kind":"in-flight","reason":"a run is in flight","url":"https://github.com/example/project/issues/3"},
            {"kind":"pr-open","reason":"its implementation pull request is open","url":"https://github.com/example/project/issues/4"}
        ]);
        let mut s = crate::supervise::tests::settings();
        s.usage = Some(super::super::super::Usage {
            issue: super::super::super::IssueRef {
                repository: "example/control".into(),
                issue: 9,
            },
            bot: "worker".into(),
        });
        for truncated in [0, 1] {
            snapshot["truncated"] = json!(truncated);
            let api = Forge {
                truncated_jobs: false,
                snapshot: snapshot.clone(),
            };
            let c = Collector {
                api: &api,
                s: &s,
                branch: "main".into(),
                now: 3 * HOUR_MS,
                root: "/repos/example/control".into(),
                cache: RefCell::new(HashMap::new()),
            };
            let result = c.hold_signatures(false);
            if truncated == 0 {
                let groups = result.unwrap();
                assert!(groups.iter().all(|g| g["problem"] == false));
                let snapshot = Snapshot {
                    observations: BTreeMap::from([("stranded".into(), Some(groups))]),
                    previous: None,
                };
                let report = evaluate(&snapshot, c.now, &s.limits);
                assert_eq!(
                    report.iter().find(|g| g["id"] == "stranded").unwrap()["status"],
                    "ok"
                );
            } else {
                assert!(result.is_err());
            }
        }
    }
}
