//! Activity observed alongside the anchor's existing, bounded comment read.
use super::*;

// Fixed item/comment limits prevent a busy repository expanding the pass's
// request envelope. This is a lower bound, not a historical activity census.
const FIELDS: &str = r#"
    sessionIssues: issues(first: 20, orderBy: {field: UPDATED_AT, direction: DESC}) {
      nodes { number updatedAt comments(last: 10) { nodes { createdAt author { login } } } }
    }
    sessionPulls: pullRequests(first: 20, orderBy: {field: UPDATED_AT, direction: DESC}) {
      nodes { number updatedAt mergedAt comments(last: 10) { nodes { createdAt author { login } } } }
    }
    sessionDefault: defaultBranchRef {
      target { ... on Commit { committedDate statusCheckRollup { state } } }
    }
"#;

pub(super) fn query() -> String {
    plan::COMMENTS_QUERY.replacen(
        "    issue(number:",
        &format!("{FIELDS}    issue(number:"),
        1,
    )
}

pub(super) fn read(
    client: &impl Transport,
    anchor: &ItemRef,
) -> Result<(Vec<plan::Comment>, Value)> {
    struct Reader<'a, C> {
        client: &'a C,
        observed: RefCell<Option<Value>>,
    }
    impl<C: Transport> Transport for Reader<'_, C> {
        fn graphql(&self, original: &str, variables: Value) -> Result<Value> {
            if self.observed.borrow().is_some() {
                return self.client.graphql(original, variables);
            }
            let data = self.client.graphql(&query(), variables)?;
            let repo = &data["repository"];
            ensure!(
                repo["sessionIssues"]["nodes"].is_array()
                    && repo["sessionPulls"]["nodes"].is_array()
                    && repo.get("sessionDefault").is_some(),
                "session activity observations missing"
            );
            *self.observed.borrow_mut() = Some(repo.clone());
            Ok(data)
        }
    }
    let reader = Reader {
        client,
        observed: RefCell::new(None),
    };
    let comments = plan::issue_comments(&reader, anchor)?;
    Ok((
        comments,
        reader.observed.into_inner().unwrap_or(Value::Null),
    ))
}

pub(super) fn score(
    observed: &Value,
    anchor: u64,
    since: OffsetDateTime,
    now: OffsetDateTime,
    operator: &str,
) -> u32 {
    let recent = |v: &Value| {
        v.as_str()
            .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
            .is_some_and(|at| at > since && at <= now)
    };
    let mut score = 0;
    for field in ["sessionIssues", "sessionPulls"] {
        for item in observed[field]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .take(20)
        {
            if field == "sessionIssues" && item["number"] == anchor {
                continue; // The pass's ledger must not trigger itself.
            }
            score += u32::from(recent(&item["updatedAt"]));
            score += 2 * u32::from(recent(&item["mergedAt"]));
            for comment in item["comments"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .take(10)
            {
                if recent(&comment["createdAt"]) {
                    score += if text(comment, "/author/login").eq_ignore_ascii_case(operator) {
                        5
                    } else {
                        1
                    };
                }
            }
        }
    }
    let commit = &observed["sessionDefault"]["target"];
    score
        + 2 * u32::from(
            recent(&commit["committedDate"])
                && matches!(
                    commit["statusCheckRollup"]["state"].as_str(),
                    Some("FAILURE" | "ERROR")
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_uses_only_the_existing_first_comment_page() {
        struct Pages {
            calls: Cell<usize>,
            missing: bool,
        }
        impl Transport for Pages {
            fn graphql(&self, sent: &str, variables: Value) -> Result<Value> {
                let page = self.calls.get();
                self.calls.set(page + 1);
                assert_eq!(
                    sent,
                    if page == 0 {
                        query()
                    } else {
                        plan::COMMENTS_QUERY.into()
                    }
                );
                assert_eq!(
                    variables["after"],
                    if page == 0 {
                        Value::Null
                    } else {
                        json!("next")
                    }
                );
                let mut data = json!({"repository": {"issue": {"comments": {
                    "nodes": [], "pageInfo": {"hasNextPage": page == 0, "endCursor": "next"}
                }}}});
                if page == 0 && !self.missing {
                    data["repository"]["sessionIssues"] = json!({"nodes": []});
                    data["repository"]["sessionPulls"] = json!({"nodes": []});
                    data["repository"]["sessionDefault"] = Value::Null;
                }
                Ok(data)
            }
        }
        let anchor = ItemRef {
            host: DEFAULT_HOST.into(),
            repository: "owner/repo".into(),
            number: 40,
        };
        for missing in [false, true] {
            let pages = Pages {
                calls: Cell::new(0),
                missing,
            };
            assert_eq!(read(&pages, &anchor).is_err(), missing);
            assert_eq!(pages.calls.get(), if missing { 1 } else { 2 });
        }
    }

    #[test]
    fn timestamps_weights_ledger_exclusion_and_ci() {
        let since = OffsetDateTime::parse("2026-11-10T00:00:00Z", &Rfc3339).unwrap();
        let now = since + time::Duration::hours(12);
        let at = "2026-11-10T01:00:00Z";
        let mut data = json!({
            "sessionIssues": {"nodes": [
                {"number": 1, "updatedAt": at, "comments": {"nodes": [
                    {"createdAt": at, "author": {"login": "operator"}},
                    {"createdAt": at, "author": {"login": "someone"}},
                    {"createdAt": "2026-11-10T00:00:00Z", "author": {"login": "operator"}}
                ]}},
                {"number": 40, "updatedAt": at, "comments": {"nodes": [{"createdAt": at, "author": {"login": "operator"}}]}},
                {"number": 2, "updatedAt": "invalid"},
                {"number": 3, "updatedAt": "2026-11-11T00:00:00Z"}
            ]},
            "sessionPulls": {"nodes": [{"number": 4, "updatedAt": at, "mergedAt": at}]},
            "sessionDefault": {"target": {"committedDate": at, "statusCheckRollup": {"state": "FAILURE"}}}
        });
        assert_eq!(score(&data, 40, since, now, "operator"), 12);
        assert_eq!(score(&data, 40, now, now, "operator"), 0);
        data["sessionDefault"]["target"]["statusCheckRollup"]["state"] = "SUCCESS".into();
        assert_eq!(score(&data, 40, since, now, "operator"), 10);
        assert_eq!(score(&Value::Null, 40, since, now, "operator"), 0);
    }
}
