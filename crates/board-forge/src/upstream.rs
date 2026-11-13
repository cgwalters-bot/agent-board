//! A fork issue's optional upstream association. Thread text supplies only a
//! number: GitHub's fork parent, never that text, supplies the repository.
use crate::github::API_ROOT;
use crate::poll::{Rest, validate_repository};
use anyhow::{Context, Result, ensure};
use board_core::ItemRef;
use serde_json::Value;
use std::collections::BTreeMap;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const PREFIX: &str = "Upstream:";

/// Exactly one whole line `Upstream: #N`, or none. Refuse qualified links too:
/// allowing a body to choose the repository could close an unrelated issue.
pub fn parse(body: &str) -> Result<Option<u64>> {
    let mut found = None;
    for line in body.lines().map(str::trim) {
        let Some(value) = line.strip_prefix(PREFIX) else {
            continue;
        };
        ensure!(
            found.is_none(),
            "upstream link ambiguous: multiple Upstream: lines"
        );
        let digits = value.trim().strip_prefix('#').unwrap_or_default();
        ensure!(
            !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
            "upstream link malformed: expected Upstream: #N"
        );
        let number = digits
            .parse::<u64>()
            .context("upstream link number overflow")?;
        ensure!(number > 0, "upstream link number must be positive");
        found = Some(number);
    }
    Ok(found)
}

fn get(client: &impl Rest, path: &str) -> Result<Value> {
    let response = client.get(&format!("{API_ROOT}{path}"), None)?;
    ensure!(
        response.status == 200,
        "upstream read {path}: HTTP {}",
        response.status
    );
    Ok(response.body)
}

pub(crate) fn issue(client: &impl Rest, repo: &str, number: u64) -> Result<Value> {
    validate_repository(repo)?;
    ensure!(number > 0, "upstream issue number must be positive");
    let issue = get(client, &format!("repos/{repo}/issues/{number}"))?;
    ensure!(
        issue["number"].as_u64() == Some(number)
            && issue["repository_url"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&format!("{API_ROOT}repos/{repo}")))
            && issue.get("pull_request").is_none_or(Value::is_null),
        "upstream issue identity mismatch (or pull request): {repo}#{number}"
    );
    Ok(issue)
}

pub(crate) fn parent(client: &impl Rest, repo: &str) -> Result<String> {
    validate_repository(repo)?;
    let metadata = get(client, &format!("repos/{repo}"))?;
    ensure!(
        metadata["full_name"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(repo))
            && metadata["fork"] == true,
        "upstream repository evidence missing: {repo} is not the expected fork"
    );
    let parent = metadata
        .pointer("/parent/full_name")
        .and_then(Value::as_str)
        .context("upstream repository evidence missing: no fork parent")?;
    validate_repository(parent).context("upstream fork parent")?;
    ensure!(
        !parent.eq_ignore_ascii_case(repo),
        "upstream fork is its own parent"
    );
    Ok(parent.to_owned())
}

/// Resolve an association on an independently identity-checked source issue.
/// No association means no repository metadata or upstream-issue reads.
pub(crate) fn target(client: &impl Rest, source: &ItemRef) -> Result<Option<ItemRef>> {
    let issue = issue(client, &source.repository, source.number)?;
    let body = match issue
        .get("body")
        .context("upstream source issue has no body field")?
    {
        Value::Null => "",
        value => value
            .as_str()
            .context("upstream source issue body is not text")?,
    };
    let Some(number) = parse(body)? else {
        return Ok(None);
    };
    Ok(Some(ItemRef {
        host: source.host.clone(),
        repository: parent(client, &source.repository)?,
        number,
    }))
}

#[derive(Clone, Debug, PartialEq)]
pub struct State {
    pub item: String,
    pub state: String,
    pub reason: Option<String>,
    pub closed_at: Option<OffsetDateTime>,
    pub closed_by: Option<String>,
}

pub type Observations = BTreeMap<String, std::result::Result<State, String>>;

impl State {
    pub(crate) fn json(&self) -> Value {
        serde_json::json!({"item":self.item, "state":self.state, "reason":self.reason,
            "closed_at":self.closed_at.and_then(|at| at.format(&Rfc3339).ok()),
            "closed_by":self.closed_by})
    }

    pub(crate) fn read(client: &impl Rest, target: &ItemRef) -> Result<Self> {
        let issue = issue(client, &target.repository, target.number)?;
        let state = issue["state"]
            .as_str()
            .context("upstream issue has no state")?;
        ensure!(
            matches!(state, "open" | "closed"),
            "upstream issue state invalid"
        );
        let reason = issue["state_reason"].as_str().map(str::to_owned);
        let closed_at = issue["closed_at"]
            .as_str()
            .map(|s| OffsetDateTime::parse(s, &Rfc3339))
            .transpose()
            .context("upstream issue closed_at invalid")?;
        let closed_by = issue
            .pointer("/closed_by/login")
            .and_then(Value::as_str)
            .map(str::to_owned);
        ensure!(
            state != "closed" || closed_at.is_some(),
            "upstream closed issue has no closed_at"
        );
        ensure!(
            reason.as_deref() != Some("completed") || closed_by.is_some(),
            "upstream completed issue has no closing actor"
        );
        Ok(Self {
            item: target.display(),
            state: state.into(),
            reason,
            closed_at,
            closed_by,
        })
    }

    pub(crate) fn completed_by_other(&self, bot: &str) -> Option<OffsetDateTime> {
        (self.state == "closed"
            && self.reason.as_deref() == Some("completed")
            && self
                .closed_by
                .as_ref()
                .is_some_and(|s| !s.eq_ignore_ascii_case(bot)))
        .then_some(self.closed_at)
        .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::poll::RestResponse;
    use serde_json::json;
    use std::cell::RefCell;

    struct Reads {
        answers: BTreeMap<String, Value>,
        seen: RefCell<Vec<String>>,
    }

    impl Rest for Reads {
        fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
            anyhow::bail!("upstream observations must not write")
        }

        fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
            self.seen.borrow_mut().push(url.to_owned());
            Ok(RestResponse {
                status: 200,
                etag: None,
                last_modified: None,
                link: None,
                body: self.answers.get(url).context("unexpected read")?.clone(),
            })
        }
    }

    #[test]
    fn upstream_identity_and_completion() {
        for case in [
            "open",
            "completed",
            "bot completed",
            "not planned",
            "missing actor",
            "wrong issue",
            "pull request",
            "wrong parent",
            "missing parent",
            "not fork",
            "no link",
            "null body",
        ] {
            let source =
                crate::parse_github_item("fork-org/project#1", None, "github.com").unwrap();
            let mut issue = json!({"number":9,"repository_url":format!("{API_ROOT}repos/other-org/project"),
                "state":"closed", "state_reason":"completed", "closed_at":"2026-11-01T00:00:00Z",
                "closed_by":{"login":"maintainer"}});
            let mut metadata = json!({"full_name":"fork-org/project", "fork":true,
                "parent":{"full_name":"other-org/project"}});
            match case {
                "open" => {
                    issue["state"] = json!("open");
                    issue["closed_at"] = Value::Null;
                }
                "bot completed" => issue["closed_by"]["login"] = json!("BOT"),
                "not planned" => issue["state_reason"] = json!("not_planned"),
                "missing actor" => issue["closed_by"] = Value::Null,
                "wrong issue" => issue["number"] = json!(10),
                "pull request" => issue["pull_request"] = json!({}),
                "wrong parent" => metadata["full_name"] = json!("elsewhere/project"),
                "missing parent" => metadata["parent"] = Value::Null,
                "not fork" => metadata["fork"] = json!(false),
                _ => (),
            }
            let reads = Reads {
                answers: BTreeMap::from([
                    (
                        format!("{API_ROOT}repos/fork-org/project/issues/1"),
                        json!({"number":1,
                    "repository_url":format!("{API_ROOT}repos/fork-org/project"),
                     "body":match case { "no link" => json!("No link"), "null body" => Value::Null, _ => json!("Upstream: #9") }}),
                    ),
                    (format!("{API_ROOT}repos/fork-org/project"), metadata),
                    (format!("{API_ROOT}repos/other-org/project/issues/9"), issue),
                ]),
                seen: RefCell::new(vec![]),
            };
            let state = target(&reads, &source).and_then(|target| {
                target
                    .map(|target| State::read(&reads, &target))
                    .transpose()
            });
            match case {
                "no link" | "null body" => {
                    assert!(state.unwrap().is_none());
                    assert_eq!(reads.seen.borrow().len(), 1);
                }
                "open" | "completed" | "bot completed" | "not planned" => {
                    let state = state.unwrap().unwrap();
                    assert_eq!(state.item, "other-org/project#9");
                    assert_eq!(
                        state.completed_by_other("bot").is_some(),
                        case == "completed"
                    );
                    assert_eq!(state.json()["state"], state.state);
                }
                _ => assert!(state.is_err(), "{case}"),
            }
            assert!(reads.seen.borrow().len() <= 3);
        }
    }

    #[test]
    fn upstream_lines() {
        for (body, expected) in [
            ("No association", Ok(None)),
            ("Text\nUpstream: #42\nMore", Ok(Some(42))),
            ("Upstream: #42\r\n", Ok(Some(42))),
            ("Upstream: #1\nUpstream: #2", Err("ambiguous")),
            ("Upstream: #1\nUpstream: #1", Err("ambiguous")),
            ("Upstream: #0", Err("positive")),
            ("Upstream: #", Err("malformed")),
            ("Upstream: #1 extra", Err("malformed")),
            ("Upstream: owner/repo#1", Err("malformed")),
            ("Upstream: other-org/repo#1", Err("malformed")),
            (
                "Upstream: https://github.com/o/r/issues/1",
                Err("malformed"),
            ),
            ("Upstream: #18446744073709551616", Err("overflow")),
            ("> Upstream: #1", Ok(None)),
        ] {
            match expected {
                Ok(value) => assert_eq!(parse(body).unwrap(), value, "{body}"),
                Err(message) => assert!(
                    format!("{:#}", parse(body).unwrap_err()).contains(message),
                    "{body}"
                ),
            }
        }
    }
}
