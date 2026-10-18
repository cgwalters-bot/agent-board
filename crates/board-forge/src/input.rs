//! Agent input from collaborators only (agent-board#111): an issue or pull
//! request is fit to hand to a run only when its conversation is locked and
//! everyone who wrote in it, its body, comments, reviews and review comments
//! and their edits, has write access to its repository, except for GitHub Actions
//! agentic-job run-status comments.
//!
//! The threat is a visitor's text steering a run: anyone can comment on an
//! open conversation, and a run reads the item it works on. Locking the
//! conversation stops new visitors at the source (GitHub then lets only users
//! with write access comment); this check refuses what got in before the lock
//! or around it. It reads whole or refuses: a conversation longer than one
//! page of each connection is not checked in part.
use crate::github::{API_ROOT, Transport, is_github_login};
use crate::poll::{Rest, validate_repository};
use anyhow::{Context, Result, bail, ensure};
use board_core::ItemRef;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Repository permissions that may write agent input: the legacy `permission`
/// GitHub reports, which reads `maintain` as `write`, and the role names.
const WRITERS: &[&str] = &["admin", "maintain", "write"];
/// Distinct accounts whose permission one check reads: each is a request.
pub const INPUT_LOGIN_LIMIT: usize = 30;
/// Appended by `supervise execute` to each comment it posts for the
/// supervisor agent. That text is the agent's, which may be injected, though
/// it is posted by the write-capable bot; a conversation holding it is not
/// collaborator input.
pub const SUPERVISOR_NOTE: &str = "<!-- agent-board-supervisor-note -->";

pub(crate) const JOB_STATUS_END: &str = "<!-- agentic-job-status -->";

/// Do not admit arbitrary workflow output: review verdicts can be model-written.
/// Status outcomes (including quoted hand-back reasons) are acceptable because
/// these runs were started by collaborators' plans, and report those runs' outcomes.
pub(crate) fn is_job_status(comment: &Value) -> bool {
    comment["author"]["__typename"] == "Bot"
        && comment["author"]["login"] == "github-actions"
        && comment["body"]
            .as_str()
            .is_some_and(|body| body.trim_end().ends_with(JOB_STATUS_END))
}

/// One page of each connection; `hasNextPage` anywhere refuses the item. The
/// fields are the same on issues and pull requests, but a union's members
/// must each be selected.
const INPUT_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issueOrPullRequest(number: $number) {
      __typename
      ... on Issue {
        locked author { ...Actor }
        userContentEdits(first: 100) { nodes { editor { ...Actor } } pageInfo { hasNextPage } }
        comments(first: 100) { nodes { url body lastEditedAt author { ...Actor } editor { ...Actor } } pageInfo { hasNextPage } }
      }
      ... on PullRequest {
        locked author { ...Actor }
        userContentEdits(first: 100) { nodes { editor { ...Actor } } pageInfo { hasNextPage } }
        comments(first: 100) { nodes { url body lastEditedAt author { ...Actor } editor { ...Actor } } pageInfo { hasNextPage } }
        reviews(first: 100) {
          nodes {
            url body lastEditedAt author { ...Actor } editor { ...Actor }
            comments(first: 100) { nodes { url body lastEditedAt author { ...Actor } editor { ...Actor } } pageInfo { hasNextPage } }
          }
          pageInfo { hasNextPage }
        }
      }
    }
  }
}
fragment Actor on Actor { __typename login }"#;

/// Refuse `item` unless its conversation is locked and every account that
/// wrote or edited text in it may write to its repository. The error names
/// the login and where it wrote. `client` needs push access to the repository
/// to read its collaborators' permissions, so an item elsewhere is refused.
pub fn check<T: Transport + Rest>(client: &T, item: &ItemRef) -> Result<()> {
    inspect(client, item, true)
}

/// [`check`] without the lock: whether everyone who wrote in `item` may write
/// to its repository, so that the poll may lock it ([`crate::poll`]).
pub fn check_writers<T: Transport + Rest>(client: &T, item: &ItemRef) -> Result<()> {
    inspect(client, item, false)
}

fn inspect<T: Transport + Rest>(client: &T, item: &ItemRef, locked: bool) -> Result<()> {
    ensure!(
        item.host == crate::github::DEFAULT_HOST,
        "input is checked only on github.com"
    );
    validate_repository(&item.repository)?;
    let (owner, name) = item.repository.split_once('/').context("item repository")?;
    let data = client.graphql(
        INPUT_QUERY,
        json!({"owner": owner, "name": name, "number": item.number}),
    )?;
    let node = data
        .pointer("/repository/issueOrPullRequest")
        .filter(|n| n.is_object())
        .with_context(|| format!("{} was not found", item.display()))?;
    let at = item.display();
    ensure!(
        !locked || node["locked"] == true,
        "{at}: the conversation is not locked, so anyone may add to it; lock it first"
    );
    let mut authors = Authors::default();
    authors.add(&node["author"], || format!("the body of {at}"))?;
    for edit in nodes(node, "userContentEdits", &at)? {
        authors.add(&edit["editor"], || format!("an edit of the body of {at}"))?;
    }
    let mut written = nodes(node, "comments", &at)?;
    if node["__typename"] == "PullRequest" {
        for review in nodes(node, "reviews", &at)? {
            written.push(review);
            written.extend(nodes(review, "comments", &at)?);
        }
    }
    for comment in written {
        let url = comment["url"].as_str().unwrap_or("a comment").to_owned();
        ensure!(
            !comment["body"]
                .as_str()
                .is_some_and(|b| b.contains(SUPERVISOR_NOTE)),
            "{url} is the supervisor agent's text, not a collaborator's"
        );
        let status = is_job_status(comment);
        if !status {
            authors.add(&comment["author"], || url.clone())?;
        }
        // GitHub leaves `editor` null where it cannot name the account, so an
        // edit without one counts as an unknown account's.
        if (comment["lastEditedAt"].is_string() || !comment["editor"].is_null())
            && (!status || comment["editor"] != comment["author"])
        {
            authors.add(&comment["editor"], || format!("an edit of {url}"))?;
        }
    }
    for (login, place) in &authors.0 {
        // GitHub answers 404 for an account that is not a collaborator.
        let response = client
            .get(
                &format!(
                    "{API_ROOT}repos/{}/collaborators/{login}/permission",
                    item.repository
                ),
                None,
            )
            .with_context(|| {
                format!(
                    "{at}: {login} wrote {place}; their permission on {} could not be read",
                    item.repository
                )
            })?;
        let permission = response.body["permission"].as_str().unwrap_or("none");
        let role = response.body["role_name"].as_str().unwrap_or("none");
        ensure!(
            response.status == 200 && (WRITERS.contains(&permission) || WRITERS.contains(&role)),
            "{at}: {login} wrote {place} without write access to {} (permission {permission})",
            item.repository
        );
    }
    Ok(())
}

/// The nodes of one page of `node[key]`, refusing a connection with more.
fn nodes<'a>(node: &'a Value, key: &str, at: &str) -> Result<Vec<&'a Value>> {
    let connection = &node[key];
    match connection["pageInfo"]["hasNextPage"].as_bool() {
        Some(false) => {}
        Some(true) => bail!("{at}: more {key} than one page of 100; it cannot be checked whole"),
        None => bail!("{at}: {key} have no pageInfo"),
    }
    Ok(connection["nodes"]
        .as_array()
        .with_context(|| format!("{at}: {key} have no nodes"))?
        .iter()
        .collect())
}

/// Each account that wrote, by lowercase login, with the first place it wrote.
#[derive(Default)]
struct Authors(BTreeMap<String, String>);

impl Authors {
    fn add(&mut self, actor: &Value, place: impl FnOnce() -> String) -> Result<()> {
        // A deleted account, or one GitHub will not name, cannot be checked.
        let Some(login) = actor["login"].as_str() else {
            bail!("an account GitHub does not name wrote {}", place());
        };
        // An app's bot shares no permission with the user account its login
        // names (`renovate` for `renovate[bot]`), so only users are checked.
        ensure!(
            actor["__typename"] == "User",
            "{login}, not a user account, wrote {}",
            place()
        );
        // The login goes into an API path.
        ensure!(
            is_github_login(login),
            "invalid login {login:?} in {}",
            place()
        );
        if !self.0.contains_key(&login.to_ascii_lowercase()) {
            ensure!(
                self.0.len() < INPUT_LOGIN_LIMIT,
                "more than {INPUT_LOGIN_LIMIT} accounts wrote in {}; it cannot be checked whole",
                place()
            );
            self.0.insert(login.to_ascii_lowercase(), place());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::poll::RestResponse;
    use std::cell::RefCell;

    /// One item, and each login's permission; absent logins are not collaborators.
    struct Forge {
        node: Value,
        permissions: BTreeMap<&'static str, &'static str>,
        asked: RefCell<Vec<String>>,
    }

    impl Transport for Forge {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            assert_eq!(query, INPUT_QUERY);
            assert_eq!(
                variables,
                json!({"owner": "example", "name": "intake", "number": 7})
            );
            Ok(json!({"repository": {"issueOrPullRequest": self.node}}))
        }
    }

    impl Rest for Forge {
        fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
            assert!(validator.is_none());
            let login = url
                .strip_prefix(&format!("{API_ROOT}repos/example/intake/collaborators/"))
                .and_then(|rest| rest.strip_suffix("/permission"))
                .unwrap_or_else(|| panic!("unexpected GET {url}"));
            self.asked.borrow_mut().push(login.into());
            // As the real transport does, 404 is an error.
            let Some(found) = self.permissions.get(login) else {
                return Err(
                    anyhow::Error::from(crate::github::NotFound).context(format!(
                        "GET {}: GitHub HTTP status 404",
                        url.trim_start_matches(API_ROOT)
                    )),
                );
            };
            Ok(RestResponse {
                status: 200,
                etag: None,
                last_modified: None,
                link: None,
                body: json!({"permission": found, "role_name": found}),
            })
        }
        fn send(&self, method: &str, url: &str, _: &Value) -> Result<(u16, Value)> {
            panic!("the check writes nothing: {method} {url}")
        }
    }

    fn page(nodes: Value) -> Value {
        json!({"nodes": nodes, "pageInfo": {"hasNextPage": false}})
    }

    fn comment(id: u64, author: Value, editor: Value) -> Value {
        let edited = if editor.is_null() {
            Value::Null
        } else {
            json!("2026-10-15T10:00:00Z")
        };
        json!({"url": format!("https://github.com/example/intake/issues/7#issuecomment-{id}"),
            "lastEditedAt": edited, "author": author, "editor": editor})
    }

    fn login(login: &str) -> Value {
        json!({"__typename": "User", "login": login})
    }

    /// A locked pull request by the operator, with a comment, an edit of the
    /// body and a review with one comment, all by collaborators.
    fn pull() -> Value {
        json!({
            "__typename": "PullRequest",
            "locked": true,
            "author": login("cgwalters"),
            "userContentEdits": page(json!([{"editor": login("cgwalters-bot")}])),
            "comments": page(json!([comment(1, login("cgwalters-bot"), Value::Null)])),
            "reviews": page(json!([{
                "url": "https://github.com/example/intake/pull/7#pullrequestreview-2",
                "lastEditedAt": null, "author": login("Maintainer"), "editor": null,
                "comments": page(json!([comment(3, login("cgwalters"), Value::Null)])),
            }])),
        })
    }

    #[test]
    fn refuses_unlocked_items_and_input_from_non_collaborators() {
        let permissions = BTreeMap::from([
            ("cgwalters", "admin"),
            ("cgwalters-bot", "write"),
            ("maintainer", "maintain"),
            ("reader", "read"),
        ]);
        let set = |pointer: &str, value: Value| {
            let mut node = pull();
            *node.pointer_mut(pointer).unwrap() = value;
            node
        };
        let more = json!({"nodes": [], "pageInfo": {"hasNextPage": true}});
        let mut crowd = pull();
        crowd["comments"] = page(
            (0..=INPUT_LOGIN_LIMIT as u64)
                .map(|n| comment(n, login(&format!("user{n}")), Value::Null))
                .collect(),
        );
        // The item, then the error or the logins whose permission was read.
        for (node, expected) in [
            (pull(), Ok(vec!["cgwalters", "cgwalters-bot", "maintainer"])),
            // An issue has no reviews to read.
            (
                json!({"__typename": "Issue", "locked": true, "author": login("cgwalters"),
                    "userContentEdits": page(json!([])), "comments": page(json!([]))}),
                Ok(vec!["cgwalters"]),
            ),
            (
                set("/locked", json!(false)),
                Err("example/intake#7: the conversation is not locked"),
            ),
            (
                set("/author", login("visitor")),
                Err(
                    "visitor wrote the body of example/intake#7; their permission on example/intake could not be read: GET repos/example/intake/collaborators/visitor/permission: GitHub HTTP status 404: not found",
                ),
            ),
            (
                set("/userContentEdits/nodes/0/editor", login("reader")),
                Err(
                    "reader wrote an edit of the body of example/intake#7 without write access to example/intake (permission read)",
                ),
            ),
            (
                set("/userContentEdits/nodes/0/editor", Value::Null),
                Err("an account GitHub does not name wrote an edit of the body"),
            ),
            (
                set("/comments/nodes/0/author", login("visitor")),
                Err(
                    "visitor wrote https://github.com/example/intake/issues/7#issuecomment-1; their permission",
                ),
            ),
            (
                set(
                    "/comments/nodes/0",
                    comment(1, login("cgwalters"), login("reader")),
                ),
                Err(
                    "reader wrote an edit of https://github.com/example/intake/issues/7#issuecomment-1 without",
                ),
            ),
            // Edited, but GitHub cannot say by whom.
            (
                set(
                    "/comments/nodes/0/lastEditedAt",
                    json!("2026-10-15T10:00:00Z"),
                ),
                Err(
                    "an account GitHub does not name wrote an edit of https://github.com/example/intake/issues/7#issuecomment-1",
                ),
            ),
            // The bot is a collaborator, but this text is the supervisor agent's.
            (
                set("/comments/nodes/0", {
                    let mut note = comment(1, login("cgwalters-bot"), Value::Null);
                    note["body"] = json!(format!("Do this instead.\n\n{SUPERVISOR_NOTE}"));
                    note
                }),
                Err(
                    "https://github.com/example/intake/issues/7#issuecomment-1 is the supervisor agent's text",
                ),
            ),
            (
                set("/comments/nodes/0/author", Value::Null),
                Err(
                    "an account GitHub does not name wrote https://github.com/example/intake/issues/7#issuecomment-1",
                ),
            ),
            (
                set("/reviews/nodes/0/author", login("visitor")),
                Err(
                    "visitor wrote https://github.com/example/intake/pull/7#pullrequestreview-2; their permission",
                ),
            ),
            (
                set("/reviews/nodes/0/comments/nodes/0/author", login("visitor")),
                Err(
                    "visitor wrote https://github.com/example/intake/issues/7#issuecomment-3; their permission",
                ),
            ),
            // Actions output without a status marker is not collaborator input.
            (
                set(
                    "/comments/nodes/0/author",
                    json!({"__typename": "Bot", "login": "github-actions"}),
                ),
                Err("github-actions, not a user account"),
            ),
            (
                set(
                    "/comments/nodes/0/author",
                    json!({"__typename": "Bot", "login": "renovate[bot]"}),
                ),
                Err("renovate[bot], not a user account"),
            ),
            (
                set(
                    "/comments/nodes/0/author",
                    json!({"__typename": "Bot", "login": "github-actions-bot[bot]"}),
                ),
                Err("github-actions-bot[bot], not a user account"),
            ),
            (
                set("/comments/nodes/0/author", login("github-actions")),
                Err(
                    "github-actions wrote https://github.com/example/intake/issues/7#issuecomment-1; their permission",
                ),
            ),
            (
                set("/comments/nodes/0", {
                    let mut note = comment(
                        1,
                        json!({"__typename": "Bot", "login": "github-actions"}),
                        Value::Null,
                    );
                    note["body"] = json!(SUPERVISOR_NOTE);
                    note
                }),
                Err("is the supervisor agent's text"),
            ),
            (
                set("/comments/nodes/0/author", login("github-actions[bot]")),
                Err("invalid login"),
            ),
            (
                set(
                    "/reviews/nodes/0/author",
                    json!({"__typename": "Bot", "login": "cgwalters"}),
                ),
                Err(
                    "cgwalters, not a user account, wrote https://github.com/example/intake/pull/7#pullrequestreview-2",
                ),
            ),
            (
                set("/comments/nodes/0/author", login("a/../b")),
                Err("invalid login \"a/../b\""),
            ),
            (
                set("/comments", more.clone()),
                Err("more comments than one page"),
            ),
            (
                set("/userContentEdits", more.clone()),
                Err("more userContentEdits than one page"),
            ),
            (
                set("/reviews", more.clone()),
                Err("more reviews than one page"),
            ),
            (
                set("/reviews/nodes/0/comments", more),
                Err("more comments than one page"),
            ),
            (
                set("/comments/pageInfo", Value::Null),
                Err("comments have no pageInfo"),
            ),
            (crowd, Err("more than 30 accounts wrote in")),
            (Value::Null, Err("example/intake#7 was not found")),
        ] {
            let forge = Forge {
                node,
                permissions: permissions.clone(),
                asked: RefCell::default(),
            };
            let item = ItemRef {
                host: "github.com".into(),
                repository: "example/intake".into(),
                number: 7,
            };
            match (check(&forge, &item), expected) {
                (Ok(()), Ok(asked)) => assert_eq!(*forge.asked.borrow(), asked),
                (Err(error), Err(expected)) => {
                    assert!(format!("{error:#}").contains(expected), "{error:#}")
                }
                (result, expected) => panic!("{result:?} != {expected:?}"),
            }
        }
        // The poll checks the writers of an item before it locks it.
        let mut unlocked = pull();
        unlocked["locked"] = json!(false);
        let forge = Forge {
            node: unlocked,
            permissions,
            asked: RefCell::default(),
        };
        let item = ItemRef {
            host: "github.com".into(),
            repository: "example/intake".into(),
            number: 7,
        };
        check_writers(&forge, &item).unwrap();
        assert!(check(&forge, &item).is_err());
    }

    #[test]
    fn status_comments_require_the_app_and_end_marker() {
        let actions = json!({"__typename": "Bot", "login": "github-actions"});
        let none = Value::Null;
        for (kind, name, body, edited, editor, admitted) in [
            ("Bot", "github-actions", JOB_STATUS_END, false, &none, true),
            (
                "Bot",
                "github-actions",
                "Review verdict: approved",
                false,
                &none,
                false,
            ),
            ("Bot", "other-app", JOB_STATUS_END, false, &none, false),
            ("User", "visitor", JOB_STATUS_END, false, &none, false),
            (
                "Bot",
                "github-actions",
                "<!-- agentic-job-status --> more",
                false,
                &none,
                false,
            ),
            // The app updating its own status comment is still its status;
            // anyone else's edit is checked as theirs.
            (
                "Bot",
                "github-actions",
                JOB_STATUS_END,
                true,
                &actions,
                true,
            ),
            (
                "Bot",
                "github-actions",
                JOB_STATUS_END,
                true,
                &login("cgwalters"),
                true,
            ),
            (
                "Bot",
                "github-actions",
                JOB_STATUS_END,
                true,
                &login("visitor"),
                false,
            ),
            ("Bot", "github-actions", JOB_STATUS_END, true, &none, false),
        ] {
            let mut node = pull();
            node["comments"]["nodes"][0] = json!({
                "author": {"__typename": kind, "login": name},
                "body": body, "editor": editor,
                "lastEditedAt": if edited { json!("2026-10-18T08:00:00Z") } else { Value::Null }
            });
            let forge = Forge {
                node,
                permissions: BTreeMap::from([
                    ("cgwalters", "admin"),
                    ("cgwalters-bot", "write"),
                    ("maintainer", "write"),
                ]),
                asked: RefCell::default(),
            };
            let item = ItemRef {
                host: "github.com".into(),
                repository: "example/intake".into(),
                number: 7,
            };
            assert_eq!(
                check(&forge, &item).is_ok(),
                admitted,
                "{kind} {name} {body} {editor}"
            );
            if kind == "User" {
                assert!(forge.asked.borrow().iter().any(|login| login == name));
            }
        }
    }
}
