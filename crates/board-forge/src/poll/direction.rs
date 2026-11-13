//! Reporting receipts for intake, not authorization or dispatch deduplication.
//! A claim proves admission, never an answer. Pending requests survive search
//! cursor movement and disposable response-cache eviction. Receipts are only
//! refreshed when intake reads their thread anyway; they never schedule reads
//! or admit work themselves.

use super::comments::{Said, stamp};
use super::*;
use serde::{Deserialize, Serialize};

pub const REQUEST_LIMIT: usize = 100;
const MAX_RECEIPT_BYTES: usize = 2048;
/// Digits of the largest `u64` comment ID.
const MAX_ID_DIGITS: usize = 20;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Direction {
    /// Applying intake passes saved so far.
    pub passes: u64,
    /// Sticky: some receipt was dropped for room.
    pub overflow: bool,
    /// `OWNER/REPO#N/ANCHOR` to its receipt.
    pub requests: BTreeMap<String, Request>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub thread: String,
    pub comment: String,
    pub source: String,
    pub created_at: String,
    pub first_seen: String,
    pub first_pass: u64,
    pub checked_at: String,
    pub reply: Option<String>,
    pub dispatch: Option<String>,
}

impl Direction {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            self.requests.len() <= REQUEST_LIMIT,
            "operator direction exceeds {REQUEST_LIMIT} receipts"
        );
        for (key, request) in &self.requests {
            ensure!(
                serde_json::to_vec(request)?.len() <= MAX_RECEIPT_BYTES,
                "operator direction receipt exceeds {MAX_RECEIPT_BYTES} bytes"
            );
            ensure!(
                super::comments::refused_entry(key),
                "invalid operator direction key {key:?}"
            );
            ensure!(
                key == &format!("{}/{}", request.thread, request.comment),
                "operator direction identity mismatch"
            );
            ensure!(
                super::comments::ANCHORS.iter().any(|prefix| {
                    request.comment.strip_prefix(prefix).is_some_and(|id| {
                        id.len() <= MAX_ID_DIGITS && id.parse::<u64>().is_ok_and(|id| id > 0)
                    })
                }),
                "operator direction comment ID is invalid"
            );
            let (repo, number) = request
                .thread
                .split_once('#')
                .context("operator direction has no item")?;
            let number: u64 = number.parse().context("operator direction item number")?;
            let issue = format!(
                "https://github.com/{repo}/issues/{number}#{}",
                request.comment
            );
            let pull = format!(
                "https://github.com/{repo}/pull/{number}#{}",
                request.comment
            );
            ensure!(
                request.source == issue || request.source == pull,
                "operator direction source mismatch"
            );
            for at in [
                &request.created_at,
                &request.first_seen,
                &request.checked_at,
            ] {
                OffsetDateTime::parse(at, &Rfc3339).context("operator direction timestamp")?;
            }
            ensure!(
                request.first_pass <= self.passes,
                "operator direction is from a future pass"
            );
            if let Some(reply) = &request.reply {
                let prefix = request.source.split('#').next().unwrap_or("");
                ensure!(
                    reply
                        .strip_prefix(&format!("{prefix}#issuecomment-"))
                        .is_some_and(|id| id.parse::<u64>().is_ok_and(|id| id > 0)),
                    "operator direction reply identity mismatch"
                );
            }
            if let Some(dispatch) = &request.dispatch {
                ensure!(
                    valid_run(dispatch),
                    "operator direction dispatch URL is invalid"
                );
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn pending(&self) -> BTreeSet<&str> {
        (self.requests.values())
            .filter(|r| r.reply.is_none())
            .map(|r| r.thread.as_str())
            .collect()
    }

    pub(super) fn observe(
        &mut self,
        repo: &str,
        issue: &Value,
        said: &[Said],
        comments: &[Value],
        bot: &str,
        now: OffsetDateTime,
    ) -> Result<()> {
        let number = issue["number"]
            .as_u64()
            .context("direction has no item number")?;
        let thread = format!("{repo}#{number}");
        let path = if issue.get("pull_request").is_some_and(|p| !p.is_null()) {
            "pull"
        } else {
            "issues"
        };
        let base = format!("https://github.com/{repo}/{path}/{number}");
        let checked_at = stamp(now)?;
        let reply = super::notify::answered_by_hand(comments, bot);
        // Refresh existing receipts even when a burst of new comments cannot
        // fit. Otherwise that burst could prevent an old answer freeing space.
        // Refresh by retained identity, not today's admission classification:
        // editing/deleting a mention or lifting a stop must not strand receipts.
        for request in self.requests.values_mut().filter(|r| r.thread == thread) {
            request.checked_at.clone_from(&checked_at);
            let created_at = OffsetDateTime::parse(&request.created_at, &Rfc3339)
                .context("operator direction creation timestamp")?;
            if let Some((_, id)) =
                reply.filter(|r| super::notify::before((&request.comment, created_at), *r))
            {
                request.reply = Some(format!("{base}#issuecomment-{id}"));
            }
        }
        for s in said {
            let key = format!("{thread}/{}", s.key);
            let answer = reply.filter(|r| super::notify::before((&s.key, s.at), *r));
            if !self.requests.contains_key(&key) && self.requests.len() >= REQUEST_LIMIT {
                // Answered receipts go first, then the oldest unanswered, so a
                // full ledger keeps recording new questions rather than freezing.
                let oldest = (self.requests.iter())
                    .min_by_key(|(_, r)| (r.reply.is_none(), &r.first_seen, &r.source))
                    .map(|(key, r)| (key.clone(), r.reply.is_none()));
                if let Some((key, unanswered)) = oldest {
                    if unanswered && answer.is_some() {
                        // Never drop an open question to record a settled one.
                        continue;
                    }
                    self.requests.remove(&key);
                    self.overflow |= unanswered;
                }
            }
            let request = self
                .requests
                .entry(format!("{thread}/{}", s.key))
                .or_insert(Request {
                    thread: thread.clone(),
                    comment: s.key.clone(),
                    source: format!("{base}#{}", s.key),
                    created_at: stamp(s.at)?,
                    first_seen: checked_at.clone(),
                    first_pass: self.passes,
                    checked_at: checked_at.clone(),
                    reply: None,
                    dispatch: None,
                });
            request.checked_at.clone_from(&checked_at);
            if let Some((_, id)) = answer {
                request.reply = Some(format!("{base}#issuecomment-{id}"));
            }
        }
        Ok(())
    }

    pub(super) fn dispatched(&mut self, thread: &str, keys: &[String], line: &str) {
        let Some((_, url)) = line.rsplit_once(" -> ") else {
            return;
        };
        if !valid_run(url) {
            return;
        }
        for key in keys {
            if let Some(request) = self.requests.get_mut(&format!("{thread}/{key}")) {
                request.dispatch = Some(url.into());
            }
        }
    }

    /// Oldest unanswered requests first. This extension is deliberately not a
    /// needs-you ask: the bot owes a reply, not another operator decision.
    pub fn snapshot(&self) -> Value {
        let mut requests: Vec<_> = self.requests.values().collect();
        requests.sort_by_key(|r| (r.reply.is_some(), &r.first_seen, &r.source));
        json!({"schema": "agent-board/operator-direction/v1", "passes": self.passes, "overflow": self.overflow,
            "complete": false, "requests": requests.iter().map(|r| {
                let mut value = serde_json::to_value(r).unwrap_or_default();
                value["unanswered_passes"] = json!(self.passes.saturating_sub(r.first_pass));
                value["failure"] = json!(r.reply.is_none() && self.passes > r.first_pass);
                value
            }).collect::<Vec<_>>()})
    }
}

fn valid_run(url: &str) -> bool {
    url.strip_prefix("https://github.com/")
        .and_then(|path| path.split_once("/actions/runs/"))
        .is_some_and(|(repo, id)| {
            validate_repository(repo).is_ok() && id.parse::<u64>().is_ok_and(|id| id > 0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_receipts_are_not_answers_and_pending_is_bounded() {
        let now = OffsetDateTime::parse("2026-11-13T12:00:00Z", &Rfc3339).unwrap();
        let issue = json!({"number": 54, "pull_request": {}});
        let said = Said {
            key: "issuecomment-1".into(),
            at: now,
            body: "Why?".into(),
            changes_requested: false,
        };
        let mut ledger = Direction::default();
        for prefix in super::super::comments::ANCHORS {
            let mut anchored = said.clone();
            anchored.key = format!("{prefix}1");
            let mut observed = Direction::default();
            observed
                .observe("our/app", &issue, &[anchored], &[], "bot", now)
                .unwrap();
            let state = State {
                interval_seconds: MIN_INTERVAL_SECONDS,
                operator_direction: observed,
                ..Default::default()
            };
            assert_eq!(
                State::from_json(&state.to_json())
                    .unwrap()
                    .operator_direction,
                state.operator_direction
            );
        }
        ledger
            .observe(
                "our/app",
                &issue,
                std::slice::from_ref(&said),
                &[],
                "bot",
                now,
            )
            .unwrap();
        ledger.dispatched(
            "our/app#54",
            std::slice::from_ref(&said.key),
            "triage -> https://github.com/our/tracker/actions/runs/3",
        );
        assert_eq!(ledger.snapshot()["requests"][0]["failure"], false);
        ledger.passes += 1;
        for (author, body, answered) in [
            ("bot", "<!-- agent-board-claim: {} --> started", false),
            ("bot", "<!-- agentic-job-status --> Running", false),
            ("other", "Here is the answer", false),
            ("bot", " ", false),
            (
                "bot",
                "The commit used the wrong author; here is the correction.",
                true,
            ),
        ] {
            let mut observed = ledger.clone();
            let comments = vec![
                json!({"id": 2, "user": {"login": author}, "body": body, "created_at": "2026-11-13T12:00:01Z"}),
            ];
            observed
                .observe(
                    "our/app",
                    &issue,
                    std::slice::from_ref(&said),
                    &comments,
                    "bot",
                    now,
                )
                .unwrap();
            observed.validate().unwrap();
            assert_eq!(
                observed.snapshot()["requests"][0]["failure"],
                !answered,
                "{body}"
            );
            assert_eq!(observed.pending().is_empty(), answered);
            let state = State {
                interval_seconds: MIN_INTERVAL_SECONDS,
                operator_direction: observed.clone(),
                ..Default::default()
            };
            let mut decoded = State::from_json(&state.to_json()).unwrap();
            decoded.bind_responses("new-scope");
            assert_eq!(decoded.operator_direction, observed);
        }
        for id in 2..=REQUEST_LIMIT {
            let s = Said {
                key: format!("issuecomment-{id}"),
                ..said.clone()
            };
            ledger
                .observe("our/app", &issue, &[s], &[], "bot", now)
                .unwrap();
        }
        let full = ledger.clone();
        let overflow = Said {
            key: "issuecomment-101".into(),
            ..said.clone()
        };
        ledger
            .observe("our/app", &issue, &[overflow], &[], "bot", now)
            .unwrap();
        assert!(!full.overflow);
        assert_eq!(ledger.requests.len(), REQUEST_LIMIT);
        assert!(!ledger.requests.contains_key("our/app#54/issuecomment-1"));
        assert!(ledger.requests.contains_key("our/app#54/issuecomment-101"));
        assert!(ledger.overflow);
        let full = ledger.clone();
        let settled = Said {
            key: "issuecomment-102".into(),
            ..said
        };
        let answer = [
            json!({"id": 103, "user": {"login": "bot"}, "body": "Done.", "created_at": "2026-11-13T12:00:01Z"}),
        ];
        ledger
            .observe("other/app", &issue, &[settled], &answer, "bot", now)
            .unwrap();
        assert_eq!(ledger.requests, full.requests);
        let mut snapshot =
            crate::pass::holds::snapshot(&[], stamp(now).unwrap(), Default::default());
        snapshot.operator_direction = Some(ledger.snapshot());
        let (fitted, body) =
            crate::pass::holds::fit_to(snapshot, crate::pass::holds::Bound::Parts).unwrap();
        let published = crate::pass::holds::published(&body).unwrap();
        assert_eq!(
            published["operator_direction"]["requests"]
                .as_array()
                .unwrap()
                .len(),
            REQUEST_LIMIT
        );
        assert_eq!(fitted.operator_direction, Some(ledger.snapshot()));
        assert!(!crate::pass::holds::changed(Some(&body), &fitted).unwrap());
        for malformed in [json!(false), json!("unexpected"), json!([]), Value::Null] {
            let mut previous = published.clone();
            previous["operator_direction"] = malformed;
            let previous = format!(
                "{}\n```json\n{}\n```",
                crate::pass::holds::HOLDS_MARKER,
                previous
            );
            assert!(crate::pass::holds::changed(Some(&previous), &fitted).unwrap());
        }
        let mut invalid = serde_json::to_value(&ledger).unwrap();
        invalid["requests"]["our/app#54/issuecomment-2"]["source"] =
            json!("https://github.com/other/app/issues/54#issuecomment-2");
        assert!(
            serde_json::from_value::<Direction>(invalid)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut oversized = ledger.clone();
        let mut receipt = oversized.requests.values().next().unwrap().clone();
        receipt.comment = format!("issuecomment-{}1", "0".repeat(2_000_000));
        receipt.source = format!("https://github.com/our/app/pull/54#{}", receipt.comment);
        oversized.requests =
            BTreeMap::from([(format!("{}/{}", receipt.thread, receipt.comment), receipt)]);
        assert!(oversized.validate().is_err());
    }
}
