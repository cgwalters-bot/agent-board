//! Read-only work inventory from the decisions the pass already made.

use super::{holds::Published, *};

const SCHEMA: &str = "agent-board/dispatchable-work/v1";
const MAX_ITEMS: usize = 200;

pub(super) fn snapshot(
    repositories: &[String],
    followed: Option<&(Vec<poll::Wait>, BTreeSet<String>)>,
    steps: &[poll::Stepped],
    plans: &BTreeMap<String, String>,
    holds: &[Published],
) -> Value {
    let mut repos = BTreeMap::new();
    let mut left = MAX_ITEMS;
    for repository in repositories
        .iter()
        .map(|r| r.to_ascii_lowercase())
        .collect::<BTreeSet<_>>()
    {
        let mut items = BTreeMap::new();
        for step in steps
            .iter()
            .filter(|s| s.repository.eq_ignore_ascii_case(&repository))
        {
            let actionable = matches!(step.action, "fix" | "rebase" | "continue" | "merge");
            items.insert(
                format!("{}#{}", repository, step.number),
                json!({
                    "kind": if actionable { step.action } else { "held" },
                    "reason": step.reason.chars().take(500).collect::<String>(),
                    "operator": step.progress() == poll::Progress::Person,
                    "head": step.head,
                    "taken": step.ran,
                }),
            );
        }
        for (item, kind) in plans {
            if item
                .rsplit_once('#')
                .is_some_and(|(r, _)| r.eq_ignore_ascii_case(&repository))
            {
                items.insert(
                    item.to_ascii_lowercase(),
                    json!({"kind":"accepted-plan", "plan_kind":kind, "operator":false}),
                );
            }
        }
        for hold in holds {
            if let Some(item) = &hold.item
                && item
                    .rsplit_once('#')
                    .is_some_and(|(r, _)| r.eq_ignore_ascii_case(&repository))
            {
                items.insert(
                    item.to_ascii_lowercase(),
                    json!({
                        "kind":"held", "hold_kind":hold.kind, "operator":hold.operator,
                        "reason":hold.reason.chars().take(500).collect::<String>(),
                        "carried":hold.carried,
                    }),
                );
            }
        }
        let mut counts = BTreeMap::<String, usize>::new();
        for item in items.values() {
            *counts.entry(text(item, "/kind").to_owned()).or_default() += 1;
        }
        let count = items.len();
        let included = count.min(left);
        left -= included;
        repos.insert(repository.clone(), json!({
            "reviews_complete": followed.is_some_and(|(_, unread)| !unread.contains(&repository)),
            // Scheduling can stop before reading plans at WIP, pool or request
            // limits. Never represent an unvisited issue as having no plan.
            "issues_complete":false,
            "count":count, "counts":counts, "omitted":count - included,
            "items":items.into_iter().take(included).map(|(item, mut row)| {
                row["item"] = item.into(); row
            }).collect::<Vec<_>>(),
        }));
    }
    json!({"schema":SCHEMA, "repositories":repos})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_work_counts_and_publication_are_bounded() {
        let step = |number, action, waiting| poll::Stepped {
            repository: "o/r".into(),
            number,
            head: "a".repeat(40),
            action,
            reason: "decision".into(),
            ran: false,
            moved_at: None,
            waiting,
        };
        let steps = vec![
            step(1, "fix", poll::WaitReason::Person),
            step(2, "rebase", poll::WaitReason::Person),
            step(3, "continue", poll::WaitReason::Person),
            step(4, "merge", poll::WaitReason::Person),
            step(5, "wait", poll::WaitReason::Person),
        ];
        let plans = BTreeMap::from([("o/other#8".into(), "implement".into())]);
        let followed = (vec![], BTreeSet::from(["o/other".into()]));
        let work = snapshot(
            &["o/r".into(), "o/other".into()],
            Some(&followed),
            &steps,
            &plans,
            &[],
        );
        assert_eq!(
            work["repositories"]["o/r"]["counts"],
            json!({"fix":1,"rebase":1,"continue":1,"merge":1,"held":1})
        );
        assert_eq!(work["repositories"]["o/r"]["reviews_complete"], true);
        assert_eq!(work["repositories"]["o/other"]["reviews_complete"], false);
        assert_eq!(
            work["repositories"]["o/other"]["counts"]["accepted-plan"],
            1
        );
        let mut document = holds::snapshot(&[], "2026-11-05T12:00:00Z".into(), Default::default());
        document.dispatchable_work = Some(work.clone());
        let (_, body) = holds::fit(document.clone()).unwrap();
        assert_eq!(holds::published(&body).unwrap()["dispatchable_work"], work);
        assert!(!holds::changed(Some(&body), &document).unwrap());

        let many = (1..=MAX_ITEMS + 5)
            .map(|n| step(n as u64, "fix", poll::WaitReason::Person))
            .collect::<Vec<_>>();
        let work = snapshot(&["o/r".into()], None, &many, &BTreeMap::new(), &[]);
        assert_eq!(work["repositories"]["o/r"]["count"], MAX_ITEMS + 5);
        assert_eq!(work["repositories"]["o/r"]["omitted"], 5);
        assert_eq!(
            work["repositories"]["o/r"]["items"]
                .as_array()
                .unwrap()
                .len(),
            MAX_ITEMS
        );
        document.dispatchable_work = Some(json!({"oversized":"x".repeat(100_000)}));
        let (fitted, _) = holds::fit(document).unwrap();
        assert!(fitted.dispatchable_work.is_none());
    }
}
