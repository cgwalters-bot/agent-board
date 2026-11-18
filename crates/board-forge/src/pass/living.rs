//! One bot-owned status comment. Machine records stay in the durable local ledger.
use super::*;
use crate::epic_apply;
use serde::Serialize;
use std::path::{Path, PathBuf};

pub(crate) const MARKER: &str = "<!-- agent-board-status v1 -->";
const ORIGINAL_MARKER: &str = "<!-- agent-board-status -->";
pub(super) const RECORDS: &str = "<!-- agent-board-status-records\n";
const STATE: &str = "<!-- agent-board-status-state: ";
const COMMENT_BYTES: usize = 60_000;
pub(super) const MINIMIZE: &str = "mutation($id: ID!) { minimizeComment(input: {subjectId: $id, classifier: OUTDATED}) { clientMutationId } }";
pub(super) const MINIMIZED: &str =
    "query($id: ID!) { node(id: $id) { ... on IssueComment { isMinimized } } }";
pub(super) const HISTORY_STATE: &str = "query($id: ID!) { node(id: $id) { ... on IssueComment { url body createdAt lastEditedAt author { login } editor { login } isMinimized } } }";
const SCHEMA: &str = "agent-board/status/v1";
const MAX_RECORDS: usize = 1000;
const MAX_BYTES: u64 = 4 * 1024 * 1024;
const DISPLAY_CHARS: usize = 2000;
const SECTIONS: &[&str] = &[
    "Current plan",
    "Last dispatch",
    "Last apply",
    "Last triage outcome",
];

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: Option<u64>,
    records: Vec<Value>,
    published: Option<String>,
    #[serde(default)]
    prose: BTreeMap<String, String>,
    // Last validated reference per public comment, including minimized duplicates.
    // An empty value denotes a known legacy status without a state reference.
    // Those comments remain readable after the authoritative status advances.
    #[serde(default)]
    references: BTreeMap<u64, String>,
    // A first POST without a response can still become visible after a
    // complete scan and a successful retry. Keep its reference independently
    // of the retry's ID and later revisions, never as evidence of work.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    uncertain_first_posts: BTreeSet<String>,
    #[serde(default)]
    pulls: Option<Vec<pull_links::PullLink>>,
    // A confirmed minimization stays done even if a person later restores it.
    #[serde(default)]
    minimized: BTreeSet<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://api.github.com/repos/o/r/issues/1/comments";

    fn at() -> OffsetDateTime {
        OffsetDateTime::parse(
            "2026-11-01T00:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap()
    }

    fn dispatched(id: u64) -> Value {
        json!({"body": format!("<!-- agent-board-dispatch: {id}@time -->\nDispatched [plan](https://github.com/o/r/issues/1#issuecomment-{id}): [run](https://github.com/o/r/actions/runs/8).")})
    }

    #[test]
    fn pull_inventory_creates_one_status_and_survives_restart_and_plan_updates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let path = path.to_str().unwrap();
        let issue = crate::github::parse_github_issue("o/r#1", None, DEFAULT_HOST).unwrap();
        let link = |state| {
            serde_json::from_value(json!({"repository":"up/r","number":8,"state":state})).unwrap()
        };
        let mut store = Store::load(path, "bot", at()).unwrap();
        store.track_pulls(&issue, vec![link("OPEN")]).unwrap();
        let write = store.pending().pop().unwrap();
        assert_eq!(write.method, "POST");
        assert!(text(&write.body, "/body").contains("https://github.com/up/r/pull/8 (open)"));
        store.finish(&write, &mut (201, json!({"id":30}))).unwrap();
        let mut store = Store::load(path, "bot", at()).unwrap();
        store.track_pulls(&issue, vec![link("OPEN")]).unwrap();
        assert!(store.pending().is_empty());
        let write = store
            .prepare("POST", URL, &dispatched(10))
            .unwrap()
            .unwrap();
        assert_eq!(write.method, "PATCH");
        assert!(text(&write.body, "/body").contains("### Pull requests"));
        store.finish(&write, &mut (200, json!({}))).unwrap();
        store.track_pulls(&issue, vec![link("MERGED")]).unwrap();
        let write = store.pending().pop().unwrap();
        assert_eq!(write.method, "PATCH");
        assert!(text(&write.body, "/body").contains("/pull/8 (merged)"));
        store.finish(&write, &mut (200, json!({}))).unwrap();
        store.track_pulls(&issue, vec![]).unwrap();
        assert!(text(&store.pending()[0].body, "/body").contains("None observed."));
    }

    fn legacy_body(entry: &Entry) -> String {
        format!(
            "{MARKER}\n{RECORDS}{}\n-->",
            serde_json::to_string(&entry.records).unwrap()
        )
    }

    #[test]
    fn machine_records_stay_local_and_missing_or_stale_state_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let path = path.to_str().unwrap();
        let mut store = Store::load(path, "bot", at()).unwrap();
        let stale = store.ledger.entries.clone();
        let body = json!({"body":format!("Applied updates.\n```{}\n{{\"plan\":\"secret-plan\",\"actions\":[\"closed issue 99\"]}}\n```", epic_apply::APPLY_MARKER)});
        let write = store.prepare("POST", URL, &body).unwrap().unwrap();
        let public = text(&write.body, "/body").to_owned();
        assert!(public.contains("Applied updates."));
        assert!(!public.contains("secret-plan"));
        assert!(!public.contains("closed issue"));
        assert!(!public.contains(RECORDS));
        assert_eq!(state_reference(&public).unwrap().len(), 64);
        store.finish(&write, &mut (201, json!({"id":30}))).unwrap();
        let vars = json!({"owner":"o","name":"r","number":1});
        let data = json!({"repository":{"issue":{"comments":{"nodes":[{
            "body":public,"url":"https://github.com/o/r/issues/1#issuecomment-30",
            "author":{"login":"bot"},"lastEditedAt":null
        }]}}}});
        let mut restored = Store::load(path, "bot", at()).unwrap();
        restored.expand(&vars, &mut data.clone()).unwrap();
        assert!(restored.pending().is_empty());
        assert_eq!(restored.ledger.entries["o/r#1"].records.len(), 1);

        for entries in [stale, BTreeMap::from([("o/r#1".into(), Entry::default())])] {
            restored.ledger.entries = entries.clone();
            let error = restored.expand(&vars, &mut data.clone()).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("restore its durable livingStatus file")
            );
            assert_eq!(restored.ledger.entries.len(), entries.len());
        }
    }

    #[test]
    fn legacy_records_migrate_to_local_state_without_losing_replay_guards() {
        let dir = tempfile::tempdir().unwrap();
        let mut source =
            Store::load(dir.path().join("old.json").to_str().unwrap(), "bot", at()).unwrap();
        let write = source
            .prepare("POST", URL, &dispatched(10))
            .unwrap()
            .unwrap();
        source.finish(&write, &mut (201, json!({"id":30}))).unwrap();
        let body = legacy_body(&source.ledger.entries["o/r#1"]);
        let mut target =
            Store::load(dir.path().join("new.json").to_str().unwrap(), "bot", at()).unwrap();
        let mut data = json!({"repository":{"issue":{"comments":{"nodes":[{
            "body":body,"url":"https://github.com/o/r/issues/1#issuecomment-30",
            "author":{"login":"bot"},"lastEditedAt":null
        }]}}}});
        target
            .expand(&json!({"owner":"o","name":"r","number":1}), &mut data)
            .unwrap();
        assert_eq!(
            target.ledger.entries["o/r#1"].records,
            source.ledger.entries["o/r#1"].records
        );
        let write = target.pending().pop().unwrap();
        assert_eq!(write.method, "PATCH");
        assert!(!text(&write.body, "/body").contains(RECORDS));
        target.acknowledge(&write).unwrap();
        assert!(target.pending().is_empty());
    }

    #[test]
    fn successive_dispatches_edit_one_status_and_keep_both_claims() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let path = path.to_str().unwrap();
        let mut store = Store::load(path, "bot", at()).unwrap();
        for (id, expected) in [(10, "POST"), (20, "PATCH")] {
            let write = store
                .prepare("POST", URL, &dispatched(id))
                .unwrap()
                .unwrap();
            assert_eq!(write.method, expected);
            assert!(
                !write.body["body"]
                    .as_str()
                    .unwrap()
                    .split(RECORDS)
                    .next()
                    .unwrap()
                    .contains("agent-board-dispatch")
            );
            store.finish(&write, &mut (200, json!({"id": 30}))).unwrap();
            store = Store::load(path, "bot", at()).unwrap();
        }
        let variables = json!({"owner":"o", "name":"r", "number":1, "after":null});
        let mut data = json!({"repository":{"issue":{"comments":{"nodes":[]}}}});
        store.expand(&variables, &mut data).unwrap();
        let nodes = data
            .pointer("/repository/issue/comments/nodes")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(nodes.len(), 2);
        assert!(nodes[0]["body"].as_str().unwrap().contains("10@time"));
        assert!(nodes[1]["body"].as_str().unwrap().contains("20@time"));
        let mut second_page = json!({"repository":{"issue":{"comments":{"nodes":[]}}}});
        store
            .expand(
                &json!({"after":"cursor", "owner":"o", "name":"r", "number":1}),
                &mut second_page,
            )
            .unwrap();
        assert_eq!(
            second_page
                .pointer("/repository/issue/comments/nodes")
                .unwrap(),
            &json!([])
        );
    }

    #[test]
    fn cold_start_edits_existing_marker_and_foreign_edits_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let mut store = Store::load(path.to_str().unwrap(), "bot", at()).unwrap();
        let variables = json!({"owner":"o", "name":"r", "number":1});
        let mut data = json!({"repository":{"issue":{"comments":{"nodes":[{
            "body":MARKER,"url":"https://github.com/o/r/issues/1#issuecomment-30",
            "author":{"login":"bot"},"lastEditedAt":null
        }]}}}});
        let original = data.clone();
        store.expand(&variables, &mut data).unwrap();
        let write = store
            .prepare("POST", URL, &dispatched(10))
            .unwrap()
            .unwrap();
        assert_eq!(write.method, "PATCH");
        assert!(write.url.ends_with("/issues/comments/30"));
        store.finish(&write, &mut (201, json!({"id":30}))).unwrap();
        data = original;
        data["repository"]["issue"]["comments"]["nodes"][0]["lastEditedAt"] =
            "2026-11-01T01:00:00Z".into();
        assert!(store.expand(&variables, &mut data).is_err());
    }

    #[test]
    fn stale_cache_and_paginated_duplicates_use_first_bot_marker() {
        let dir = tempfile::tempdir().unwrap();
        let mut store =
            Store::load(dir.path().join("cache.json").to_str().unwrap(), "bot", at()).unwrap();
        let write = store
            .prepare("POST", URL, &dispatched(10))
            .unwrap()
            .unwrap();
        store.finish(&write, &mut (201, json!({"id":1}))).unwrap();
        let page = |id, bot, marker, section| {
            json!({"repository":{"issue":{"comments":{
                "nodes":[{"body":format!("{marker}\n## Board status\n\n### {section}\nKept prose.\n"),
                    "url":format!("https://github.com/o/r/issues/1#issuecomment-{id}"),
                    "author":{"login":bot},"lastEditedAt":null}],
                "pageInfo":{"hasNextPage":true,"endCursor":"next"}
            }}}})
        };
        let vars = json!({"owner":"o","name":"r","number":1,"after":null});
        store
            .expand(&vars, &mut page(2, "someone", MARKER, "Current plan"))
            .unwrap();
        let later = json!({"owner":"o","name":"r","number":1,"after":"next"});
        store
            .expand(
                &later,
                &mut page(30, "bot", ORIGINAL_MARKER, "Current plan"),
            )
            .unwrap();
        store
            .expand(&later, &mut page(40, "bot", MARKER, "Last apply"))
            .unwrap();
        assert_eq!(store.ledger.entries["o/r#1"].id, Some(30));
        assert!(
            store.duplicates().is_empty(),
            "merge must be published first"
        );
        let write = store.pending().pop().unwrap();
        assert!(text(&write.body, "/body").contains("### Current plan"));
        assert!(text(&write.body, "/body").contains("### Last apply"));
        store.acknowledge(&write).unwrap();
        assert_eq!(store.duplicates(), vec![("o/r#1".into(), 40)]);
    }

    #[test]
    fn duplicate_relocation_preserves_plan_identity_and_routes_edits_to_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut source = Store::load(
            dir.path().join("source.json").to_str().unwrap(),
            "bot",
            at(),
        )
        .unwrap();
        let plan = json!({"body":format!("Plan.\n```{}\n{{}}\n```", plan::PLAN_FENCE)});
        let write = source.prepare("POST", URL, &plan).unwrap().unwrap();
        source.finish(&write, &mut (201, json!({"id":40}))).unwrap();
        let logical = format!("40@{}", source.at);
        let dispatch =
            json!({"body":format!("<!-- agent-board-dispatch: {logical} -->\nDispatched.")});
        let write = source.prepare("POST", URL, &dispatch).unwrap().unwrap();
        source.finish(&write, &mut (200, json!({}))).unwrap();
        let mut target = Store::load(
            dir.path().join("target.json").to_str().unwrap(),
            "bot",
            at(),
        )
        .unwrap();
        let node = |id, body: &str| json!({"url":format!("https://github.com/o/r/issues/1#issuecomment-{id}"),"body":body,"author":{"login":"bot"},"lastEditedAt":null});
        let mut data = json!({"repository":{"issue":{"comments":{"nodes":[node(30, MARKER),node(40, &legacy_body(&source.ledger.entries["o/r#1"]))]}}}});
        target
            .expand(&json!({"owner":"o","name":"r","number":1}), &mut data)
            .unwrap();
        let records: Vec<_> = target.ledger.entries["o/r#1"]
            .records
            .iter()
            .map(|r| plan::comment(r).unwrap())
            .collect();
        assert_eq!(plan::plan_id(&records[0]).unwrap(), logical);
        assert_eq!(plan::records(&records, &["bot"], &logical).len(), 1);
        let write = target
            .prepare(
                "PATCH",
                "https://api.github.com/repos/o/r/issues/comments/40",
                &plan,
            )
            .unwrap()
            .unwrap();
        assert!(write.url.ends_with("/comments/30"));
        let before = target.ledger.entries["o/r#1"].records.len();
        target.prepare("POST", URL, &json!({"body":format!("{}\n```{}\n{{}}\n```", "x".repeat(COMMENT_BYTES), plan::PLAN_FENCE)})).unwrap();
        assert_eq!(target.ledger.entries["o/r#1"].records.len(), before + 1);
        assert!(
            target
                .pending()
                .iter()
                .all(|w| text(&w.body, "/body").len() <= COMMENT_BYTES)
        );
    }

    #[test]
    fn changed_plan_edits_status_without_overwriting_dispatch_or_growing_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let mut store = Store::load(path.to_str().unwrap(), "bot", at()).unwrap();
        let plan =
            |prose: &str| json!({"body": format!("{prose}\n```{}\n{{}}\n```", plan::PLAN_FENCE)});
        let write = store
            .prepare("POST", URL, &plan("First plan."))
            .unwrap()
            .unwrap();
        store.finish(&write, &mut (201, json!({"id":30}))).unwrap();
        let write = store
            .prepare("POST", URL, &dispatched(10))
            .unwrap()
            .unwrap();
        store.finish(&write, &mut (200, json!({}))).unwrap();
        let write = store
            .prepare(
                "PATCH",
                "https://api.github.com/repos/o/r/issues/comments/30",
                &plan("Changed plan."),
            )
            .unwrap()
            .unwrap();
        assert_eq!(write.method, "PATCH");
        let body = text(&write.body, "/body");
        assert!(body.contains("Changed plan."), "{body}");
        assert!(body.contains("### Last dispatch"), "{body}");
        assert!(!body.contains("First plan."), "{body}");
        assert_eq!(store.ledger.entries["o/r#1"].records.len(), 2);
        let records = &mut store.ledger.entries.get_mut("o/r#1").unwrap().records;
        records.resize(MAX_RECORDS, records[1].clone());
        assert!(store.prepare("POST", URL, &dispatched(20)).is_err());
    }

    #[test]
    fn full_ledger_refusal_does_not_poison_later_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let path = path.to_str().unwrap();
        let mut store = Store::load(path, "bot", at()).unwrap();
        let write = store
            .prepare("POST", URL, &dispatched(10))
            .unwrap()
            .unwrap();
        store.finish(&write, &mut (201, json!({"id":30}))).unwrap();
        let records = &mut store.ledger.entries.get_mut("o/r#1").unwrap().records;
        records.resize(MAX_RECORDS, records[0].clone());
        store.save().unwrap();

        let error = store
            .prepare(
                "POST",
                "https://api.github.com/repos/o/r/issues/2/comments",
                &dispatched(20),
            )
            .err()
            .unwrap();
        assert!(error.to_string().contains("status ledger is full"));
        let write = store
            .prepare(
                "PATCH",
                "https://api.github.com/repos/o/r/issues/comments/30",
                &dispatched(10),
            )
            .unwrap()
            .unwrap();
        store.finish(&write, &mut (200, json!({}))).unwrap();

        let store = Store::load(path, "bot", at()).unwrap();
        assert_eq!(store.ledger.entries.len(), 1);
        assert_eq!(store.ledger.entries["o/r#1"].records.len(), MAX_RECORDS);
        assert!(store.pending().is_empty());
    }

    #[test]
    fn release_targets_latest_dispatch_and_responses_use_logical_plan_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let mut store = Store::load(path.to_str().unwrap(), "bot", at()).unwrap();
        for id in [10, 20] {
            let write = store
                .prepare("POST", URL, &dispatched(id))
                .unwrap()
                .unwrap();
            let mut response = (200, json!({"id":30,"created_at":"2020-01-01T00:00:00Z"}));
            store.finish(&write, &mut response).unwrap();
            assert_eq!(response.1["created_at"], store.at);
            let write = store.prepare("PATCH", "https://api.github.com/repos/o/r/issues/comments/30", &json!({"body":"The plan in URL was not dispatched. A later dispatch may take it."})).unwrap().unwrap();
            store.finish(&write, &mut (200, json!({}))).unwrap();
        }
        assert!(
            store.ledger.entries["o/r#1"]
                .records
                .iter()
                .all(|r| kind(text(r, "/body")).is_none())
        );
        let write = store
            .prepare("POST", URL, &dispatched(20))
            .unwrap()
            .unwrap();
        assert_eq!(write.method, "PATCH");
        assert!(text(&write.body, "/body").contains("### Last dispatch"));
    }

    #[test]
    fn failed_public_edit_remains_pending_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let path = path.to_str().unwrap();
        let mut store = Store::load(path, "bot", at()).unwrap();
        let write = store
            .prepare("POST", URL, &dispatched(10))
            .unwrap()
            .unwrap();
        store.finish(&write, &mut (201, json!({"id":30}))).unwrap();
        assert!(store.pending().is_empty());
        let write = store
            .prepare("POST", URL, &dispatched(20))
            .unwrap()
            .unwrap();
        store.finish(&write, &mut (500, json!({}))).unwrap();
        let mut store = Store::load(path, "bot", at()).unwrap();
        let pending = store.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].method, "PATCH");
        store.acknowledge(&pending[0]).unwrap();
        assert!(Store::load(path, "bot", at()).unwrap().pending().is_empty());
        assert_eq!(store.ledger.entries["o/r#1"].records.len(), 2);
    }

    #[test]
    fn first_living_status_post_recovers_without_appending_or_reposting() {
        // An epic apply that lost its claim's reply never wrote; its claim
        // must not block the retry.
        let claim = format!(
            "<!-- {0}: p -->\nApplying.\n```{0}\n{{\"plan\":\"p\"}}\n```",
            epic_apply::APPLY_MARKER
        );
        let cases = [
            (json!({"body":claim}), false),
            (
                json!({"body":format!("{}\nWaiting for a decision.", hold_marker(10))}),
                true,
            ),
        ];
        for ((receipt, kept), landed) in cases
            .iter()
            .flat_map(|case| [false, true].map(|landed| (case, landed)))
        {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ledger.json");
            let path = path.to_str().unwrap();
            let mut store = Store::load(path, "bot", at()).unwrap();
            let write = store.prepare("POST", URL, receipt).unwrap().unwrap();
            let body = text(&write.body, "/body");
            let url = "https://github.com/o/r/issues/1#issuecomment-30";
            let nodes = if landed {
                // GitHub committed the POST but the response was lost.
                json!([{"body":body,"url":url,"author":{"login":"bot"},
                    "lastEditedAt":null,"editor":null,"createdAt":store.at}])
            } else {
                store.finish(&write, &mut (422, json!({}))).unwrap();
                json!([])
            };
            // The same pass's read may not show a POST that lost its reply.
            let mut empty = json!({"repository":{"issue":{"comments":{
                "nodes":[],"pageInfo":{"hasNextPage":false}
            }}}});
            store
                .expand(&json!({"owner":"o","name":"r","number":1}), &mut empty)
                .unwrap();
            assert_eq!(store.ledger.entries["o/r#1"].records.len(), 1);
            assert!(store.pending().is_empty());
            drop(store);

            let mut store = Store::load(path, "bot", at()).unwrap();
            assert_eq!(store.pending().len(), 1);
            assert!(store.prepare("POST", URL, receipt).is_err());
            let mut data = json!({"repository":{"issue":{"comments":{
                "nodes":nodes,"pageInfo":{"hasNextPage":false}
            }}}});
            store
                .expand(&json!({"owner":"o","name":"r","number":1}), &mut data)
                .unwrap();
            if !kept {
                let entry = &store.ledger.entries["o/r#1"];
                assert!(entry.records.is_empty());
                assert_eq!(entry.id, landed.then_some(30));
                assert!(store.pending().iter().all(|w| w.method == "PATCH"));
                continue;
            }
            if landed {
                assert_eq!(text(&data, "/repository/issue/comments/nodes/0/url"), url);
                assert!(store.pending().is_empty());
            } else {
                assert_eq!(data["repository"]["issue"]["comments"]["nodes"], json!([]));
                let retry = store.pending().pop().unwrap();
                assert_eq!(retry.method, "POST");
                assert_eq!(retry.body, write.body);
                store.finish(&retry, &mut (201, json!({"id":30}))).unwrap();
            }
            let store = Store::load(path, "bot", at()).unwrap();
            let entry = &store.ledger.entries["o/r#1"];
            assert_eq!(entry.id, Some(30));
            assert_eq!(entry.records.len(), 1);
            assert_eq!(text(&entry.records[0], "/url"), url);
            assert_eq!(text(&entry.records[0], "/body"), text(receipt, "/body"));
            assert!(store.pending().is_empty());
        }
    }

    #[test]
    fn first_status_claim_can_land_after_another_pass_saw_no_comment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let path = path.to_str().unwrap();
        let variables = json!({"owner":"o","name":"r","number":1});
        let page = |nodes| {
            json!({"repository":{"issue":{"comments":{
                "nodes":nodes,"pageInfo":{"hasNextPage":false}
            }}}})
        };
        let day = time::Duration::days(1);
        let mut store = Store::load(path, "bot", at()).unwrap();
        let lost = store
            .prepare("POST", URL, &dispatched(10))
            .unwrap()
            .unwrap();
        let delayed = json!({"body":lost.body["body"],
            "url":"https://github.com/o/r/issues/1#issuecomment-30","author":{"login":"bot"},
            "lastEditedAt":null,"editor":null,"createdAt":store.at});

        let mut store = Store::load(path, "bot", at() + day).unwrap();
        store.expand(&variables, &mut page(json!([]))).unwrap();
        let plan = json!({"body":format!("Current.\n```{}\n{{}}\n```", plan::PLAN_FENCE)});
        let retry = store.prepare("POST", URL, &plan).unwrap().unwrap();
        store.finish(&retry, &mut (201, json!({"id":31}))).unwrap();
        let visible = json!({"body":retry.body["body"],
            "url":"https://github.com/o/r/issues/1#issuecomment-31","author":{"login":"bot"},
            "lastEditedAt":null,"editor":null,"createdAt":store.at});

        let mut store = Store::load(path, "bot", at() + 2 * day).unwrap();
        let mut data = page(json!([delayed, visible]));
        store.expand(&variables, &mut data).unwrap();
        let entry = &store.ledger.entries["o/r#1"];
        assert_eq!(entry.id, Some(30));
        assert_eq!(entry.records.len(), 1);
        assert_eq!(entry.records[0]["body"], plan["body"]);
        assert_eq!(
            data["repository"]["issue"]["comments"]["nodes"][0]["body"],
            plan["body"]
        );
        assert!(!entry.prose.contains_key("Last dispatch"));
        assert_eq!(store.pending()[0].method, "PATCH");
    }

    #[test]
    fn first_hold_receipt_can_land_after_retry_and_a_new_status_revision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let path = path.to_str().unwrap();
        let vars = json!({"owner":"o","name":"r","number":1});
        let page = |nodes| {
            json!({"repository":{"issue":{"comments":{
                "nodes":nodes,"pageInfo":{"hasNextPage":false}
            }}}})
        };
        let node = |id, write: &Write| {
            json!({"body":write.body["body"],
            "url":format!("https://github.com/o/r/issues/1#issuecomment-{id}"),
            "author":{"login":"bot"},"lastEditedAt":null,"editor":null,
            "createdAt":"2026-11-01T00:00:00Z"})
        };
        let receipt = |id| json!({"body":format!("{}\nHold {id}.", hold_marker(id))});
        let day = time::Duration::days(1);
        let mut store = Store::load(path, "bot", at()).unwrap();
        let lost = store.prepare("POST", URL, &receipt(10)).unwrap().unwrap();
        let delayed = node(30, &lost);

        let mut store = Store::load(path, "bot", at() + day).unwrap();
        store.expand(&vars, &mut page(json!([]))).unwrap();
        let retry = store.pending().pop().unwrap();
        assert_eq!(retry.body, lost.body);
        store.finish(&retry, &mut (201, json!({"id":31}))).unwrap();
        let newer = store.prepare("POST", URL, &receipt(11)).unwrap().unwrap();
        store.finish(&newer, &mut (200, json!({"id":31}))).unwrap();
        let visible = node(31, &newer);
        store.expand(&vars, &mut page(json!([visible]))).unwrap();

        let mut store = Store::load(path, "bot", at() + 2 * day).unwrap();
        store
            .expand(&vars, &mut page(json!([delayed, visible])))
            .unwrap();
        let entry = &store.ledger.entries["o/r#1"];
        assert_eq!(entry.id, Some(30));
        assert_eq!(entry.records.len(), 2);
        let pending = store.pending().pop().unwrap();
        assert_eq!(pending.method, "PATCH");
        assert!(text(&pending.body, "/body").contains("Hold 11."));
        assert!(!text(&pending.body, "/body").contains("Hold 10."));
    }

    #[test]
    fn replacing_a_legacy_status_preserves_its_dispatch_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let issue = crate::github::parse_github_issue("o/r#1", None, DEFAULT_HOST).unwrap();
        let mut store = Store::load(path.to_str().unwrap(), "bot", at()).unwrap();
        let write = store
            .prepare("POST", URL, &dispatched(10))
            .unwrap()
            .unwrap();
        store.finish(&write, &mut (201, json!({"id":30}))).unwrap();
        let entry = store.ledger.entries.get_mut("o/r#1").unwrap();
        entry.references.clear(); // Older ledgers only stored the current ID.
        let record = plan::comment(&entry.records[0]).unwrap();
        let mut data = json!({"repository":{"issue":{"comments":{
            "nodes":[{"body":MARKER,"url":"https://github.com/o/r/issues/1#issuecomment-40",
                "author":{"login":"bot"},"createdAt":store.at,"lastEditedAt":null,"editor":null}],
            "pageInfo":{"hasNextPage":false}
        }}}});
        store
            .expand(&json!({"owner":"o","name":"r","number":1}), &mut data)
            .unwrap();
        assert_eq!(store.ledger.entries["o/r#1"].id, Some(40));
        assert_eq!(store.dispatch_origin(&issue, &record), (true, false));
        // The ledger keeps GitHub's case; the scheduler's key is lowercase.
        let entry = store.ledger.entries.remove("o/r#1").unwrap();
        store.ledger.entries.insert("O/R#1".into(), entry);
        let mixed = crate::github::parse_github_issue("O/R#1", None, DEFAULT_HOST).unwrap();
        assert_eq!(store.dispatch_origin(&mixed, &record), (true, false));
    }

    #[test]
    fn historical_plans_require_complete_trusted_coverage_and_publication() {
        for (name, edit, expected) in [
            ("superseded", None, 1),
            ("foreign newer plan", Some("foreign"), 0),
            ("unknown newer editor", Some(""), 0),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ledger.json");
            let mut store = Store::load(path.to_str().unwrap(), "bot", at()).unwrap();
            store.cleanup = true;
            let plan = json!({"schema":plan::JOB_PLAN_SCHEMA,"repo":"o/r","item":1,"kind":"implement","runner":"hosted","brief":"Do it."});
            let node = |id, time, body| json!({"url":format!("https://github.com/o/r/issues/1#issuecomment-{id}"), "body":body,"createdAt":time,"lastEditedAt":null,"author":{"login":"bot"},"editor":null});
            let old = node(
                10,
                "2026-10-01T00:00:00Z",
                format!("Old.\n```{}\n{plan}\n```", plan::PLAN_FENCE),
            );
            let mut current = node(
                20,
                "2026-10-02T00:00:00Z",
                format!("Current.\n```{}\n{plan}\n```", plan::PLAN_FENCE),
            );
            if let Some(editor) = edit {
                current["lastEditedAt"] = current["createdAt"].clone();
                current["editor"] = if editor.is_empty() {
                    Value::Null
                } else {
                    json!({"login":editor})
                };
            }
            let page = |nodes, more| json!({"repository":{"issue":{"comments":{"nodes":nodes,"pageInfo":{"hasNextPage":more,"endCursor":"next"}}}}});
            let vars = json!({"owner":"o","name":"r","number":1,"after":null});
            store
                .expand(
                    &vars,
                    &mut page(
                        json!([node(30, "2026-10-03T00:00:00Z", MARKER.into()), old.clone()]),
                        true,
                    ),
                )
                .unwrap();
            assert!(store.historical(false).is_empty(), "incomplete history");
            let later = json!({"owner":"o","name":"r","number":1,"after":"next"});
            store
                .expand(&later, &mut page(json!([current]), false))
                .unwrap();
            assert!(
                store.historical(true).is_empty(),
                "must confirm publication"
            );
            let pending = store.pending().pop().unwrap();
            store.acknowledge(&pending).unwrap();
            assert_eq!(store.historical(true).len(), expected, "{name}");
            if expected > 0 {
                assert_eq!(
                    text(&store.historical(true)[0].1, "/url"),
                    text(&old, "/url")
                );
                let restored = Store::load(path.to_str().unwrap(), "bot", at()).unwrap();
                let records = &restored.ledger.entries["o/r#1"].records;
                assert_eq!(
                    plan::plan_id(&plan::comment(&records[0]).unwrap()).unwrap(),
                    "10@2026-10-01T00:00:00Z"
                );
                assert!(text(&pending.body, "/body").contains("Current."));
                // A later complete read no longer sees the newer source.
                // Its archived replay identity cannot supersede a live plan.
                store
                    .expand(
                        &vars,
                        &mut page(
                            json!([node(30, "2026-10-03T00:00:00Z", MARKER.into()), old.clone()]),
                            false,
                        ),
                    )
                    .unwrap();
                assert!(
                    store.historical(false).is_empty(),
                    "deleted superseding source"
                );
                let mut removed_fence =
                    node(20, "2026-10-02T00:00:00Z", "Replaced by a person.".into());
                removed_fence["lastEditedAt"] = "2026-10-04T00:00:00Z".into();
                removed_fence["editor"] = json!({"login":"foreign"});
                store
                    .expand(
                        &vars,
                        &mut page(
                            json!([
                                node(30, "2026-10-03T00:00:00Z", MARKER.into()),
                                old,
                                removed_fence
                            ]),
                            false,
                        ),
                    )
                    .unwrap();
                assert!(store.historical(false).is_empty(), "foreign fence removal");
            }
        }
    }

    #[test]
    fn historical_protocol_coverage_preserves_receipts_and_refuses_bounds() {
        for (name, body, admitted) in [
            (
                "dispatch",
                "<!-- agent-board-dispatch: 10@2026-10-01T00:00:00Z -->\nDispatched.".to_owned(),
                true,
            ),
            (
                "receipt",
                format!(
                    "Applied.\n```{}\n{}\n```",
                    epic_apply::APPLY_MARKER,
                    serde_json::to_string(&epic_apply::Detail::new(
                        "10@2026-10-01T00:00:00Z",
                        epic_apply::Outcome::Applied
                    ))
                    .unwrap()
                ),
                true,
            ),
            (
                "legacy receipt",
                "<!-- agent-board-epic-apply: 10@2026-10-01T00:00:00Z -->".into(),
                false,
            ),
            (
                "malformed",
                "```agent-board-epic-apply\n{}\n```".into(),
                false,
            ),
            ("unrelated", "A human-readable reply.".into(), false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut store = Store::load(
                dir.path().join("ledger.json").to_str().unwrap(),
                "bot",
                at(),
            )
            .unwrap();
            store.cleanup = true;
            let vars = json!({"owner":"o","name":"r","number":1});
            let node = |id, body| json!({"url":format!("https://github.com/o/r/issues/1#issuecomment-{id}"),"body":body,"createdAt":"2026-10-01T00:00:00Z","lastEditedAt":null,"author":{"login":"bot"},"editor":null});
            let record = node(10, body);
            let mut data = json!({"repository":{"issue":{"comments":{"nodes":[node(30, MARKER.into()),record.clone()],"pageInfo":{"hasNextPage":false}}}}});
            store.expand(&vars, &mut data).unwrap();
            assert_eq!(
                store.historical(false).len(),
                usize::from(admitted),
                "{name}"
            );
            if admitted {
                assert!(same_record(
                    &store.ledger.entries["o/r#1"].records[0],
                    &record
                ));
                let write = store.pending().pop().unwrap();
                store.acknowledge(&write).unwrap();
                assert_eq!(store.historical(true).len(), 1);
                store.minimized("o/r#1", 10).unwrap();
                assert!(store.historical(true).is_empty());
                let restored = Store::load(store.path.to_str().unwrap(), "bot", at()).unwrap();
                assert!(restored.ledger.entries["o/r#1"].minimized.contains(&10));
                // Ledger exhaustion must hold cleanup rather than drop guards.
                store
                    .history
                    .get_mut("o/r#1")
                    .unwrap()
                    .resize(MAX_RECORDS, record.clone());
                let vars = json!({"owner":"o","name":"r","number":1,"after":"more"});
                assert!(store.expand(&vars, &mut data).is_err());
                assert!(store.historical(false).is_empty());
            }
        }
    }

    #[test]
    fn historical_later_page_replaces_cached_receipt_before_emission() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::load(
            dir.path().join("ledger.json").to_str().unwrap(),
            "bot",
            at(),
        )
        .unwrap();
        store.cleanup = true;
        let body = |outcome| {
            format!(
                "Receipt.\n```{}\n{}\n```",
                epic_apply::APPLY_MARKER,
                serde_json::to_string(&epic_apply::Detail::new("10@2026-10-01T00:00:00Z", outcome))
                    .unwrap()
            )
        };
        let node = |id, body| json!({"url":format!("https://github.com/o/r/issues/1#issuecomment-{id}"),"body":body,"createdAt":"2026-10-01T00:00:00Z","lastEditedAt":null,"author":{"login":"bot"},"editor":null});
        let source = node(10, body(epic_apply::Outcome::Stopped));
        let page = |nodes, more| json!({"repository":{"issue":{"comments":{"nodes":nodes,"pageInfo":{"hasNextPage":more,"endCursor":"next"}}}}});
        let vars = json!({"owner":"o","name":"r","number":1,"after":null});
        store
            .expand(
                &vars,
                &mut page(json!([node(30, MARKER.into()), source.clone()]), false),
            )
            .unwrap();
        let mut first = page(json!([node(30, MARKER.into())]), true);
        store.expand(&vars, &mut first).unwrap();
        assert_eq!(
            first.pointer("/repository/issue/comments/nodes").unwrap(),
            &json!([])
        );
        let mut completed = source;
        completed["body"] = body(epic_apply::Outcome::Applied).into();
        completed["lastEditedAt"] = "2026-10-02T00:00:00Z".into();
        completed["editor"] = json!({"login":"bot"});
        let mut last = page(json!([completed]), false);
        store
            .expand(
                &json!({"owner":"o","name":"r","number":1,"after":"next"}),
                &mut last,
            )
            .unwrap();
        let records: Vec<_> = last
            .pointer("/repository/issue/comments/nodes")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|n| plan::comment(n).unwrap())
            .collect();
        let (receipts, unread) = epic_apply::receipts(&records, &["bot"]);
        assert!(unread.is_empty());
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].detail.outcome, epic_apply::Outcome::Applied);
    }

    #[test]
    fn historical_plan_withdrawn_at_its_source_is_not_rescheduled() {
        let plan = json!({"schema":plan::JOB_PLAN_SCHEMA,"repo":"o/r","item":1,"kind":"implement","runner":"hosted","brief":"Implement it."});
        let fenced = format!("Plan.\n```{}\n{plan}\n```", plan::PLAN_FENCE);
        let node = |id, body: &str| json!({"url":format!("https://github.com/o/r/issues/1#issuecomment-{id}"),"body":body,"createdAt":"2026-10-01T00:00:00Z","lastEditedAt":null,"author":{"login":"bot"},"editor":null});
        let page = |nodes: Vec<Value>| json!({"repository":{"issue":{"comments":{"nodes":nodes,"pageInfo":{"hasNextPage":false}}}}});
        let vars = json!({"owner":"o","name":"r","number":1});
        let mut unfenced = node(10, "Plan withdrawn.");
        unfenced["lastEditedAt"] = "2026-10-02T00:00:00Z".into();
        unfenced["editor"] = json!({"login":"bot"});
        for ((name, source, kept), later) in [
            ("unchanged", Some(node(10, &fenced)), true),
            ("deleted", None, false),
            ("fence removed by trusted edit", Some(unfenced), false),
        ]
        .into_iter()
        .flat_map(|case| [(case.clone(), true), (case, false)])
        {
            let dir = tempfile::tempdir().unwrap();
            let mut store = Store::load(
                dir.path().join("ledger.json").to_str().unwrap(),
                "bot",
                at(),
            )
            .unwrap();
            store.cleanup = true;
            // A held, undispatched plan is imported but stays current.
            store
                .expand(&vars, &mut page(vec![node(30, MARKER), node(10, &fenced)]))
                .unwrap();
            assert!(store.historical(false).is_empty(), "{name}");
            let write = store.pending().pop().unwrap();
            store.acknowledge(&write).unwrap();
            // Turning cleanup off later must not revive archived plans either.
            store.cleanup = later;
            let name = format!("{name}, cleanup {later}");
            // Plan selection and dispatch's re-read both see the issue as it is.
            for read in ["selection", "dispatch"] {
                let mut data = page(
                    [node(30, MARKER)]
                        .into_iter()
                        .chain(source.clone())
                        .collect(),
                );
                store.expand(&vars, &mut data).unwrap();
                let comments: Vec<_> = data
                    .pointer("/repository/issue/comments/nodes")
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|n| plan::comment(n).unwrap())
                    .collect();
                assert_eq!(
                    plan::newest_plan(&comments, &["bot"]).is_ok(),
                    kept,
                    "{name} {read}: {comments:#?}"
                );
                assert_eq!(
                    comments
                        .iter()
                        .filter(|c| plan::has_fence(&c.body, plan::PLAN_FENCE))
                        .count(),
                    usize::from(kept),
                    "{name} {read}: no archived duplicate"
                );
            }
            assert!(
                store.ledger.entries["o/r#1"]
                    .records
                    .iter()
                    .any(|r| text(r, "/url").ends_with("-10")),
                "{name}: archived record kept for replay protection"
            );
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    schema: String,
    bot: String,
    entries: BTreeMap<String, Entry>,
}

pub(super) struct Store {
    path: PathBuf,
    ledger: Ledger,
    at: String,
    pub saved: usize,
    duplicates: BTreeMap<String, BTreeSet<u64>>,
    discovered: BTreeMap<String, u64>,
    written: BTreeSet<String>,
    // First POSTs sent this pass. One that lost its reply may become visible
    // only after a read in the same pass, so its unbound records wait for a
    // later pass's full read before being dropped or retried.
    attempted: BTreeSet<String>,
    pub cleanup: bool,
    pub read_only: bool,
    history: BTreeMap<String, Vec<Value>>,
    complete: BTreeSet<String>,
}

/// A prepared public write. The full machine record is already durable.
pub(super) struct Write {
    key: String,
    pub method: &'static str,
    pub url: String,
    pub body: Value,
    created: bool,
    replaced_post: bool,
    record: Option<usize>,
}

pub(super) fn check_paths(path: &str, cache: Option<&str>) -> Result<()> {
    fn destination(path: &str) -> Result<PathBuf> {
        let path = Path::new(path);
        ensure!(
            !path.as_os_str().is_empty(),
            "livingStatus must name a cache file"
        );
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "state paths must not be symlinks"
                );
                return path.canonicalize().context("resolve state file");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect state file"),
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        Ok(parent
            .canonicalize()
            .context("resolve state directory")?
            .join(path.file_name().context("state path has no filename")?))
    }
    let ledger = destination(path)?;
    if let Some(cache) = cache {
        ensure!(
            ledger != destination(cache)?,
            "livingStatus must differ from the response cache state path"
        );
    }
    Ok(())
}

fn kind(body: &str) -> Option<&'static str> {
    if body.starts_with(&format!("<!-- {}: ", plan::DISPATCH_MARKER))
        || body.starts_with(plan::REFUSED_TRIAGE)
    {
        Some("Last dispatch")
    } else if body.starts_with(&format!("<!-- {HOLD_MARKER}: verdict ")) {
        Some("Last triage outcome")
    } else if plan::has_fence(body, epic_apply::APPLY_MARKER) {
        Some("Last apply")
    } else if plan::has_fence(body, plan::PLAN_FENCE) {
        Some("Current plan")
    } else {
        None
    }
}

fn identity(body: &str) -> Result<Option<String>> {
    if kind(body) == Some("Last apply") {
        let detail = plan::fenced_json(body, epic_apply::APPLY_MARKER)?;
        return Ok(detail.and_then(|v| v["plan"].as_str().map(str::to_owned)));
    }
    Ok(body
        .lines()
        .next()
        .filter(|l| l.starts_with("<!-- "))
        .map(str::to_owned))
}

fn state_id(entry: &Entry) -> String {
    // The POST response assigns URLs after the public body has been prepared.
    // Keep the reference stable across that assignment and duplicate relocation.
    let records: Vec<_> = entry
        .records
        .iter()
        .map(|record| {
            let mut record = record.clone();
            record["url"] = Value::Null;
            record
        })
        .collect();
    format!("{:x}", Sha256::digest(Value::Array(records).to_string()))
}

fn state_reference(body: &str) -> Option<&str> {
    body.lines().find_map(|line| {
        line.strip_prefix(STATE)
            .and_then(|s| s.strip_suffix(" -->"))
    })
}

fn public_body(entry: &Entry) -> String {
    let mut out = format!("{MARKER}\n## Board status\n");
    for &section in SECTIONS {
        if let Some(record) = entry
            .records
            .iter()
            .rev()
            .find(|r| kind(r["body"].as_str().unwrap_or_default()) == Some(section))
        {
            // Only the prose before a machine fence is public. Neither receipt
            // actions nor fingerprints are copied into hidden HTML comments.
            let body = record["body"].as_str().unwrap_or_default();
            let prose = body.split("```").next().unwrap_or_default();
            let prose = prose
                .lines()
                .filter(|l| !l.starts_with("<!-- "))
                .collect::<Vec<_>>()
                .join("\n");
            let prose: String = prose.chars().take(DISPLAY_CHARS).collect();
            out.push_str(&format!("\n### {section}\n{}\n", prose.trim()));
        } else if let Some(prose) = entry.prose.get(section) {
            out.push_str(&format!("\n### {section}\n{prose}\n"));
        }
    }
    if let Some(pulls) = &entry.pulls {
        out.push_str("\n### Pull requests\n");
        if pulls.is_empty() {
            out.push_str("None observed.\n");
        }
        for pull in pulls {
            out.push_str(&pull.display());
            out.push('\n');
        }
        out.push_str("\nObserved links only; not permission to push.\n");
    }
    out.push_str(&format!("\n{STATE}{} -->", state_id(entry)));
    out
}

fn marked(body: &str) -> bool {
    body.starts_with(MARKER) || body.starts_with(ORIGINAL_MARKER)
}

fn bind_records(entry: &mut Entry, key: &str, id: u64) -> Result<()> {
    let (repo, number) = key.rsplit_once('#').context("status issue key")?;
    let url = format!("https://github.com/{repo}/issues/{number}#issuecomment-{id}");
    for record in entry.records.iter_mut().filter(|r| r["url"].is_null()) {
        record["url"] = url.clone().into();
    }
    Ok(())
}

impl Store {
    pub fn load(path: &str, bot: &str, now: OffsetDateTime) -> Result<Self> {
        let path = PathBuf::from(path);
        let ledger = match std::fs::File::open(&path) {
            Ok(file) => {
                use std::io::Read;
                ensure!(
                    file.metadata()?.len() <= MAX_BYTES,
                    "status ledger is too large"
                );
                let mut bytes = Vec::new();
                file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() as u64 <= MAX_BYTES,
                    "status ledger is too large"
                );
                serde_json::from_slice::<Ledger>(&bytes).context("decode status ledger")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ledger {
                schema: SCHEMA.into(),
                bot: bot.into(),
                entries: BTreeMap::new(),
            },
            Err(e) => return Err(e).context("read status ledger"),
        };
        ensure!(
            ledger.schema == SCHEMA && ledger.bot.eq_ignore_ascii_case(bot),
            "status ledger schema or bot differs"
        );
        ensure!(
            ledger.entries.len() <= MAX_RECORDS,
            "status ledger entry bound exceeded"
        );
        ensure!(
            ledger
                .entries
                .values()
                .map(|e| e.records.len())
                .sum::<usize>()
                <= MAX_RECORDS,
            "status ledger record bound exceeded"
        );
        ensure!(
            ledger
                .entries
                .values()
                .map(|e| e.references.len() + e.uncertain_first_posts.len())
                .sum::<usize>()
                <= MAX_RECORDS,
            "status ledger reference bound exceeded"
        );
        ensure!(
            ledger
                .entries
                .values()
                .map(|e| e.minimized.len())
                .sum::<usize>()
                <= MAX_RECORDS,
            "status ledger minimization bound exceeded"
        );
        for (key, entry) in &ledger.entries {
            if let Some(pulls) = &entry.pulls {
                pull_links::validate(pulls)?;
            }
            let (repo, number) = key
                .rsplit_once('#')
                .context("invalid status ledger issue key")?;
            validate_repository(repo)?;
            ensure!(
                number.parse::<u64>().is_ok(),
                "invalid status ledger issue number"
            );
            let url = format!(
                "https://github.com/{repo}/issues/{number}#issuecomment-{}",
                entry.id.unwrap_or_default()
            );
            for record in &entry.records {
                let mut record = record.clone();
                if record["url"].is_null() {
                    record["url"] = url.clone().into();
                }
                let comment = plan::comment(&record).context("invalid status ledger record")?;
                ensure!(
                    comment.url.starts_with(&format!(
                        "https://github.com/{repo}/issues/{number}#issuecomment-"
                    )) && plan::comment_id(&comment.url).is_some()
                        && plan::written_by(&comment, bot),
                    "status ledger record identity differs"
                );
            }
        }
        Ok(Self {
            path,
            ledger,
            at: now.format(&time::format_description::well_known::Rfc3339)?,
            saved: 0,
            duplicates: BTreeMap::new(),
            discovered: BTreeMap::new(),
            written: BTreeSet::new(),
            attempted: BTreeSet::new(),
            cleanup: false,
            read_only: false,
            history: BTreeMap::new(),
            complete: BTreeSet::new(),
        })
    }

    fn save(&self) -> Result<()> {
        use std::io::Write;
        ensure!(
            self.ledger.entries.len() <= MAX_RECORDS,
            "status ledger entry bound exceeded"
        );
        ensure!(
            self.ledger
                .entries
                .values()
                .map(|e| e.minimized.len())
                .sum::<usize>()
                <= MAX_RECORDS,
            "status ledger minimization bound exceeded"
        );
        ensure!(
            self.ledger
                .entries
                .values()
                .map(|e| e.references.len() + e.uncertain_first_posts.len())
                .sum::<usize>()
                <= MAX_RECORDS,
            "status ledger reference bound exceeded"
        );
        ensure!(
            self.ledger
                .entries
                .values()
                .map(|e| e.records.len())
                .sum::<usize>()
                <= MAX_RECORDS,
            "status ledger record bound exceeded"
        );
        let bytes = serde_json::to_vec(&self.ledger)?;
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "status ledger is full; nothing sent"
        );
        if self.read_only {
            return Ok(());
        }
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut temp = tempfile::NamedTempFile::new_in(parent)
            .context("create status ledger temporary file")?;
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path).context("replace status ledger")?;
        std::fs::File::open(parent)?
            .sync_all()
            .context("sync status ledger directory")
    }

    pub fn prepare(&mut self, method: &str, url: &str, body: &Value) -> Result<Option<Write>> {
        let before = self.ledger.entries.clone();
        let result = self.prepare_inner(method, url, body);
        if result.is_err() {
            self.ledger.entries = before;
        }
        result
    }

    fn prepare_inner(&mut self, method: &str, url: &str, body: &Value) -> Result<Option<Write>> {
        let Some(text) = body["body"].as_str() else {
            return Ok(None);
        };
        if marked(text) {
            return Ok(None);
        }
        let parts: Vec<_> = url
            .strip_prefix(API_ROOT)
            .unwrap_or_default()
            .split('/')
            .collect();
        let key = match (method, parts.as_slice()) {
            ("POST", ["repos", owner, repo, "issues", number, "comments"])
                if kind(text).is_some() =>
            {
                validate_repository(&format!("{owner}/{repo}"))?;
                ensure!(number.parse::<u64>().is_ok(), "invalid status issue");
                format!("{owner}/{repo}#{number}")
            }
            ("PATCH", ["repos", owner, repo, "issues", "comments", id]) => {
                let id = id.parse::<u64>().context("status comment id")?;
                let prefix = format!("{owner}/{repo}#");
                let Some((key, _)) = self.ledger.entries.iter().find(|(k, e)| {
                    k.starts_with(&prefix)
                        && (e.id == Some(id)
                            || e.records
                                .iter()
                                .any(|r| plan::comment_id(super::text(r, "/url")) == Some(id)))
                }) else {
                    return Ok(None);
                };
                key.clone()
            }
            _ => return Ok(None),
        };
        let total: usize = self.ledger.entries.values().map(|e| e.records.len()).sum();
        if method == "POST" {
            ensure!(total < MAX_RECORDS, "status ledger is full; nothing sent");
        }
        let entry = self.ledger.entries.entry(key.clone()).or_default();
        ensure!(
            !entry.records.iter().any(|r| r["url"].is_null()),
            "first status publication for {key} is pending; reconcile before adding a record"
        );
        let created = entry.id.is_none();
        let id = entry.id;
        let (repo, number) = key.rsplit_once('#').context("status issue key")?;
        let record_url =
            id.map(|id| format!("https://github.com/{repo}/issues/{number}#issuecomment-{id}"));
        let record = json!({"body": text, "url": record_url, "author": {"login": self.ledger.bot},
            "editor": null, "lastEditedAt": null, "createdAt": self.at});
        let index = if method == "POST" {
            entry.records.push(record);
            entry.records.len() - 1
        } else {
            let identity = identity(text)?;
            let identity = identity.map(|line| match line.strip_prefix(plan::REFUSED_TRIAGE) {
                Some(plan) => format!("<!-- {}: {plan}", plan::DISPATCH_MARKER),
                None => line,
            });
            let index = entry
                .records
                .iter()
                .rposition(|r| {
                    identity.as_ref().is_some_and(|id| {
                        crate::pass::living::identity(r["body"].as_str().unwrap_or_default())
                            .ok()
                            .flatten()
                            .as_ref()
                            == Some(id)
                    })
                })
                .or_else(|| {
                    entry.records.iter().rposition(|r| {
                        kind(text).is_some()
                            && kind(r["body"].as_str().unwrap_or_default()) == kind(text)
                    })
                })
                .or_else(|| {
                    (kind(text).is_none() && text.starts_with("The plan in "))
                        .then(|| {
                            entry.records.iter().rposition(|r| {
                                kind(r["body"].as_str().unwrap_or_default())
                                    == Some("Last dispatch")
                            })
                        })
                        .flatten()
                })
                .context("status has no record to edit")?;
            entry.records[index]["body"] = text.into();
            // Dispatch outcome edits must not change the plan record's identity.
            entry.records[index]["lastEditedAt"] = self.at.clone().into();
            entry.records[index]["editor"] = json!({"login": self.ledger.bot});
            index
        };
        let public = public_body(entry);
        ensure!(
            public.len() <= COMMENT_BYTES,
            "status comment is full; nothing sent"
        );
        let public_url = match id {
            Some(id) => format!("{API_ROOT}repos/{repo}/issues/comments/{id}"),
            None => url.to_owned(),
        };
        self.save()?;
        if created {
            self.attempted.insert(key.clone());
        }
        Ok(Some(Write {
            key,
            method: if created { "POST" } else { "PATCH" },
            url: public_url,
            body: json!({"body": public}),
            created,
            replaced_post: method == "POST" && !created,
            record: Some(index),
        }))
    }

    /// Whether the dispatch came from a status comment or a standalone
    /// comment archived in the ledger. Copying the record does not change
    /// which comment supplied its original dispatch boundary.
    pub(super) fn dispatch_origin(&self, issue: &ItemRef, comment: &plan::Comment) -> (bool, bool) {
        let Some((entry, id)) = self.entry(issue).zip(plan::comment_id(&comment.url)) else {
            return (false, false);
        };
        let saved = entry.records.iter().any(|record| {
            text(record, "/url") == comment.url && text(record, "/body") == comment.body
        });
        let status = entry.id == Some(id) || entry.references.contains_key(&id);
        (saved && status, saved && !status)
    }

    pub(super) fn first_post_pending(&self, issue: &ItemRef) -> bool {
        self.entry(issue)
            .is_some_and(|entry| entry.records.iter().any(|record| record["url"].is_null()))
    }

    pub(super) fn triage_retry_exhausted(&self, issue: &ItemRef, id: &str) -> bool {
        self.entry(issue).is_some_and(|entry| {
            let marker = format!("{}{id} -->", plan::REFUSED_TRIAGE);
            entry
                .records
                .iter()
                .filter(|record| text(record, "/body").lines().next() == Some(marker.as_str()))
                .nth(1)
                .is_some()
        })
    }

    /// Ledger keys keep the case GitHub returned; [`key`] lowercases.
    fn entry(&self, issue: &ItemRef) -> Option<&Entry> {
        let key = key(issue);
        self.ledger
            .entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&key))
            .map(|(_, entry)| entry)
    }

    pub fn finish(&mut self, write: &Write, response: &mut (u16, Value)) -> Result<()> {
        if !(200..300).contains(&response.0) {
            return Ok(());
        }
        if write.created {
            let id = response.1["id"]
                .as_u64()
                .context("status POST returned no id; reconcile ledger")?;
            let entry = self
                .ledger
                .entries
                .get_mut(&write.key)
                .context("missing status entry")?;
            entry.id = Some(id);
            bind_records(entry, &write.key, id)?;
            if let Some(reference) = state_reference(text(&write.body, "/body")) {
                entry.references.insert(id, reference.into());
            }
            self.save()?;
        }
        self.acknowledge(write)?;
        self.written.insert(write.key.clone());
        self.saved += usize::from(write.replaced_post);
        let id = self
            .ledger
            .entries
            .get(&write.key)
            .and_then(|e| e.id)
            .context("status has no id")?;
        let (repo, number) = write.key.rsplit_once('#').context("status issue key")?;
        response.1["id"] = id.into();
        response.1["html_url"] =
            format!("https://github.com/{repo}/issues/{number}#issuecomment-{id}").into();
        if let Some(index) = write.record {
            let node = &self.ledger.entries[&write.key].records[index];
            let at = if node["lastEditedAt"].is_null() {
                &node["createdAt"]
            } else {
                &node["lastEditedAt"]
            };
            response.1["created_at"] = at.clone();
            response.1["updated_at"] = at.clone();
        }
        Ok(())
    }

    pub(super) fn track_pulls(
        &mut self,
        issue: &ItemRef,
        pulls: Vec<pull_links::PullLink>,
    ) -> Result<()> {
        pull_links::validate(&pulls)?;
        let key = key(issue);
        if pulls.is_empty() && !self.ledger.entries.contains_key(&key) {
            return Ok(());
        }
        let before = self.ledger.entries.clone();
        let entry = self.ledger.entries.entry(key).or_default();
        entry.pulls = Some(pulls);
        let size = public_body(entry).len();
        let result = (|| {
            ensure!(
                size <= COMMENT_BYTES,
                "status comment is full; nothing sent"
            );
            self.save()
        })();
        if let Err(error) = result {
            self.ledger.entries = before;
            return Err(error);
        }
        Ok(())
    }

    pub fn pending(&self) -> Vec<Write> {
        self.ledger
            .entries
            .iter()
            .filter_map(|(key, entry)| {
                // A prepared first POST has a durable record without a URL.
                // Reconciliation in a later pass scans the whole thread
                // before retrying it.
                let unbound = entry.records.iter().any(|r| r["url"].is_null());
                if entry.id.is_none()
                    && ((entry.pulls.is_none() && !unbound)
                        || (unbound && self.attempted.contains(key)))
                {
                    return None;
                }
                let body = public_body(entry);
                if body.len() > COMMENT_BYTES {
                    return None;
                }
                if entry.published.as_ref() == Some(&body) {
                    return None;
                }
                let (repo, number) = key.rsplit_once('#')?;
                Some(Write {
                    key: key.clone(),
                    method: if entry.id.is_some() { "PATCH" } else { "POST" },
                    url: match entry.id {
                        Some(id) => format!("{API_ROOT}repos/{repo}/issues/comments/{id}"),
                        None => format!("{API_ROOT}repos/{repo}/issues/{number}/comments"),
                    },
                    body: json!({"body":body}),
                    created: entry.id.is_none(),
                    replaced_post: false,
                    record: None,
                })
            })
            .collect()
    }

    pub fn issue(write: &Write) -> Result<ItemRef> {
        crate::github::parse_github_issue(&write.key, None, DEFAULT_HOST)
    }

    pub fn acknowledge(&mut self, write: &Write) -> Result<()> {
        self.ledger
            .entries
            .get_mut(&write.key)
            .context("missing status entry")?
            .published = write.body["body"].as_str().map(str::to_owned);
        self.save()
    }

    /// Discover by marker and authenticated ownership, never by a cached id alone.
    pub fn expand(&mut self, variables: &Value, data: &mut Value) -> Result<()> {
        let before = (
            self.ledger.entries.clone(),
            self.duplicates.clone(),
            self.discovered.clone(),
            self.history.clone(),
            self.complete.clone(),
        );
        let result = self.expand_inner(variables, data);
        if result.is_err() {
            (
                self.ledger.entries,
                self.duplicates,
                self.discovered,
                self.history,
                self.complete,
            ) = before;
            let key = format!(
                "{}/{}#{}",
                text(variables, "/owner"),
                text(variables, "/name"),
                variables["number"]
            );
            self.complete.remove(&key);
        }
        result
    }

    fn expand_inner(&mut self, variables: &Value, data: &mut Value) -> Result<()> {
        let complete = data.pointer("/repository/issue/comments/pageInfo/hasNextPage")
            == Some(&Value::Bool(false));
        let Some(nodes) = data
            .pointer_mut("/repository/issue/comments/nodes")
            .and_then(Value::as_array_mut)
        else {
            return Ok(());
        };
        let key = format!(
            "{}/{}#{}",
            text(variables, "/owner"),
            text(variables, "/name"),
            variables["number"]
        );
        let mut expanded = Vec::new();
        let previous_records = self
            .ledger
            .entries
            .get(&key)
            .map(|e| e.records.clone())
            .unwrap_or_default();
        if variables["after"].is_null() {
            self.discovered.remove(&key);
            self.duplicates.remove(&key);
            self.history.remove(&key);
            self.complete.remove(&key);
        }
        if self.cleanup {
            let history = self.history.entry(key.clone()).or_default();
            ensure!(
                history.len() + nodes.len() <= MAX_RECORDS,
                "cleanup history bound exceeded"
            );
            history.extend(nodes.iter().cloned());
            if complete {
                self.complete.insert(key.clone());
            }
        }
        for node in nodes.iter() {
            let body = text(node, "/body");
            if !marked(body) || !text(node, "/author/login").eq_ignore_ascii_case(&self.ledger.bot)
            {
                expanded.push(node.clone());
                continue;
            }
            ensure!(
                text(node, "/author/login").eq_ignore_ascii_case(&self.ledger.bot)
                    && (node["lastEditedAt"].is_null()
                        || text(node, "/editor/login").eq_ignore_ascii_case(&self.ledger.bot)),
                "status comment has a foreign or unknown editor"
            );
            let id = plan::comment_id(text(node, "/url")).context("status comment id missing")?;
            let mut ignore_display = false;
            if body.contains(STATE) {
                let reference = state_reference(body).context("invalid status state reference")?;
                ensure!(!reference.is_empty(), "empty status state reference");
                ensure!(
                    self.ledger.entries.get(&key).is_some_and(|entry| {
                        reference == state_id(entry)
                            || entry.published.as_deref().and_then(state_reference)
                                == Some(reference)
                            || entry.references.get(&id).map(String::as_str) == Some(reference)
                            || entry.uncertain_first_posts.contains(reference)
                    }),
                    "status ledger missing or stale for {key}; restore its durable livingStatus file before continuing"
                );
            }
            let entry = self.ledger.entries.entry(key.clone()).or_default();
            if let Some(reference) = state_reference(body) {
                ignore_display = entry.uncertain_first_posts.contains(reference);
                if entry.records.iter().any(|r| r["url"].is_null()) && reference == state_id(entry)
                {
                    // The status POST landed, but without its reply the
                    // dispatcher never reached the workflow POST and epic
                    // apply never wrote. Adopt the comment, not their
                    // unstarted claims.
                    let before = entry.records.len();
                    entry.records.retain(|record| {
                        !record["url"].is_null()
                            || !matches!(
                                kind(text(record, "/body")),
                                Some("Last dispatch" | "Last apply")
                            )
                    });
                    if entry.records.len() < before {
                        entry.uncertain_first_posts.insert(reference.into());
                        ignore_display = true;
                    }
                    bind_records(entry, &key, id)?;
                }
                entry.references.insert(id, reference.into());
            } else {
                entry.references.entry(id).or_default();
            }
            let first = self
                .discovered
                .get(&key)
                .copied()
                .map_or(id, |old| old.min(id));
            if let Some(old) = self.discovered.get(&key).copied().filter(|old| *old != id) {
                self.duplicates
                    .entry(key.clone())
                    .or_default()
                    .insert(old.max(id));
            }
            self.discovered.insert(key.clone(), first);
            if let Some(old) = entry.id.filter(|old| *old != first) {
                entry.references.entry(old).or_default();
            }
            entry.id = Some(first);
            if ignore_display {
                // Adopt the marker; use the ledger, not the uncertain POST's
                // older display of a claim or a hold outcome.
                if id == first {
                    entry.published = Some(body.into());
                }
                continue;
            }
            if let Some((_, encoded)) = body.split_once(RECORDS) {
                let encoded = encoded
                    .strip_suffix("\n-->")
                    .context("invalid status records")?;
                ensure!(encoded.len() <= COMMENT_BYTES, "status records too large");
                let records: Vec<Value> =
                    serde_json::from_str(encoded).context("decode status records")?;
                for mut record in records {
                    if record["url"].is_null() {
                        record["url"] = node["url"].clone();
                    }
                    let comment = plan::comment(&record).context("invalid status record")?;
                    ensure!(
                        plan::written_by(&comment, &self.ledger.bot)
                            && comment.url.starts_with(
                                text(node, "/url")
                                    .split("#issuecomment-")
                                    .next()
                                    .unwrap_or_default()
                            )
                            && plan::comment_id(&comment.url).is_some(),
                        "foreign status record"
                    );
                    // Keep a pending newer revision rather than appending the
                    // older public claim after its completed receipt.
                    if let Some(old) = entry.records.iter_mut().find(|r| {
                        r["createdAt"] == record["createdAt"]
                            && r["url"] == record["url"]
                            && (identity(text(r, "/body")).ok().flatten()
                                == identity(text(&record, "/body")).ok().flatten())
                            && kind(text(r, "/body")) == kind(text(&record, "/body"))
                    }) {
                        if text(&record, "/lastEditedAt") > text(old, "/lastEditedAt") {
                            *old = record;
                        }
                    } else {
                        entry.records.push(record);
                    }
                }
            }
            for &section in SECTIONS {
                if let Some((_, prose)) = body.split_once(&format!("\n### {section}\n")) {
                    let prose = prose
                        .split("\n### ")
                        .next()
                        .unwrap_or_default()
                        .split(RECORDS)
                        .next()
                        .unwrap_or_default()
                        .split(STATE)
                        .next()
                        .unwrap_or_default()
                        .trim();
                    entry
                        .prose
                        .insert(section.into(), prose.chars().take(DISPLAY_CHARS).collect());
                }
            }
            ensure!(
                entry.records.len() <= MAX_RECORDS,
                "status record bound exceeded"
            );
            ensure!(
                public_body(entry).len() <= COMMENT_BYTES,
                "status comment is full; nothing sent"
            );
            if id == first {
                entry.published = Some(body.into());
            }
        }
        if complete
            && !self.discovered.contains_key(&key)
            && !self.written.contains(&key)
            && let Some(entry) = self.ledger.entries.get_mut(&key)
        {
            if let Some(id) = entry.id.take() {
                entry.references.entry(id).or_default();
            }
            entry.published = None;
        }
        if complete
            && !self.attempted.contains(&key)
            && let Some(entry) = self.ledger.entries.get_mut(&key)
        {
            // No matching POST was found for the remaining unbound records.
            // A dispatch or apply could not proceed without a comment ID.
            // Let the checked operation acquire its own claim on retry; a
            // background publication would claim work that never started.
            // The ordinary triage receipt is different: its label write
            // preceded the POST, so keep the display receipt pending.
            let keep = |record: &Value| {
                !record["url"].is_null()
                    || kind(text(record, "/body")) == Some("Last triage outcome")
            };
            if entry.records.iter().any(|r| r["url"].is_null()) {
                if !self.discovered.contains_key(&key) {
                    entry.uncertain_first_posts.insert(state_id(entry));
                }
                entry.records.retain(keep);
            }
            if let Some(id) = self.discovered.get(&key) {
                bind_records(entry, &key, *id)?;
            }
        }
        if self.cleanup && complete && self.discovered.contains_key(&key) {
            let entry = self
                .ledger
                .entries
                .get_mut(&key)
                .context("missing discovered status")?;
            // Import only self-contained protocols. Keep the original URL and
            // timestamps: relocation would change logical plan/replay identity.
            for node in &self.history[&key] {
                if let Some(source) = trusted_history(node, &key, &self.ledger.bot) {
                    if let Some(saved) = entry.records.iter_mut().find(|r| same_origin(r, node)) {
                        // Recovery edits target the living status, leaving the
                        // historical source untouched. Never resurrect that
                        // older claim beside its confirmed durable revision.
                        if plan::comment(saved).is_ok_and(|c| source.changed > c.changed) {
                            *saved = node.clone();
                        }
                    } else {
                        entry.records.push(node.clone());
                    }
                }
            }
            entry.records.sort_by_key(|r| {
                plan::comment(r)
                    .ok()
                    .map(|c| (c.changed, plan::comment_id(&c.url)))
            });
        }
        // A read immediately after our write may not show the status yet.
        // Keep local claims authoritative even during that visibility window.
        if let Some(entry) = self.ledger.entries.get(&key) {
            // Archived records outlive turning cleanup off; without its
            // complete history, judge them by this page alone.
            let sources = if self.cleanup {
                self.history.get(&key).map_or(&[][..], Vec::as_slice)
            } else {
                nodes.as_slice()
            };
            // An archived plan keeps replay identity, not scheduling authority:
            // once its source is deleted or no longer a trusted plan, the
            // issue has withdrawn it.
            let withdrawn = |r: &Value| {
                !plan::comment_id(text(r, "/url"))
                    .is_some_and(|id| entry.id == Some(id) || entry.references.contains_key(&id))
                    && plan::has_fence(text(r, "/body"), plan::PLAN_FENCE)
                    && !sources.iter().any(|n| {
                        same_origin(r, n)
                            && trusted_history(n, &key, &self.ledger.bot)
                                .is_some_and(|c| plan::has_fence(&c.body, plan::PLAN_FENCE))
                    })
            };
            expanded.retain(|node| {
                !trusted_history(node, &key, &self.ledger.bot).is_some_and(|source| {
                    entry.records.iter().any(|r| {
                        same_origin(r, node)
                            && plan::comment(r).is_ok_and(|saved| saved.changed > source.changed)
                    })
                })
            });
            expanded.extend(
                entry
                    .records
                    .iter()
                    .filter(|r| {
                        (if self.cleanup {
                            // Do not emit a cached revision before a later
                            // page can supply its newer historical source.
                            complete
                        } else {
                            variables["after"].is_null() || !previous_records.contains(r)
                        }) && !r["url"].is_null()
                            && !sources.contains(r)
                            && !withdrawn(r)
                    })
                    .cloned(),
            );
        }
        *nodes = expanded;
        self.save()?;
        Ok(())
    }

    pub fn duplicates(&self) -> Vec<(String, u64)> {
        self.duplicates
            .iter()
            .filter(|(key, _)| {
                self.ledger
                    .entries
                    .get(*key)
                    .is_some_and(|e| e.published.as_deref() == Some(public_body(e).as_str()))
            })
            .flat_map(|(key, ids)| ids.iter().map(move |id| (key.clone(), *id)))
            .collect()
    }

    /// Only exact, authenticated records covered by the authoritative
    /// ledger are eligible. Uncovered history is evidence, not garbage: leave it
    /// visible rather than guessing at legacy multi-comment replay protocols.
    pub fn historical(&self, published: bool) -> Vec<(String, Value)> {
        if !self.cleanup {
            return Vec::new();
        }
        let mut out = Vec::new();
        for key in &self.complete {
            let Some(entry) = self.ledger.entries.get(key) else {
                continue;
            };
            if !self.discovered.contains_key(key)
                || (published && entry.published.as_deref() != Some(public_body(entry).as_str()))
            {
                continue;
            }
            let history = &self.history[key];
            for node in history {
                let Some(comment) = trusted_history(node, key, &self.ledger.bot) else {
                    continue;
                };
                if entry.id == plan::comment_id(&comment.url) || kind(&comment.body).is_none() {
                    continue;
                }
                if plan::comment_id(&comment.url).is_some_and(|id| entry.minimized.contains(&id)) {
                    continue;
                }
                // Body equality alone is not enough: changing the URL or edit
                // time changes a plan's logical identity and can replay work.
                let covered = entry.records.iter().any(|r| {
                    trusted_history(r, key, &self.ledger.bot).is_some() && same_record(r, node)
                });
                if !covered {
                    continue;
                }
                if kind(&comment.body) == Some("Current plan") {
                    // Any ambiguous/untrusted newer plan holds cleanup. Keep
                    // the current logical plan, even if its prose is identical.
                    let plans: Vec<_> = history
                        .iter()
                        // Only living-status records are independently
                        // authoritative. Archived source plans that vanished
                        // or changed cannot prove live supersession.
                        .chain(
                            entry
                                .records
                                .iter()
                                .filter(|r| plan::comment_id(text(r, "/url")) == entry.id),
                        )
                        .filter(|n| plan::has_fence(text(n, "/body"), plan::PLAN_FENCE))
                        .collect();
                    let changed_source = entry
                        .records
                        .iter()
                        .filter(|r| {
                            plan::comment_id(text(r, "/url")) != entry.id
                                && plan::has_fence(text(r, "/body"), plan::PLAN_FENCE)
                        })
                        .any(|r| {
                            history
                                .iter()
                                .any(|n| same_origin(r, n) && !same_record(r, n))
                        });
                    if changed_source
                        || plans
                            .iter()
                            .any(|n| trusted_history(n, key, &self.ledger.bot).is_none())
                        || !plans
                            .iter()
                            .filter_map(|n| trusted_history(n, key, &self.ledger.bot))
                            .any(|c| c.changed > comment.changed)
                    {
                        continue;
                    }
                }
                out.push((key.clone(), node.clone()));
            }
        }
        out
    }

    pub fn minimized(&mut self, key: &str, id: u64) -> Result<()> {
        self.ledger
            .entries
            .get_mut(key)
            .context("missing cleanup entry")?
            .minimized
            .insert(id);
        self.save()
    }
}

fn trusted_history(node: &Value, key: &str, bot: &str) -> Option<plan::Comment> {
    // Absent facts are not evidence of an unedited comment.
    node.get("lastEditedAt")?;
    node.get("editor")?;
    let comment = plan::comment(node).ok()?;
    let protocols = [
        comment.body.starts_with("<!-- agent-board-dispatch: "),
        plan::has_fence(&comment.body, epic_apply::APPLY_MARKER),
        plan::has_fence(&comment.body, plan::PLAN_FENCE),
        plan::has_fence(&comment.body, crate::epic::EPIC_PLAN_FENCE),
    ];
    if protocols.into_iter().filter(|p| *p).count() != 1 {
        return None;
    }
    let (repo, number) = key.rsplit_once('#')?;
    if !plan::written_by(&comment, bot)
        || !comment.url.starts_with(&format!(
            "https://github.com/{repo}/issues/{number}#issuecomment-"
        ))
        || plan::comment_id(&comment.url).is_none()
    {
        return None;
    }
    match kind(&comment.body)? {
        "Last dispatch" => {
            let identity = comment
                .body
                .lines()
                .next()?
                .strip_prefix("<!-- agent-board-dispatch: ")?
                .strip_suffix(" -->")?;
            let (id, at) = identity.split_once('@')?;
            id.parse::<u64>().ok()?;
            OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339).ok()?;
        }
        "Last apply" => {
            let (receipts, unread) = epic_apply::receipts(std::slice::from_ref(&comment), &[bot]);
            if receipts.len() != 1 || !unread.is_empty() {
                return None;
            }
        }
        "Current plan" => {
            let value = plan::fenced_json(&comment.body, plan::PLAN_FENCE).ok()??;
            let plan = plan::JobPlan::from_json(&value).ok()?;
            if plan.repo != repo || plan.item.to_string() != number {
                return None;
            }
        }
        _ => return None,
    }
    Some(comment)
}

fn same_record(a: &Value, b: &Value) -> bool {
    [
        "url",
        "body",
        "createdAt",
        "lastEditedAt",
        "author",
        "editor",
    ]
    .iter()
    .all(|field| a.get(field) == b.get(field))
}

fn same_origin(a: &Value, b: &Value) -> bool {
    a.get("url") == b.get("url") && a.get("createdAt") == b.get("createdAt")
}
