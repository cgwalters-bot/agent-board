//! Blockers (agent-board#482, docs/pass.md#blockers): the pull requests an
//! overlap hold names. The hold stays: an issue whose run would change what
//! an open pull request of the bot changes waits for it. But a pull request
//! nobody moves held its overlaps for good, and the stall alarm then said
//! so once for every item it held. So stage 4 looks at each such pull
//! request once, whatever number of items it holds, and either sees it move
//! or raises one needs-you card for it.
//!
//! The pass moves only what it follows. Stage 2 has already decided a step
//! for each of the bot's pull requests in `follow` ([`poll::Stepped`]): a
//! fix or rebase round where it conflicts or changes were requested, a
//! review start with `pr.reviewPools`, a merge, each within the poll's
//! rounds and the pass's caps, and deduplicated by head by its marker. That
//! is the blocker's one action this pass, and nothing here starts or writes
//! anything of its own. A blocker stage 2 cannot move (a person's to act on,
//! a review nothing starts, or a repository outside `follow`) and that has
//! stood still for `overlap.staleHours` is a [`HoldKind::Blocking`] entry
//! in the hold snapshot: one card, which the needs-you list carries, in
//! place of the stall alarms of the items it holds.
use super::*;
use crate::poll::{Progress, Stepped};

/// The most blocking pull requests outside `follow` read a pass; one past
/// them is not read, and the holds of its items may stall as before.
const BLOCKER_READS: usize = 10;
/// Requests left before each read for the hold snapshot and its needs-you
/// list.
const BLOCKER_RESERVE: usize = WRITE_RESERVE + HOLDS_READS + plan::PLAN_COMMENT_PAGES;
/// Commit characters a card names its head by.
const HEAD_CHARS: usize = 12;

/// A blocker outside `follow`, as far as its card needs it.
pub(super) const PULL_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) { pullRequest(number: $number) {
    state isDraft mergeable headRefOid createdAt reviewDecision
    commits(last: 1) { nodes { commit { committedDate } } }
    reviews(last: 1) { nodes { submittedAt } }
  } }
}"#;

/// The open pull requests that overlap holds name, each with the stage 4
/// candidates it holds.
pub(super) type Blocking = BTreeMap<ItemRef, BTreeSet<ItemRef>>;

/// What the pass saw of one blocker.
#[derive(Debug, PartialEq)]
enum Seen {
    /// Its step moved it this pass: how.
    Moving(String),
    /// It waits on what stage 2 follows by itself: how, and since when
    /// its head has not changed, where that reads. A wait as long as
    /// `overlap.staleHours` is not moving it.
    Waiting {
        how: String,
        since: Option<OffsetDateTime>,
    },
    /// Nothing but a person moves it, at `head`: why, and since when it
    /// has stood still where that reads.
    Still {
        head: String,
        why: String,
        since: Option<OffsetDateTime>,
    },
    /// Merged or closed since the overlap check read it.
    Gone(String),
    /// Not seen to move or to wait on a person: why.
    Unseen(String),
}

/// Stage 2's step for a followed blocker, as [`Seen`].
fn stepped(step: &Stepped) -> Seen {
    let said = format!("{}: {}", step.action, step.reason);
    let still = |why: String| Seen::Still {
        head: step.head.clone(),
        why,
        since: step.moved_at,
    };
    match step.progress() {
        Progress::Moving => Seen::Moving(format!("stage 2 moves it ({said})")),
        Progress::Waiting => Seen::Waiting {
            how: said,
            since: step.moved_at,
        },
        Progress::Held => Seen::Unseen(format!("stage 2's step did not run this pass ({said})")),
        Progress::Unreviewed => still(format!("{said}, and no review of it has come")),
        Progress::Person => still(said),
    }
}

/// A blocker outside `follow`, `node` as [`PULL_QUERY`] read it, as
/// [`Seen`]: none of it moves by itself.
fn unfollowed(node: &Value, repository: &str) -> Seen {
    let at = |pointer: &str| {
        node.pointer(pointer)
            .and_then(Value::as_str)
            .and_then(|at| OffsetDateTime::parse(at, &Rfc3339).ok())
    };
    let state = text(node, "/state");
    if state != "OPEN" {
        return Seen::Gone(format!("it is {} now", state.to_ascii_lowercase()));
    }
    let what = if node["isDraft"] == true {
        "it is a draft"
    } else if node["mergeable"] == "CONFLICTING" {
        "it conflicts with its base"
    } else {
        match text(node, "/reviewDecision") {
            "CHANGES_REQUESTED" => "changes were requested",
            "APPROVED" => "it is approved and waits to be merged",
            _ if at("/reviews/nodes/0/submittedAt").is_none() => "it has no review",
            _ => "it waits for a review",
        }
    };
    // Opened and committed must both read, or its age is unknown; a
    // review is later where it has one.
    let since = (at("/createdAt").zip(at("/commits/nodes/0/commit/committedDate")))
        .map(|(opened, committed)| opened.max(committed))
        .map(|changed| changed.max(at("/reviews/nodes/0/submittedAt").unwrap_or(changed)));
    Seen::Still {
        head: text(node, "/headRefOid").to_owned(),
        why: format!("{what}, and the pass does not follow {repository}"),
        since,
    }
}

/// The items `held` names, in order.
fn named(held: &BTreeSet<ItemRef>) -> String {
    let names: Vec<String> = held.iter().map(ItemRef::display).collect();
    names.join(", ")
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    /// Each pull request this pass's overlap holds named, once: a line for
    /// it, and a card for one that only a person moves and that has stood
    /// still for `overlap.staleHours`. The items of each blocker it saw
    /// move or wait on a person raise no stall alarm of their own
    /// ([`holds::Watch::blocked`]). Off without `overlap`, or with
    /// `staleHours` 0.
    pub(super) fn blockers(&mut self, board: &BoardRead) {
        let blocking = std::mem::take(&mut *self.blocking.borrow_mut());
        let Some(hours) = (self.config.overlap.as_ref())
            .map(|o| o.stale_hours)
            .filter(|h| *h > 0)
        else {
            return;
        };
        let mut reads = BLOCKER_READS;
        // Items held by a blocker that was not seen may stall as before.
        let (mut seen_on, mut unseen_on) = (BTreeSet::new(), BTreeSet::new());
        for (pull, held) in blocking {
            let items = named(&held);
            let seen = self.seen(&pull, &mut reads);
            let said = |what: &str| format!("blocker {}: {what}; it holds {items}", pull.display());
            let idle = |since: Option<OffsetDateTime>| {
                since
                    .map(|since| (self.now - since).whole_hours())
                    .filter(|idle| *idle < i64::try_from(hours).unwrap_or(i64::MAX))
            };
            let (head, why, since) = match seen {
                Seen::Moving(how) => {
                    self.line(said(&how));
                    seen_on.extend(held.iter().map(ItemRef::display));
                    continue;
                }
                Seen::Waiting { how, since } => match idle(since) {
                    Some(_) => {
                        self.line(said(&format!("stage 2 moves it ({how})")));
                        seen_on.extend(held.iter().map(ItemRef::display));
                        continue;
                    }
                    // Its holds stall again: a wait this long is stuck.
                    None => {
                        let long = since
                            .map_or("since a time that does not read".into(), |since| {
                                format!("in {} hours", (self.now - since).whole_hours())
                            });
                        let stuck = format!("stage 2's step has not moved it {long} ({how})");
                        self.line(said(&stuck));
                        unseen_on.extend(held.iter().map(ItemRef::display));
                        continue;
                    }
                },
                Seen::Gone(why) | Seen::Unseen(why) => {
                    self.line(said(&why));
                    unseen_on.extend(held.iter().map(ItemRef::display));
                    continue;
                }
                Seen::Still { head, why, since } => (head, why, since),
            };
            seen_on.extend(held.iter().map(ItemRef::display));
            if let Some(idle) = idle(since) {
                let still = format!(
                    "{why}; it has stood still for {idle} hour(s) of the {hours} before a card"
                );
                self.line(said(&still));
                continue;
            }
            // The board's first Priority among the items it holds.
            let priority = held
                .iter()
                .filter_map(|item| board.items.get(&key(item))?.priority.clone())
                .min_by_key(|p| {
                    board
                        .options
                        .iter()
                        .position(|o| o == p)
                        .unwrap_or(usize::MAX)
                });
            let head: String = head.chars().take(HEAD_CHARS).collect();
            // The items it holds are said, but kept out of the reason its
            // card's `since` goes by, so that one more held item is no new
            // card.
            let reason = format!("only a person moves it at {head} ({why})");
            let card = format!(
                "blocker {}: {reason}; it holds {items} by overlap",
                pull.display()
            );
            self.line(card);
            self.report.holds.push(Entry {
                scope: Scope::Item(pull),
                hold: Hold::new(HoldKind::Blocking, reason),
                priority,
                why: None,
            });
        }
        self.blocked = &seen_on - &unseen_on;
    }

    /// What `pull` does this pass: stage 2's step where it follows its
    /// repository, else its own read, which spends one of `reads`.
    fn seen(&self, pull: &ItemRef, reads: &mut usize) -> Seen {
        let followed =
            (self.config.follow.iter()).any(|r| r.eq_ignore_ascii_case(&pull.repository));
        if followed {
            let step = self.stepped.iter().find(|s| {
                s.number == pull.number && s.repository.eq_ignore_ascii_case(&pull.repository)
            });
            return match step {
                Some(step) => stepped(step),
                None => Seen::Unseen("stage 2 decided no step for it this pass".into()),
            };
        }
        let bounds = self.client.bounds;
        if *reads == 0 {
            return Seen::Unseen(format!(
                "not read: past the {BLOCKER_READS} blockers read a pass"
            ));
        }
        if bounds.left.get() <= BLOCKER_RESERVE || Instant::now() > bounds.admit_by {
            return Seen::Unseen("not read: the pass's request budget or time is low".into());
        }
        *reads -= 1;
        let read = overlap::owner_name(pull).and_then(|(owner, name)| {
            let variables = json!({"owner": owner, "name": name, "number": pull.number});
            let data = self.client.graphql(PULL_QUERY, variables)?;
            data.pointer("/repository/pullRequest")
                .filter(|n| n.is_object())
                .cloned()
                .context("it is not a pull request")
        });
        match read {
            Ok(node) => unfollowed(&node, &pull.repository),
            Err(error) => Seen::Unseen(format!("could not be read: {error:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_blocker_does() {
        let at = |stamp: &str| OffsetDateTime::parse(stamp, &Rfc3339).ok();
        let step = |action: &'static str, reason: &str, ran: bool| Stepped {
            repository: "o/r".into(),
            number: 5,
            head: "abc".into(),
            action,
            reason: reason.into(),
            ran,
            moved_at: at("2026-10-20T00:00:00Z"),
        };
        let still = |why: &str| Seen::Still {
            head: "abc".into(),
            why: why.into(),
            since: at("2026-10-20T00:00:00Z"),
        };
        let waiting = |how: &str| Seen::Waiting {
            how: how.into(),
            since: at("2026-10-20T00:00:00Z"),
        };
        for (step, seen) in [
            (
                step("fix", "conflicts with main", true),
                Seen::Moving("stage 2 moves it (fix: conflicts with main)".into()),
            ),
            (
                step("fix", "conflicts with main", false),
                Seen::Unseen(
                    "stage 2's step did not run this pass (fix: conflicts with main)".into(),
                ),
            ),
            (
                step("wait", "fix round 1 is running for abc", false),
                waiting("wait: fix round 1 is running for abc"),
            ),
            (
                step("wait", "no approving review of the head yet", false),
                still("wait: no approving review of the head yet, and no review of it has come"),
            ),
            (
                step(
                    "wait",
                    "approved before its base became main; the head needs a review against it",
                    false,
                ),
                still(
                    "wait: approved before its base became main; the head needs a review against it",
                ),
            ),
            (
                step("needs-human", "2 fix rounds used", true),
                still("needs-human: 2 fix rounds used"),
            ),
            (
                step("skip", "labelled needs-human", false),
                still("skip: labelled needs-human"),
            ),
            (
                step("skip", "auto-merge already on", false),
                waiting("skip: auto-merge already on"),
            ),
        ] {
            assert_eq!(stepped(&step), seen, "{step:?}");
        }
        let node = |change: Value| {
            let mut node = json!({"state": "OPEN", "isDraft": false, "mergeable": "MERGEABLE",
                "headRefOid": "abc", "createdAt": "2026-10-17T00:00:00Z", "reviewDecision": null,
                "commits": {"nodes": [{"commit": {"committedDate": "2026-10-18T00:00:00Z"}}]},
                "reviews": {"nodes": []}});
            for (key, value) in change.as_object().unwrap() {
                node[key] = value.clone();
            }
            node
        };
        let still = |why: &str, since: Option<&str>| Seen::Still {
            head: "abc".into(),
            why: format!("{why}, and the pass does not follow o/r"),
            since: since.and_then(at),
        };
        let reviewed = json!({"nodes": [{"submittedAt": "2026-10-19T00:00:00Z"}]});
        for (change, seen) in [
            (
                json!({}),
                still("it has no review", Some("2026-10-18T00:00:00Z")),
            ),
            (
                json!({"mergeable": "CONFLICTING", "reviews": reviewed}),
                still("it conflicts with its base", Some("2026-10-19T00:00:00Z")),
            ),
            (
                json!({"reviewDecision": "CHANGES_REQUESTED", "reviews": reviewed}),
                still("changes were requested", Some("2026-10-19T00:00:00Z")),
            ),
            (
                json!({"reviewDecision": "APPROVED", "reviews": reviewed}),
                still(
                    "it is approved and waits to be merged",
                    Some("2026-10-19T00:00:00Z"),
                ),
            ),
            (
                json!({"reviewDecision": "REVIEW_REQUIRED", "reviews": reviewed}),
                still("it waits for a review", Some("2026-10-19T00:00:00Z")),
            ),
            (
                json!({"isDraft": true}),
                still("it is a draft", Some("2026-10-18T00:00:00Z")),
            ),
            // An age that does not read is unknown, never young.
            (json!({"createdAt": null}), still("it has no review", None)),
            (
                json!({"state": "MERGED"}),
                Seen::Gone("it is merged now".into()),
            ),
        ] {
            assert_eq!(unfollowed(&node(change.clone()), "o/r"), seen, "{change}");
        }
    }
}
