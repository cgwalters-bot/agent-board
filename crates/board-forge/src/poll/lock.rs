//! `lockCollaboratorItems`: lock the open issues of the followed repositories
//! whose input is only from collaborators ([`crate::input::check`],
//! agent-board#139). It was the supervisor's backstop when `supervise
//! execute` took only a locked item; dispatch no longer needs a lock
//! (agent-board#155), so this stays only for a caller that opts in.
//!
//! Off by default. Locking only stops people without write access from
//! adding to an item; it never makes one dispatchable by itself, as
//! `dispatch --collaborators-only` reads the whole conversation again. So an
//! item with any other input is left unlocked: locked, it would look ready
//! to that check while its text could still steer a run. The bound is on the items checked a pass, each a
//! query and a read of each writer's permission, so that the first passes
//! over a backlog do not spend the rate limit; the pass's request budget
//! bounds them too, and the rest wait for later passes.
use super::*;
use crate::github::DEFAULT_HOST;
use crate::input;
use board_core::ItemRef;
use std::cell::RefCell;

/// The most `lockCollaboratorItems` may be.
pub const LOCK_ITEMS_LIMIT: usize = 100;
/// The `lock_reason` the API records: the item's conversation is settled.
const LOCK_REASON: &str = "resolved";
/// The most requests one item takes: its query, a permission read for each
/// account that wrote in it, and the lock.
const ITEM_REQUESTS: usize = input::INPUT_LOGIN_LIMIT + 2;

/// The pass as the input check's client: each request is charged to the
/// pass's budget, and GETs go through its cache.
struct Charged<'p, 'a, R, G>(RefCell<&'p mut Pass<'a, R, G>>);

impl<R: Rest, G: Transport> Rest for Charged<'_, '_, R, G> {
    fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
        // The client errs on a status other than 2xx or 304, and the cache
        // answers a 304 with the body it kept.
        let (body, _) = self.0.borrow_mut().get_url(url, Value::clone, false)?;
        Ok(RestResponse {
            status: 200,
            etag: None,
            last_modified: None,
            link: None,
            body,
        })
    }

    fn send(&self, method: &str, url: &str, _: &Value) -> Result<(u16, Value)> {
        bail!("the input check writes nothing: {method} {url}")
    }
}

impl<R: Rest, G: Transport> Transport for Charged<'_, '_, R, G> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let mut pass = self.0.borrow_mut();
        pass.spend()?;
        pass.graphql.graphql(query, variables)
    }
}

/// An open item as the pass keeps the list: not its text.
fn slim_items(body: &Value) -> Value {
    body.as_array().map_or(Value::Null, |items| {
        items
            .iter()
            // PR locks block reviews by non-collaborator job tokens, and the
            // supervisor only dispatches issues.
            .filter(|i| i.get("pull_request").is_none())
            .map(|i| json!({"number": i["number"], "locked": i["locked"], "updated_at": i["updated_at"]}))
            .collect()
    })
}

impl<R: Rest, G: Transport> Pass<'_, R, G> {
    /// Check up to `limit` open, unlocked items of `follow`, and lock those
    /// with input only from collaborators where the pass applies. Items
    /// `refused` before, unchanged since, go last, so that a backlog of them
    /// cannot keep the rest from their turn. Returns what to keep as
    /// [`State::lock_refused`].
    pub(super) fn lock(
        &mut self,
        follow: &BTreeSet<&String>,
        limit: usize,
        refused: &BTreeMap<String, String>,
        outcome: &mut Outcome,
    ) -> BTreeMap<String, String> {
        // Refused before and unchanged, repository, number, `updated_at`.
        let mut queue = Vec::new();
        for repo in follow {
            match self.open_items(repo) {
                Ok(items) => {
                    for item in items {
                        let (Some(number), Some(updated)) =
                            (item["number"].as_u64(), item["updated_at"].as_str())
                        else {
                            continue;
                        };
                        // A missing `locked` is not read as unlocked.
                        if item["locked"] == false {
                            let key = format!("{repo}#{number}");
                            let seen = refused.get(&key).is_some_and(|u| u == updated);
                            queue.push((seen, (*repo).clone(), number, updated.to_owned()));
                        }
                    }
                }
                Err(error) => {
                    outcome.errors += 1;
                    outcome.lines.push(format!("{repo} lock error: {error:#}"));
                }
            }
        }
        queue.sort();
        let total = queue.len();
        let (mut checked, mut locked, mut left) = (0, 0, 0);
        let mut kept = BTreeMap::new();
        for (seen, repo, number, updated) in queue {
            let key = format!("{repo}#{number}");
            // Without the requests one item may take, the rest wait for a
            // later pass rather than fail this one.
            if checked == limit || self.remaining < ITEM_REQUESTS {
                if seen {
                    kept.insert(key, updated);
                }
                continue;
            }
            checked += 1;
            let item = ItemRef {
                host: DEFAULT_HOST.into(),
                repository: repo.clone(),
                number,
            };
            let result = input::check_writers(&Charged(RefCell::new(&mut *self)), &item);
            match result {
                Err(error) => {
                    left += 1;
                    kept.insert(key.clone(), updated);
                    outcome
                        .lines
                        .push(format!("{key} unlocked: not dispatchable: {error:#}"));
                }
                // Text added between the check and the lock is locked in, but
                // the dispatching check reads it and refuses the item.
                Ok(()) => {
                    let rest = self.rest;
                    let path = format!("repos/{repo}/issues/{number}/lock");
                    let sent = match self.apply.is_some() {
                        true => self
                            .send(rest, "PUT", &path, json!({"lock_reason": LOCK_REASON}), 0)
                            .map(drop),
                        false => Ok(()),
                    };
                    match sent {
                        Ok(()) => {
                            locked += 1;
                            outcome
                                .lines
                                .push(format!("{key} lock: input only from collaborators"));
                        }
                        Err(error) => {
                            outcome.errors += 1;
                            outcome.lines.push(format!("{key} lock error: {error:#}"));
                        }
                    }
                }
            }
        }
        outcome.lines.push(format!(
            "lock: {locked} locked, {left} left unlocked, {} unchecked (at most {limit} a pass)",
            total - checked
        ));
        kept
    }

    /// Every page of a followed repository's open issues, excluding pull requests.
    fn open_items(&mut self, repo: &str) -> Result<Vec<Value>> {
        let mut url = Some(format!(
            "{API_ROOT}repos/{repo}/issues?state=open&per_page=100"
        ));
        let mut all = Vec::new();
        while let Some(page) = url {
            ensure!(
                page.starts_with(API_ROOT),
                "pagination left the API: {page}"
            );
            // Older cached lists omitted the issue/PR marker. Read fresh so
            // those entries cannot cause a pull request to be locked.
            let (body, next) = self.get_url(&page, slim_items, true)?;
            all.extend(
                body.as_array()
                    .context("open issues are not an array")?
                    .iter()
                    .cloned(),
            );
            url = next;
        }
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "example/intake";
    const BOT: &str = "example-bot";
    const UPDATED: &str = "2026-10-16T12:00:00Z";

    /// One followed repository: its open items, the conversation of each by
    /// number, and its collaborators' permissions.
    #[derive(Default)]
    struct Forge {
        items: Vec<Value>,
        conversations: BTreeMap<u64, Value>,
        writers: BTreeSet<&'static str>,
        gets: RefCell<Vec<String>>,
        queried: RefCell<Vec<u64>>,
        writes: RefCell<Vec<(String, Value)>>,
    }

    impl Rest for Forge {
        fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
            self.gets.borrow_mut().push(url.into());
            let path = url.strip_prefix(API_ROOT).context("outside the API")?;
            let prefix = format!("repos/{REPO}/");
            let body = match path.strip_prefix(&prefix) {
                Some("events?per_page=100") | Some("pulls?state=open&per_page=100") => json!([]),
                Some("issues?state=open&per_page=100") => {
                    assert!(validator.is_none(), "item identity requires a fresh list");
                    json!(self.items)
                }
                Some(rest) if rest.starts_with("collaborators/") => {
                    let login = rest
                        .trim_start_matches("collaborators/")
                        .trim_end_matches("/permission");
                    ensure!(
                        self.writers.contains(login),
                        "GET {path}: GitHub HTTP status 404"
                    );
                    json!({"permission": "write", "role_name": "write"})
                }
                _ if path == "user" => json!({"login": BOT}),
                _ => bail!("unexpected GET {url}"),
            };
            Ok(RestResponse {
                status: 200,
                etag: None,
                last_modified: None,
                link: None,
                body,
            })
        }

        fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
            self.writes
                .borrow_mut()
                .push((format!("{method} {url}"), body.clone()));
            Ok((204, Value::Null))
        }
    }

    impl Transport for Forge {
        fn graphql(&self, _: &str, variables: Value) -> Result<Value> {
            let number = variables["number"].as_u64().context("number")?;
            self.queried.borrow_mut().push(number);
            Ok(json!({"repository": {"issueOrPullRequest": self.conversations[&number]}}))
        }
    }

    fn user(login: &str) -> Value {
        json!({"__typename": "User", "login": login})
    }

    fn page(nodes: Value) -> Value {
        json!({"nodes": nodes, "pageInfo": {"hasNextPage": false}})
    }

    /// Open issue `number`, by the operator, with a comment by `commenter`.
    fn issue(forge: &mut Forge, number: u64, locked: bool, commenter: &str) {
        forge.items.push(
            json!({"number": number, "locked": locked, "updated_at": UPDATED, "body": "kept out of the state"}),
        );
        let comment = json!({
            "url": format!("https://github.com/{REPO}/issues/{number}#issuecomment-{number}"),
            "body": "text", "lastEditedAt": null, "author": user(commenter), "editor": null,
        });
        forge.conversations.insert(
            number,
            json!({"__typename": "Issue", "locked": locked, "author": user("operator"),
                "userContentEdits": page(json!([])), "comments": page(json!([comment]))}),
        );
    }

    fn forge() -> Forge {
        let mut forge = Forge {
            writers: BTreeSet::from(["operator", BOT]),
            ..Forge::default()
        };
        issue(&mut forge, 1, false, "stranger");
        issue(&mut forge, 2, false, BOT);
        issue(&mut forge, 3, true, BOT);
        forge
    }

    fn pass(forge: &Forge, policy: Value, state: State, apply: bool) -> Outcome {
        let config = Config {
            operator: "operator".into(),
            bot: BOT.into(),
            dispatch_repo: "example/runners".into(),
            review_workflow: None,
            policy: PollPolicy::from_json(&policy).unwrap(),
        };
        run(
            forge,
            forge,
            &config,
            state,
            &[],
            &[REPO.into()],
            apply.then(|| Apply {
                dispatcher: forge,
                workflow: "dispatch.yml".into(),
                pools: vec![],
            }),
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap()
    }

    fn lock_url(number: u64) -> String {
        format!("PUT {API_ROOT}repos/{REPO}/issues/{number}/lock")
    }

    #[test]
    fn collaborator_pull_requests_are_not_locked_or_counted_against_the_bound() {
        for apply in [false, true] {
            let mut forge = Forge {
                writers: BTreeSet::from(["operator", BOT]),
                ..Forge::default()
            };
            issue(&mut forge, 1, false, BOT);
            forge.items[0]["pull_request"] = json!({"url": "pull request"});
            forge.conversations.get_mut(&1).unwrap()["__typename"] = json!("PullRequest");
            issue(&mut forge, 2, false, BOT);
            // A pre-fix cache has no way to distinguish the PR from an issue.
            let state = State {
                responses: BTreeMap::from([(
                    format!("{API_ROOT}repos/{REPO}/issues?state=open&per_page=100"),
                    json!({"etag": "old-list", "body": [
                        {"number": 1, "locked": false, "updated_at": UPDATED},
                        {"number": 2, "locked": false, "updated_at": UPDATED}
                    ]}),
                )]),
                ..State::default()
            };
            let outcome = pass(&forge, json!({"lockCollaboratorItems": 1}), state, apply);
            assert_eq!(outcome.errors, 0, "{:?}", outcome.lines);
            assert_eq!(*forge.queried.borrow(), [2]);
            let expected = if apply {
                vec![(lock_url(2), json!({"lock_reason": "resolved"}))]
            } else {
                vec![]
            };
            assert_eq!(*forge.writes.borrow(), expected);
            assert!(outcome.state.lock_refused.is_empty());
            assert_eq!(
                outcome.lines.last().unwrap(),
                "lock: 1 locked, 0 left unlocked, 0 unchecked (at most 1 a pass)"
            );
        }
    }

    #[test]
    fn collaborator_only_items_are_locked_and_others_left_with_a_reason() {
        let forge = forge();
        let outcome = pass(
            &forge,
            json!({"lockCollaboratorItems": 30}),
            State::default(),
            true,
        );
        assert_eq!(outcome.errors, 0, "{:?}", outcome.lines);
        assert_eq!(
            *forge.writes.borrow(),
            [(lock_url(2), json!({"lock_reason": "resolved"}))]
        );
        // The locked one is not checked again.
        assert_eq!(*forge.queried.borrow(), [1, 2]);
        assert_eq!(
            outcome.lines,
            [
                format!(
                    "{REPO}#1 unlocked: not dispatchable: {REPO}#1: stranger wrote https://github.com/{REPO}/issues/1#issuecomment-1; their permission on {REPO} could not be read: GET repos/{REPO}/collaborators/stranger/permission: GitHub HTTP status 404"
                ),
                format!("{REPO}#2 lock: input only from collaborators"),
                "lock: 1 locked, 1 left unlocked, 0 unchecked (at most 30 a pass)".into(),
            ]
        );
        assert_eq!(
            outcome.state.lock_refused,
            BTreeMap::from([(format!("{REPO}#1"), UPDATED.into())])
        );
        // The list is kept without the items' text.
        let state = outcome.state.to_json().to_string();
        assert!(!state.contains("kept out of the state"), "{state}");
        assert_eq!(
            State::from_json(&outcome.state.to_json()).unwrap(),
            outcome.state
        );

        // A preview checks the same and locks nothing.
        let forge = self::forge();
        let preview = pass(
            &forge,
            json!({"lockCollaboratorItems": 30}),
            State::default(),
            false,
        );
        assert!(forge.writes.borrow().is_empty());
        assert_eq!(preview.lines[1..], outcome.lines[1..]);
    }

    #[test]
    fn the_bound_holds_and_a_refused_item_waits_until_it_changes() {
        let forge = forge();
        let first = pass(
            &forge,
            json!({"lockCollaboratorItems": 1}),
            State::default(),
            true,
        );
        assert_eq!(*forge.queried.borrow(), [1]);
        assert!(forge.writes.borrow().is_empty());
        assert_eq!(
            first.lines.last().unwrap(),
            "lock: 0 locked, 1 left unlocked, 1 unchecked (at most 1 a pass)"
        );
        // The next pass takes #2 before the refused #1, which is kept.
        let forge = self::forge();
        let second = pass(
            &forge,
            json!({"lockCollaboratorItems": 1}),
            first.state,
            true,
        );
        assert_eq!(*forge.queried.borrow(), [2]);
        assert_eq!(
            *forge.writes.borrow(),
            [(lock_url(2), json!({"lock_reason": "resolved"}))]
        );
        assert_eq!(second.state.lock_refused.len(), 1);
        // Changed since, #1 comes first again.
        let mut forge = self::forge();
        forge.items[0]["updated_at"] = json!("2026-10-17T00:00:00Z");
        let third = pass(
            &forge,
            json!({"lockCollaboratorItems": 1}),
            second.state,
            true,
        );
        assert_eq!(*forge.queried.borrow(), [1]);
        assert_eq!(
            third.state.lock_refused,
            BTreeMap::from([(format!("{REPO}#1"), "2026-10-17T00:00:00Z".into())])
        );
    }

    #[test]
    fn items_wait_without_the_requests_one_may_take() {
        let forge = forge();
        let config = Config {
            operator: "operator".into(),
            bot: BOT.into(),
            dispatch_repo: "example/runners".into(),
            review_workflow: None,
            policy: PollPolicy::default(),
        };
        // The list, then enough for #1 and not for #2 after it.
        let mut pass = Pass {
            rest: &forge,
            graphql: &forge,
            config: &config,
            now: OffsetDateTime::UNIX_EPOCH,
            previous: BTreeMap::new(),
            used: BTreeMap::new(),
            remaining: 1 + ITEM_REQUESTS,
            apply: None,
            dispatched: 0,
            queues: BTreeMap::new(),
            outages: None,
            paces: None,
        };
        let mut outcome = Outcome {
            lines: Vec::new(),
            errors: 0,
            activity: 0,
            in_flight: 0,
            state: State::default(),
        };
        let repo = REPO.to_owned();
        pass.lock(&BTreeSet::from([&repo]), 30, &BTreeMap::new(), &mut outcome);
        assert_eq!(*forge.queried.borrow(), [1]);
        assert_eq!(outcome.errors, 0);
        assert_eq!(
            outcome.lines.last().unwrap(),
            "lock: 0 locked, 1 left unlocked, 1 unchecked (at most 30 a pass)"
        );
    }

    #[test]
    fn off_by_default() {
        let forge = forge();
        let outcome = pass(&forge, json!({}), State::default(), true);
        assert!(outcome.lines.is_empty(), "{:?}", outcome.lines);
        assert!(forge.queried.borrow().is_empty());
        assert!(!forge.gets.borrow().iter().any(|u| u.contains("/issues?")));
    }

    #[test]
    fn lock_items_policy_table() {
        for (value, want) in [
            (json!({}), Some(None)),
            (json!({"lockCollaboratorItems": 1}), Some(Some(1))),
            (
                json!({"lockCollaboratorItems": LOCK_ITEMS_LIMIT}),
                Some(Some(LOCK_ITEMS_LIMIT)),
            ),
            (json!({"lockCollaboratorItems": 0}), None),
            (json!({"lockCollaboratorItems": LOCK_ITEMS_LIMIT + 1}), None),
            (json!({"lockCollaboratorItems": "30"}), None),
            (json!({"lockCollaboratorItems": true}), None),
        ] {
            let got = PollPolicy::from_json(&value).ok().map(|p| p.lock_items);
            assert_eq!(got, want, "{value}");
        }
        for bad in [
            json!({"lock_refused": {"example/intake": UPDATED}}),
            json!({"lock_refused": {"a/../b#1": UPDATED}}),
            json!({"lock_refused": {"example/intake#1": "yesterday"}}),
            json!({"lock_refused": []}),
        ] {
            let mut state = State {
                interval_seconds: MIN_INTERVAL_SECONDS,
                ..State::default()
            }
            .to_json();
            state["lock_refused"] = bad["lock_refused"].clone();
            assert!(State::from_json(&state).is_err(), "{bad}");
        }
    }
}
