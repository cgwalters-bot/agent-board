//! Epics on a Forgejo board (docs/forges.md#epics). Forgejo has no
//! sub-issues, so parentage is two relationship comments by the bot that
//! must agree: the epic's newest `<!-- agent-board-epic-children: N -->`
//! listing the child, and the child's newest
//! `<!-- agent-board-epic-parent: OWNER/REPO#N -->` naming the epic. A person
//! editing one issue cannot attach work to an epic, and dependency links keep
//! their meaning, blocking.
use super::{text, unedited};
use crate::epic::{EPIC_LABEL, IssueRef};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// The first line of an epic's children comment, naming the epic's number.
pub const CHILDREN_MARKER: &str = "agent-board-epic-children";
/// The first line of a child's parent comment, naming its epic.
pub const PARENT_MARKER: &str = "agent-board-epic-parent";
/// The most children one epic lists; more is unreadable. Each child costs
/// two reads a pass (its issue and its comments).
pub const CHILD_LIMIT: usize = 50;
/// A timeline `label` event's body when it added the label, not removed it.
const LABEL_ADDED: &str = "1";

fn line(marker: &str, value: impl std::fmt::Display) -> String {
    format!("<!-- {marker}: {value} -->")
}

/// The epic's children comment listing `children`.
pub fn children_comment(epic: u64, children: &[IssueRef]) -> String {
    let names: Vec<String> = children.iter().map(ToString::to_string).collect();
    format!(
        "{}\n{}\n\nThe sub-issues of this epic, as the board pass records them.",
        line(CHILDREN_MARKER, epic),
        json!(names)
    )
}

/// A child's parent comment naming `epic`.
pub fn parent_comment(epic: &IssueRef) -> String {
    format!(
        "{}\nA sub-issue of the epic {epic}, as the board pass records it.",
        line(PARENT_MARKER, epic)
    )
}

/// The rest of the newest comment by `bot` whose first line is `marker`'s
/// with its value, and that value: the newest wins, never an older one. An
/// edited one is an error, as for every record.
fn newest<'c>(comments: &'c [Value], bot: u64, marker: &str) -> Result<Option<(&'c str, &'c str)>> {
    let prefix = format!("<!-- {marker}: ");
    let newest = comments
        .iter()
        .filter(|c| c["user"]["id"].as_u64() == Some(bot))
        .filter(|c| c["body"].as_str().is_some_and(|b| b.starts_with(&prefix)))
        .max_by_key(|c| c["id"].as_u64());
    let Some(comment) = newest else {
        return Ok(None);
    };
    ensure!(
        unedited(comment),
        "its newest {marker} comment was edited; a person inspects it"
    );
    let body = text(comment, "body")?;
    let (head, rest) = body.split_once('\n').unwrap_or((body, ""));
    let value = head
        .trim_end()
        .strip_prefix(&prefix)
        .and_then(|v| v.strip_suffix(" -->"))
        .with_context(|| format!("its newest {marker} comment cannot be read"))?;
    Ok(Some((value, rest)))
}

/// The children the epic `epic` lists in its newest children comment by
/// `bot`, none without one. One that cannot be read is an error.
pub fn children(comments: &[Value], bot: u64, epic: u64) -> Result<Vec<IssueRef>> {
    let Some((number, rest)) = newest(comments, bot, CHILDREN_MARKER)? else {
        return Ok(Vec::new());
    };
    ensure!(
        number == epic.to_string(),
        "its newest {CHILDREN_MARKER} comment names another epic"
    );
    let names: Vec<String> = rest
        .lines()
        .next()
        .and_then(|l| serde_json::from_str(l).ok())
        .with_context(|| format!("its newest {CHILDREN_MARKER} comment has no list"))?;
    ensure!(
        names.len() <= CHILD_LIMIT,
        "it lists more than {CHILD_LIMIT} children"
    );
    let mut seen = BTreeSet::new();
    names
        .iter()
        .map(|name| {
            let child = IssueRef::parse(name)?;
            ensure!(
                seen.insert(name.to_ascii_lowercase()),
                "it lists {name} twice"
            );
            Ok(child)
        })
        .collect()
}

/// The epic a child's newest parent comment by `bot` names, if any.
pub fn parent(comments: &[Value], bot: u64) -> Result<Option<IssueRef>> {
    newest(comments, bot, PARENT_MARKER)?
        .map(|(epic, _)| IssueRef::parse(epic))
        .transpose()
}

/// Who made the newest `epic` label event of an issue's whole timeline,
/// where that event added the label: the id the epic's liveness rests on.
pub fn labelled_by(timeline: &[Value]) -> Option<u64> {
    let newest = timeline
        .iter()
        .filter(|e| e["type"] == "label")
        .filter(|e| {
            e["label"]["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(EPIC_LABEL))
        })
        .max_by_key(|e| e["id"].as_u64())?;
    (newest["body"] == LABEL_ADDED)
        .then(|| newest["user"]["id"].as_u64())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(id: u64, user: u64, body: &str, edited: bool) -> Value {
        let updated = if edited {
            "2026-10-23T15:16:00Z"
        } else {
            "2026-10-23T15:15:10Z"
        };
        json!({"id": id, "user": {"id": user}, "body": body,
            "created_at": "2026-10-23T15:15:10Z", "updated_at": updated})
    }

    #[test]
    fn relationship_comments() {
        let one = IssueRef::parse("example/work#7").unwrap();
        let epic = IssueRef::parse("example/plans#3").unwrap();
        let listed = children_comment(3, std::slice::from_ref(&one));
        let parent_line = parent_comment(&epic);
        type Case<'a> = (&'a str, Vec<Value>, Result<Vec<IssueRef>, &'a str>);
        let cases: Vec<Case> = vec![
            ("none", vec![], Ok(vec![])),
            (
                "listed",
                vec![comment(1, 9, &listed, false)],
                Ok(vec![one.clone()]),
            ),
            (
                "a visitor's newer list is not the bot's",
                vec![
                    comment(1, 9, &listed, false),
                    comment(2, 5, &children_comment(3, &[]), false),
                ],
                Ok(vec![one.clone()]),
            ),
            (
                "newest wins, even empty",
                vec![
                    comment(1, 9, &listed, false),
                    comment(2, 9, &children_comment(3, &[]), false),
                ],
                Ok(vec![]),
            ),
            (
                "edited newest never falls back",
                vec![
                    comment(1, 9, &children_comment(3, &[]), false),
                    comment(2, 9, &listed, true),
                ],
                Err("edited"),
            ),
            (
                "another epic's list",
                vec![comment(
                    1,
                    9,
                    &children_comment(4, std::slice::from_ref(&one)),
                    false,
                )],
                Err("another epic"),
            ),
            (
                "no list",
                vec![comment(
                    1,
                    9,
                    "<!-- agent-board-epic-children: 3 -->\nnope",
                    false,
                )],
                Err("no list"),
            ),
            (
                "twice",
                vec![comment(
                    1,
                    9,
                    &children_comment(3, &[one.clone(), one.clone()]),
                    false,
                )],
                Err("twice"),
            ),
        ];
        for (name, comments, expected) in cases {
            match (children(&comments, 9, 3), expected) {
                (Ok(got), Ok(want)) => assert_eq!(got, want, "{name}"),
                (Err(error), Err(want)) => {
                    assert!(format!("{error:#}").contains(want), "{name}: {error:#}")
                }
                (got, want) => panic!("{name}: {got:?} != {want:?}"),
            }
        }
        let parents = [comment(1, 9, &parent_line, false)];
        assert_eq!(parent(&parents, 9).unwrap(), Some(epic.clone()));
        assert_eq!(parent(&parents, 5).unwrap(), None);
        assert!(parent(&[comment(1, 9, &parent_line, true)], 9).is_err());
    }

    #[test]
    fn newest_epic_label_event() {
        let event = |id: u64, user: u64, name: &str, body: &str| {
            json!({"id": id, "type": "label", "user": {"id": user},
                "label": {"name": name}, "body": body})
        };
        let other = json!({"id": 9, "type": "comment", "user": {"id": 5}});
        for (name, timeline, by) in [
            ("none", vec![], None),
            ("added", vec![event(1, 9, "epic", "1")], Some(9)),
            (
                "re-added by another",
                vec![event(1, 9, "epic", "1"), event(2, 5, "Epic", "1")],
                Some(5),
            ),
            (
                "removed last",
                vec![event(1, 9, "epic", "1"), event(2, 9, "epic", "")],
                None,
            ),
            (
                "other labels and events",
                vec![event(1, 9, "epic", "1"), event(3, 5, "bug", "1"), other],
                Some(9),
            ),
        ] {
            assert_eq!(labelled_by(&timeline), by, "{name}");
        }
    }
}
