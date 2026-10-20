//! Agent input from collaborators only (agent-board#111): an issue or pull
//! request is fit to hand to a run only when its conversation is locked and
//! everyone who wrote in it, its body, comments, reviews and review comments
//! and their edits, has write access to its repository.
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
/// Appended by `supervise execute` to each comment and issue body it posts
/// for the supervisor agent. That text is the agent's, which may be injected, though
/// it is posted by the write-capable bot; a conversation holding it is not
/// collaborator input. It is visible, as load-bearing markers are, and the
/// agent cannot write it itself: its text may hold no fence.
pub const SUPERVISOR_NOTE: &str =
    "```agent-board-supervisor-note\n{\"author\":\"supervisor-agent\"}\n```";
/// The opening of the note, which is what readers match.
const SUPERVISOR_NOTE_FENCE: &str = "```agent-board-supervisor-note";
/// The hidden note written before the visible one. Still read, so that
/// comments posted with it keep refusing their conversation; never written.
pub(crate) const LEGACY_SUPERVISOR_NOTE: &str = "<!-- agent-board-supervisor-note -->";

/// Whether `body` holds the supervisor agent's note, in either form.
pub fn is_supervisor_note(body: &str) -> bool {
    body.contains(SUPERVISOR_NOTE_FENCE) || body.contains(LEGACY_SUPERVISOR_NOTE)
}

/// One page of each connection; `hasNextPage` anywhere refuses the item. The
/// fields are the same on issues and pull requests, but a union's members
/// must each be selected.
const INPUT_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issueOrPullRequest(number: $number) {
      __typename
      ... on Issue {
        locked body author { ...Actor }
        userContentEdits(first: 100) { nodes { editor { ...Actor } } pageInfo { hasNextPage } }
        comments(first: 100) { nodes { url databaseId body lastEditedAt author { ...Actor } editor { ...Actor } } pageInfo { hasNextPage } }
      }
      ... on PullRequest {
        locked body author { ...Actor }
        userContentEdits(first: 100) { nodes { editor { ...Actor } } pageInfo { hasNextPage } }
        comments(first: 100) { nodes { url databaseId body lastEditedAt author { ...Actor } editor { ...Actor } } pageInfo { hasNextPage } }
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

/// Who besides collaborators may write input, from the caller's trusted
/// configuration only, never from a plan or the item.
#[derive(Clone, Copy, Debug, Default)]
pub struct Rule {
    /// The status app's id (tracker#766, `docs/apps.md`): an issue comment
    /// whose author is an app's bot is admitted when REST names this app in
    /// its `performed_via_github_app`, and edits of it by the same bot. The
    /// threat is text that only looks like the status app's: GraphQL's `Bot`
    /// login is a slug whose user account anyone may hold, and another app's
    /// bot may write too; GitHub sets the app id from the token, so neither
    /// can show this one. Its run-status comment is all the app writes, so a
    /// bot elsewhere (a body, review or review comment) stays refused. None
    /// (the default) refuses every bot.
    pub status_app: Option<u64>,
}

/// Refuse `item` unless its conversation is locked and every account that
/// wrote or edited text in it may write to its repository, or is admitted
/// by `rule`. The error names the login and where it wrote. `client` needs
/// push access to the repository to read its collaborators' permissions, so
/// an item elsewhere is refused.
pub fn check<T: Transport + Rest>(client: &T, item: &ItemRef, rule: Rule) -> Result<()> {
    inspect(client, item, true, rule)
}

/// [`check`] without the lock: whether everyone who wrote in `item` may write
/// to its repository, so that the poll may lock it ([`crate::poll`]).
pub fn check_writers<T: Transport + Rest>(client: &T, item: &ItemRef, rule: Rule) -> Result<()> {
    inspect(client, item, false, rule)
}

fn inspect<T: Transport + Rest>(
    client: &T,
    item: &ItemRef,
    locked: bool,
    rule: Rule,
) -> Result<()> {
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
    // An issue the supervisor filed has the agent's text as its body.
    ensure!(
        !node["body"].as_str().is_some_and(is_supervisor_note),
        "the body of {at} is the supervisor agent's text, not a collaborator's"
    );
    let mut authors = Authors::default();
    authors.add(&node["author"], None, || format!("the body of {at}"))?;
    for edit in nodes(node, "userContentEdits", &at)? {
        authors.add(&edit["editor"], None, || {
            format!("an edit of the body of {at}")
        })?;
    }
    // Each with its id where an app's bot may have written it: issue
    // comments, with a status app configured.
    let mut written: Vec<_> = nodes(node, "comments", &at)?
        .into_iter()
        .map(|c| (c, rule.status_app.and(c["databaseId"].as_u64())))
        .collect();
    if node["__typename"] == "PullRequest" {
        for review in nodes(node, "reviews", &at)? {
            written.push((review, None));
            written.extend(
                nodes(review, "comments", &at)?
                    .into_iter()
                    .map(|c| (c, None)),
            );
        }
    }
    for (comment, id) in written {
        let url = comment["url"].as_str().unwrap_or("a comment").to_owned();
        ensure!(
            !comment["body"].as_str().is_some_and(is_supervisor_note),
            "{url} is the supervisor agent's text, not a collaborator's"
        );
        authors.add(&comment["author"], id, || url.clone())?;
        // GitHub leaves `editor` null where it cannot name the account, so an
        // edit without one counts as an unknown account's. A bot's edit of
        // its own comment is that bot's, checked with it.
        if comment["lastEditedAt"].is_string() || !comment["editor"].is_null() {
            let own = comment["editor"] == comment["author"];
            authors.add(&comment["editor"], id.filter(|_| own), || {
                format!("an edit of {url}")
            })?;
        }
    }
    for (login, (place, comment)) in &authors.0 {
        if let Some(comment) = comment {
            let app = status_app(client, item, *comment).with_context(|| {
                format!("{at}: {login} wrote {place}; its app could not be read")
            })?;
            ensure!(
                app.is_some() && app == rule.status_app,
                "{at}: {login} wrote {place}, but not as the status app (app {})",
                app.map_or("none".into(), |a| a.to_string())
            );
            continue;
        }
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

/// The app a bot's issue comment was written through, from REST: GraphQL's
/// `IssueComment` has no app field. A comment REST says a user wrote is not
/// the bot's that GraphQL named, so it names no app.
fn status_app<T: Rest>(client: &T, item: &ItemRef, comment: u64) -> Result<Option<u64>> {
    let response = client.get(
        &format!(
            "{API_ROOT}repos/{}/issues/comments/{comment}",
            item.repository
        ),
        None,
    )?;
    ensure!(
        response.status == 200,
        "GitHub HTTP status {}",
        response.status
    );
    let body = &response.body;
    Ok(body["performed_via_github_app"]["id"]
        .as_u64()
        .filter(|_| body["user"]["type"] == "Bot"))
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

/// Each account that wrote, by lowercase login, with the first place it
/// wrote; an app's bot as `SLUG[bot]`, with the comment that shows its app.
#[derive(Default)]
struct Authors(BTreeMap<String, (String, Option<u64>)>);

impl Authors {
    /// `comment` is the id of the issue comment `actor` wrote, where a bot
    /// may be admitted ([`Rule::status_app`]).
    fn add(
        &mut self,
        actor: &Value,
        comment: Option<u64>,
        place: impl FnOnce() -> String,
    ) -> Result<()> {
        // A deleted account, or one GitHub will not name, cannot be checked.
        let Some(login) = actor["login"].as_str() else {
            bail!("an account GitHub does not name wrote {}", place());
        };
        // An app's bot shares no permission with the user account its login
        // names (`renovate` for `renovate[bot]`), so only users' permissions
        // are checked, and a bot only by its app.
        let bot = actor["__typename"] == "Bot" && comment.is_some();
        ensure!(
            actor["__typename"] == "User" || bot,
            "{login}, not a user account, wrote {}",
            place()
        );
        // The login goes into an API path.
        ensure!(
            is_github_login(login),
            "invalid login {login:?} in {}",
            place()
        );
        let (key, comment) = match bot {
            true => (format!("{}[bot]", login.to_ascii_lowercase()), comment),
            false => (login.to_ascii_lowercase(), None),
        };
        if !self.0.contains_key(&key) {
            // Each is one request.
            ensure!(
                self.0.len() < INPUT_LOGIN_LIMIT,
                "more than {INPUT_LOGIN_LIMIT} accounts wrote in {}; it cannot be checked whole",
                place()
            );
            self.0.insert(key, (place(), comment));
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
            // Comment 4 is the status app's, 5 another app's under the same
            // bot login, and 6 the status app's on a user's behalf.
            if let Some(id) =
                url.strip_prefix(&format!("{API_ROOT}repos/example/intake/issues/comments/"))
            {
                self.asked.borrow_mut().push(format!("comment {id}"));
                let (app, user) = match id {
                    "4" => (
                        STATUS_APP,
                        json!({"login": "board-status[bot]", "type": "Bot"}),
                    ),
                    "5" => (7, json!({"login": "board-status[bot]", "type": "Bot"})),
                    "6" => (STATUS_APP, json!({"login": "board-status", "type": "User"})),
                    _ => panic!("unexpected GET {url}"),
                };
                return Ok(RestResponse {
                    status: 200,
                    etag: None,
                    last_modified: None,
                    link: None,
                    body: json!({"user": user,
                        "performed_via_github_app": {"id": app, "slug": "board-status"}}),
                });
            }
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
            "databaseId": id, "lastEditedAt": edited, "author": author, "editor": editor})
    }

    fn login(login: &str) -> Value {
        json!({"__typename": "User", "login": login})
    }

    /// GraphQL names an app's bot by its slug, without `[bot]`.
    fn bot(slug: &str) -> Value {
        json!({"__typename": "Bot", "login": slug})
    }

    const STATUS_APP: u64 = 42;
    const STATUS: Rule = Rule {
        status_app: Some(STATUS_APP),
    };

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
            // Comments posted before the note became visible still hold it.
            (
                set("/comments/nodes/0", {
                    let mut note = comment(1, login("cgwalters-bot"), Value::Null);
                    note["body"] = json!(format!("Do this instead.\n\n{LEGACY_SUPERVISOR_NOTE}"));
                    note
                }),
                Err(
                    "https://github.com/example/intake/issues/7#issuecomment-1 is the supervisor agent's text",
                ),
            ),
            // An issue the supervisor filed.
            (
                {
                    let mut filed = pull();
                    filed["body"] = json!(format!("Runs time out.\n\n{SUPERVISOR_NOTE}"));
                    filed
                },
                Err("the body of example/intake#7 is the supervisor agent's text"),
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
            // A bot's login names a user account, not the app.
            (
                set(
                    "/reviews/nodes/0/author",
                    json!({"__typename": "Bot", "login": "cgwalters"}),
                ),
                Err(
                    "cgwalters, not a user account, wrote https://github.com/example/intake/pull/7#pullrequestreview-2",
                ),
            ),
            // The status app's comment, and its own edit of it, are admitted
            // by the app REST names.
            (
                set(
                    "/comments/nodes/0",
                    comment(4, bot("board-status"), Value::Null),
                ),
                Ok(vec![
                    "comment 4",
                    "cgwalters",
                    "cgwalters-bot",
                    "maintainer",
                ]),
            ),
            (
                set(
                    "/comments/nodes/0",
                    comment(4, bot("board-status"), bot("board-status")),
                ),
                Ok(vec![
                    "comment 4",
                    "cgwalters",
                    "cgwalters-bot",
                    "maintainer",
                ]),
            ),
            // The same login text is not the app: another app's bot, ...
            (
                set(
                    "/comments/nodes/0",
                    comment(5, bot("board-status"), Value::Null),
                ),
                Err(
                    "board-status[bot] wrote https://github.com/example/intake/issues/7#issuecomment-5, but not as the status app (app 7)",
                ),
            ),
            // ... a comment REST says a user wrote, ...
            (
                set(
                    "/comments/nodes/0",
                    comment(6, bot("board-status"), Value::Null),
                ),
                Err("issuecomment-6, but not as the status app (app none)"),
            ),
            // ... a user account of that name, ...
            (
                set(
                    "/comments/nodes/0",
                    comment(4, login("board-status"), Value::Null),
                ),
                Err(
                    "board-status wrote https://github.com/example/intake/issues/7#issuecomment-4; their permission",
                ),
            ),
            // ... or another bot editing the app's comment.
            (
                set(
                    "/comments/nodes/0",
                    comment(4, bot("board-status"), bot("other")),
                ),
                Err(
                    "other, not a user account, wrote an edit of https://github.com/example/intake/issues/7#issuecomment-4",
                ),
            ),
            // The app writes only issue comments.
            (
                set(
                    "/reviews/nodes/0/comments/nodes/0/author",
                    bot("board-status"),
                ),
                Err(
                    "board-status, not a user account, wrote https://github.com/example/intake/issues/7#issuecomment-3",
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
            match (check(&forge, &item, STATUS), expected) {
                (Ok(()), Ok(asked)) => assert_eq!(*forge.asked.borrow(), asked),
                (Err(error), Err(expected)) => {
                    assert!(format!("{error:#}").contains(expected), "{error:#}")
                }
                (result, expected) => panic!("{result:?} != {expected:?}"),
            }
        }
        let item = ItemRef {
            host: "github.com".into(),
            repository: "example/intake".into(),
            number: 7,
        };
        // Without a status app configured, no bot is admitted or looked up.
        let mut status = pull();
        status["comments"] = page(json!([comment(4, bot("board-status"), Value::Null)]));
        let forge = Forge {
            node: status,
            permissions: permissions.clone(),
            asked: RefCell::default(),
        };
        let error = check(&forge, &item, Rule::default()).unwrap_err();
        assert!(
            format!("{error:#}").contains("board-status, not a user account, wrote"),
            "{error:#}"
        );
        assert!(forge.asked.borrow().is_empty());
        // The poll checks the writers of an item before it locks it.
        let mut unlocked = pull();
        unlocked["locked"] = json!(false);
        let forge = Forge {
            node: unlocked,
            permissions,
            asked: RefCell::default(),
        };
        check_writers(&forge, &item, Rule::default()).unwrap();
        assert!(check(&forge, &item, Rule::default()).is_err());
    }
}
