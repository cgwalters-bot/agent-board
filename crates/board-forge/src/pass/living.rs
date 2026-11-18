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
pub(super) const HISTORY_GROUP: &str = "query($ids: [ID!]!) { nodes(ids: $ids) { ... on IssueComment { url body createdAt lastEditedAt author { login } editor { login } isMinimized } } }";
const SCHEMA: &str = "agent-board/status/v1";
const MAX_RECORDS: usize = 1000;
const MAX_BYTES: u64 = 4 * 1024 * 1024;
const DISPLAY_CHARS: usize = 2000;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: Option<u64>,
    records: Vec<Value>,
    published: Option<String>,
    #[serde(default)]
    prose: BTreeMap<String, String>,
    // Last validated reference per public comment, including minimized duplicates.
    // Those comments remain readable after the authoritative status advances.
    #[serde(default)]
    references: BTreeMap<u64, String>,
    #[serde(default)]
    pulls: Option<Vec<pull_links::PullLink>>,
    // A confirmed minimization stays done even if a person later restores it.
    #[serde(default)]
    minimized: BTreeSet<u64>,
    // Split sources covered by an attempted or observed status publication.
    // Before a send is attempted, the import is redone on each scan.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    archived: BTreeSet<u64>,
}

impl Entry {
    fn protect_split_records(&mut self) {
        self.archived.extend(
            self.records
                .iter()
                .filter(|r| is_split_record(r))
                .filter_map(|r| plan::comment_id(text(r, "/url"))),
        );
    }

    fn archive_published(&mut self) {
        let state = state_id(self);
        if self.published.as_deref().and_then(state_reference) == Some(state.as_str()) {
            self.protect_split_records();
        }
    }
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
    fn historical_plans_require_complete_trusted_coverage_and_publication() {
        for (name, edit, split, expected) in [
            ("superseded", None, "", 1),
            ("foreign newer plan", Some("foreign"), "", 0),
            ("unknown newer editor", Some(""), "", 0),
            (
                "split claim on plan",
                None,
                "<!-- agent-board-epic-apply: 10@2026-10-01T00:00:00Z -->\n",
                0,
            ),
            (
                "split fingerprint on plan",
                None,
                "<!-- agent-board-epic-fingerprint: [] -->\n",
                0,
            ),
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
                format!("{split}Old.\n```{}\n{plan}\n```", plan::PLAN_FENCE),
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
    fn legacy_pairs_hold_on_competing_or_unreadable_claims() {
        let node = |id, author, body: &str| json!({"url":format!("https://github.com/o/r/issues/1#issuecomment-{id}"),"body":body,"createdAt":format!("2026-10-01T00:00:{id:02}Z"),"lastEditedAt":null,"author":{"login":author},"editor":null});
        let receipt = node(
            10,
            "bot",
            "<!-- agent-board-epic-apply: 1@2026-10-01T00:00:00Z -->\n2026-10-02T00:00:00Z: Applied.\n- Action 1: linked o/r#2.",
        );
        let fingerprint = node(
            20,
            "bot",
            "<!-- agent-board-epic-fingerprint: [[\"o/r#2\",\"OPEN\"]] -->\n2026-10-02T00:00:00Z: Applied.",
        );
        let claim = format!(
            "Applying.\n```{}\n{}\n```",
            epic_apply::APPLY_MARKER,
            serde_json::to_string(&epic_apply::Detail::new(
                "1@2026-10-01T00:00:00Z",
                epic_apply::Outcome::Applied
            ))
            .unwrap()
        );
        let unreadable = format!("```{}\n{{}}\n```", epic_apply::APPLY_MARKER);
        let other_plan = "2@2026-10-01T00:00:00Z";
        let fence = |detail: Value| format!("```{}\n{detail}\n```", epic_apply::APPLY_MARKER);
        let mut wrong_schema = json!(epic_apply::Detail::new(
            other_plan,
            epic_apply::Outcome::Applied
        ));
        let mut foreign_edit = node(30, "operator", &fence(wrong_schema.clone()));
        foreign_edit["lastEditedAt"] = foreign_edit["createdAt"].clone();
        foreign_edit["editor"] = json!({"login":"visitor"});
        wrong_schema["schema"] = "unsupported".into();
        for (name, live, records, expected) in [
            ("unique", vec![], vec![], 1),
            (
                "archived pair",
                vec![],
                vec![receipt.clone(), fingerprint.clone()],
                1,
            ),
            ("live claim", vec![node(30, "bot", &claim)], vec![], 0),
            ("ledger-only claim", vec![], vec![node(5, "bot", &claim)], 0),
            (
                "unreadable operator claim",
                vec![node(30, "operator", &unreadable)],
                vec![],
                0,
            ),
            (
                "unreadable ledger claim",
                vec![],
                vec![node(5, "bot", &unreadable)],
                0,
            ),
            (
                "partial detail for another plan",
                vec![node(30, "operator", &fence(json!({"plan":other_plan})))],
                vec![],
                0,
            ),
            (
                "wrong schema for another plan",
                vec![],
                vec![node(5, "bot", &fence(wrong_schema))],
                0,
            ),
            (
                "foreign edit to a claim for another plan",
                vec![foreign_edit.clone()],
                vec![],
                0,
            ),
            (
                "valid claim for another plan",
                vec![node(30, "OPERATOR", text(&foreign_edit, "/body"))],
                vec![],
                1,
            ),
            (
                "visitor cannot veto the pair",
                vec![
                    node(25, "visitor", text(&fingerprint, "/body")),
                    node(30, "visitor", &unreadable),
                ],
                vec![],
                1,
            ),
        ] {
            let history: Vec<_> = [receipt.clone(), fingerprint.clone()]
                .into_iter()
                .chain(live)
                .collect();
            let pairs = legacy_pairs(&history, &records, "o/r#1", "bot", &["bot", "operator"]);
            assert_eq!(pairs.len(), expected, "{name}");
        }
    }

    #[test]
    fn legacy_reader_keeps_source_order_and_refuses_missing_or_changed_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let path = path.to_str().unwrap();
        let vars = json!({"owner":"o","name":"r","number":1,"after":null});
        let later = json!({"owner":"o","name":"r","number":1,"after":"next"});
        let page = |nodes, more| json!({"repository":{"issue":{"comments":{"nodes":nodes,"pageInfo":{"hasNextPage":more,"endCursor":"next"}}}}});
        let node = |id, body| json!({"url":format!("https://github.com/o/r/issues/1#issuecomment-{id}"),"body":body,"createdAt":"2026-10-01T00:00:00Z","lastEditedAt":null,"author":{"login":"bot"},"editor":null});
        let status = node(30, MARKER);
        let mut receipt = node(
            10,
            "<!-- agent-board-epic-apply: 1@2026-10-01T00:00:00Z -->\n2026-10-02T00:00:00Z: Applied.\n- Action 1: linked o/r#2.",
        );
        receipt["lastEditedAt"] = "2026-10-02T00:00:00Z".into();
        receipt["editor"] = json!({"login":"bot"});
        let fingerprint = node(
            20,
            "<!-- agent-board-epic-fingerprint: [[\"o/r#2\",\"OPEN\"]] -->\n2026-10-02T00:00:00Z: Applied.",
        );
        let read = |nodes: Vec<Value>| {
            let parsed: Vec<_> = nodes.iter().map(|n| plan::comment(n).unwrap()).collect();
            epic_apply::receipts(&parsed, &["bot", "operator"])
        };
        let before = read(vec![receipt.clone(), fingerprint.clone()]);
        assert_eq!(before.0[0].detail.fingerprint.as_ref().unwrap().len(), 1);
        let mut store = Store::load(path, "bot", at()).unwrap();
        store.cleanup = true;
        store
            .expand(&vars, &mut page(json!([status, receipt]), true))
            .unwrap();
        assert!(store.historical(false).is_empty());
        assert!(store.ledger.entries["o/r#1"].records.is_empty());
        store
            .expand(&later, &mut page(json!([fingerprint]), false))
            .unwrap();
        assert!(store.historical(true).is_empty());
        let pending = store.pending().pop().unwrap();
        assert!(
            text(&pending.body, "/body").contains(
                "### Last apply\n2026-10-02T00:00:00Z: Applied.\n- Action 1: linked o/r#2."
            )
        );
        store.acknowledge(&pending).unwrap();
        assert_eq!(store.historical(true).len(), 2);
        store.minimized("o/r#1", 10).unwrap();
        let mut status = status;
        status["body"] = pending.body["body"].clone();

        for (case, ledger_path, status_present, source, succeeds) in [
            (
                "restart with minimized prefix",
                path,
                true,
                Some(fingerprint.clone()),
                true,
            ),
            (
                "status deleted",
                path,
                false,
                Some(fingerprint.clone()),
                true,
            ),
            ("source deleted", path, true, None, false),
            ("both status and source deleted", path, false, None, false),
            (
                "missing ledger",
                dir.path().join("missing").to_str().unwrap(),
                true,
                Some(fingerprint.clone()),
                false,
            ),
        ] {
            let mut restored = Store::load(ledger_path, "bot", at()).unwrap();
            let mut first = page(
                json!(if status_present {
                    vec![status.clone(), receipt.clone()]
                } else {
                    vec![receipt.clone()]
                }),
                true,
            );
            let mut last = page(json!(source.into_iter().collect::<Vec<_>>()), false);
            let result = restored
                .expand(&vars, &mut first)
                .and_then(|()| restored.expand(&later, &mut last));
            assert_eq!(result.is_ok(), succeeds, "{case}: {result:?}");
            if succeeds {
                let nodes = [first, last]
                    .into_iter()
                    .flat_map(|p| {
                        p["repository"]["issue"]["comments"]["nodes"]
                            .as_array()
                            .unwrap()
                            .clone()
                    })
                    .collect();
                assert_eq!(read(nodes), before);
                assert_eq!(
                    restored.ledger.entries["o/r#1"].records,
                    [receipt.clone(), fingerprint.clone()]
                );
                if !status_present {
                    let pending = restored.pending().pop().unwrap();
                    assert_eq!(pending.method, "POST");
                    assert_eq!(pending.body["body"], status["body"]);
                    assert!(restored.historical(true).is_empty());
                }
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

/// Where a dispatch comment's GitHub boundary comes from
/// ([`Store::dispatch_origin`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    /// A standalone receipt as GitHub returned it in the current read.
    Comment,
    /// Written to the status ledger, at the pass clock.
    Ledger,
    /// No live receipt or known status boundary.
    Unknown,
}

pub(super) struct Store {
    path: PathBuf,
    ledger: Ledger,
    at: String,
    pub saved: usize,
    duplicates: BTreeMap<String, BTreeSet<u64>>,
    discovered: BTreeMap<String, u64>,
    written: BTreeSet<String>,
    pub operator: Option<String>,
    pub cleanup: bool,
    pub read_only: bool,
    history: BTreeMap<String, Vec<Value>>,
    complete: BTreeSet<String>,
    // Only standalone receipt revisions actually emitted from GitHub pages.
    live_dispatches: BTreeMap<String, Vec<plan::Comment>>,
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
    if body.starts_with(&format!("<!-- {}: ", plan::DISPATCH_MARKER)) {
        Some("Last dispatch")
    } else if plan::has_fence(body, epic_apply::APPLY_MARKER) {
        Some("Last apply")
    } else if plan::has_fence(body, plan::PLAN_FENCE) {
        Some("Current plan")
    } else {
        None
    }
}

/// A split legacy receipt has no fence, but its completion and action lines
/// must stay visible once its sources are minimized.
fn section_kind(record: &Value) -> Option<&'static str> {
    kind(text(record, "/body")).or_else(|| {
        (is_split_record(record) && legacy_marker(record) == Some(epic_apply::APPLY_MARKER))
            .then_some("Last apply")
    })
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
    for section in ["Current plan", "Last dispatch", "Last apply"] {
        if let Some(record) = entry
            .records
            .iter()
            .rev()
            .find(|r| section_kind(r) == Some(section))
        {
            // Only the prose before a machine fence is public. Neither receipt
            // actions nor fingerprints are copied into hidden HTML comments.
            let prose = status_prose(record);
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

fn status_prose(record: &Value) -> String {
    let body = text(record, "/body");
    if is_split_record(record) {
        let prose = body
            .lines()
            .skip(1)
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n");
        if prose.chars().count() <= DISPLAY_CHARS {
            return prose;
        }
        // Cut at a line boundary and leave room to point to the unchanged
        // original, which is still readable after reversible minimization.
        let url = text(record, "/url");
        let notice = format!("\n\nAction log shortened; [read the full receipt]({url}).");
        let prefix: String = prose
            .chars()
            .take(DISPLAY_CHARS.saturating_sub(notice.chars().count()))
            .collect();
        let lines = prefix.rsplit_once('\n').map_or("", |(lines, _)| lines);
        return format!("{lines}{notice}");
    }
    body.split("```")
        .next()
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with("<!-- "))
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(DISPLAY_CHARS)
        .collect()
}

fn marked(body: &str) -> bool {
    body.starts_with(MARKER) || body.starts_with(ORIGINAL_MARKER)
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
                .map(|e| e.references.len())
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
                if entry.id.is_none() {
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
            operator: None,
            cleanup: false,
            read_only: false,
            history: BTreeMap::new(),
            live_dispatches: BTreeMap::new(),
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
                .map(|e| e.references.len())
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
        entry.protect_split_records();
        self.save()?;
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

    /// An archived copy can block replay, but it cannot locate a verdict in
    /// the live conversation. Use only the exact standalone revision read
    /// from GitHub or a receipt on a known status comment.
    pub(super) fn dispatch_origin(&self, issue: &ItemRef, comment: &plan::Comment) -> Origin {
        let Some(id) = plan::comment_id(&comment.url) else {
            return Origin::Unknown;
        };
        // Writes and discovery may use different casing. A stale alias must
        // not mask the entry that supplied a saved record and make it look
        // like a standalone comment.
        let key = key(issue);
        let status = self.ledger.entries.iter().any(|(name, entry)| {
            name.eq_ignore_ascii_case(&key)
                && (entry.id == Some(id) || entry.references.contains_key(&id))
        });
        let live = self
            .live_dispatches
            .get(&key.to_ascii_lowercase())
            .is_some_and(|records| records.contains(comment));
        match (live, status) {
            (true, false) => Origin::Comment,
            (false, true) => Origin::Ledger,
            _ => Origin::Unknown,
        }
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
            let (repo, number) = write.key.rsplit_once('#').context("status issue key")?;
            for record in &mut entry.records {
                if record["url"].is_null() {
                    record["url"] =
                        format!("https://github.com/{repo}/issues/{number}#issuecomment-{id}")
                            .into();
                }
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
                // A missing status can be recreated from verified legacy sources
                // even without a pull inventory. Reconciliation scans again
                // before sending this POST.
                let repair_legacy =
                    self.complete.contains(key) && entry.records.iter().any(is_split_record);
                if entry.id.is_none() && entry.pulls.is_none() && !repair_legacy {
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

    /// A write may have landed even if the response is lost. Persist the
    /// replay guard first; it does not by itself authorize minimization.
    pub fn start_publication(&mut self, write: &Write) -> Result<()> {
        let entry = self
            .ledger
            .entries
            .get_mut(&write.key)
            .context("missing status entry")?;
        if entry.records.iter().any(is_split_record) {
            entry.protect_split_records();
            self.save()?;
        }
        Ok(())
    }

    pub fn acknowledge(&mut self, write: &Write) -> Result<()> {
        let entry = (self.ledger.entries.get_mut(&write.key)).context("missing status entry")?;
        entry.published = write.body["body"].as_str().map(str::to_owned);
        entry.archive_published();
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
        let archived_legacy = previous_records.iter().any(is_split_record);
        let live_key = key.to_ascii_lowercase();
        if variables["after"].is_null() {
            self.live_dispatches.remove(&live_key);
            self.discovered.remove(&key);
            self.duplicates.remove(&key);
            self.history.remove(&key);
            self.complete.remove(&key);
        }
        if self.cleanup || archived_legacy {
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
            if body.contains(STATE) {
                let reference = state_reference(body).context("invalid status state reference")?;
                ensure!(
                    self.ledger.entries.get(&key).is_some_and(|entry| {
                        reference == state_id(entry)
                            || entry.published.as_deref().and_then(state_reference)
                                == Some(reference)
                            || entry.references.get(&id).map(String::as_str) == Some(reference)
                    }),
                    "status ledger missing or stale for {key}; restore its durable livingStatus file before continuing"
                );
            }
            let entry = self.ledger.entries.entry(key.clone()).or_default();
            if let Some(reference) = state_reference(body) {
                entry.references.insert(id, reference.into());
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
            entry.id = Some(first);
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
            for section in ["Current plan", "Last dispatch", "Last apply"] {
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
            entry.id = None;
            entry.published = None;
        }
        if complete && (archived_legacy || (self.cleanup && self.discovered.contains_key(&key))) {
            let history = &self.history[&key];
            let entry = self
                .ledger
                .entries
                .get_mut(&key)
                .context("missing discovered status")?;
            // GitHub still returns minimized comments in the issue connection.
            // Keep those live positions for the legacy reader's forward search.
            // If either published source vanishes or changes, neither guess its
            // old fingerprint nor drop the apply claim and allow replay.
            if archived_legacy {
                entry.archive_published();
                // A publication may have landed without an acknowledgement.
                // Judge an import never sent to the forge again from its sources.
                entry.records.retain(|r| {
                    !is_split_record(r)
                        || plan::comment_id(text(r, "/url")).is_some_and(|id| {
                            entry.archived.contains(&id) || entry.minimized.contains(&id)
                        })
                });
                let positions: Option<Vec<_>> = entry
                    .records
                    .iter()
                    .filter(|r| is_split_record(r))
                    .map(|r| history.iter().position(|node| same_record(r, node)))
                    .collect();
                ensure!(
                    positions.is_some_and(|p| p.windows(2).all(|w| w[0] < w[1])),
                    "archived legacy apply evidence changed or is missing for {key}; inspect both sources before continuing"
                );
            }
            for node in history.iter().filter(|_| self.cleanup) {
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
            // Import the pair as one durable update, or neither member. Keep
            // both original nodes, not a reconstructed receipt or fingerprint.
            if self.cleanup
                && authoritative_history(history, entry, &key, &self.ledger.bot).is_some()
            {
                let bot = &self.ledger.bot;
                let writers = [bot.as_str(), self.operator.as_deref().unwrap_or(bot)];
                for pair in legacy_pairs(history, &entry.records, &key, bot, &writers) {
                    if pair.iter().any(|node| {
                        entry
                            .records
                            .iter()
                            .any(|r| r["url"] == node["url"] && !same_record(r, node))
                    }) {
                        continue;
                    }
                    for node in pair {
                        if !entry.records.iter().any(|r| same_record(r, node)) {
                            entry.records.push(node.clone());
                        }
                    }
                }
            }
            entry.records.sort_by_key(|r| {
                plan::comment(r).ok().map(|c| {
                    let at = if is_split_record(r) {
                        c.created
                    } else {
                        c.changed
                    };
                    (at, plan::comment_id(&c.url))
                })
            });
        }
        // A read immediately after our write may not show the status yet.
        // Keep local claims authoritative even during that visibility window.
        let mut saved = Vec::new();
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
                plan::comment_id(text(r, "/url")) != entry.id
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
            saved.extend(
                entry
                    .records
                    .iter()
                    .filter(|r| {
                        !is_split_record(r)
                            && (if self.cleanup {
                                // Do not emit a cached revision before a later
                                // page can supply its newer historical source.
                                complete
                            } else {
                                variables["after"].is_null() || !previous_records.contains(r)
                            })
                            && !sources.contains(r)
                            && !withdrawn(r)
                    })
                    .cloned(),
            );
        }
        for node in &expanded {
            if kind(text(node, "/body")) == Some("Last dispatch")
                && let Ok(record) = plan::comment(node)
            {
                self.live_dispatches
                    .entry(live_key.clone())
                    .or_default()
                    .push(record);
            }
        }
        expanded.extend(saved);
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

    /// Only exact, authenticated records covered by the authoritative ledger
    /// are eligible. Legacy members must both have coverage, even on retries
    /// after their partner was already minimized.
    pub fn historical(&self, published: bool) -> Vec<(String, Value, Option<Value>)> {
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
            if authoritative_history(history, entry, key, &self.ledger.bot).is_some() {
                let bot = &self.ledger.bot;
                let writers = [bot.as_str(), self.operator.as_deref().unwrap_or(bot)];
                for [receipt, fingerprint] in
                    legacy_pairs(history, &entry.records, key, bot, &writers)
                {
                    // Minimize only what a confirmed public status covered.
                    if [receipt, fingerprint].iter().all(|node| {
                        entry.records.iter().any(|r| same_record(r, node))
                            && (!published
                                || plan::comment_id(text(node, "/url"))
                                    .is_some_and(|id| entry.archived.contains(&id)))
                    }) {
                        for (node, partner) in [(receipt, fingerprint), (fingerprint, receipt)] {
                            if !plan::comment_id(text(node, "/url"))
                                .is_some_and(|id| entry.minimized.contains(&id))
                            {
                                out.push((key.clone(), node.clone(), Some(partner.clone())));
                            }
                        }
                    }
                }
            }
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
                out.push((key.clone(), node.clone(), None));
            }
        }
        out
    }

    pub fn authoritative_source(&self, key: &str) -> Option<Value> {
        authoritative_history(
            self.history.get(key)?,
            self.ledger.entries.get(key)?,
            key,
            &self.ledger.bot,
        )
        .cloned()
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

fn history_comment(node: &Value, key: &str, bot: &str) -> Option<plan::Comment> {
    // Absent facts are not evidence of an unedited comment.
    node.get("lastEditedAt")?;
    node.get("editor")?;
    let comment = plan::comment(node).ok()?;
    let (repo, number) = key.rsplit_once('#')?;
    let id = plan::comment_id(&comment.url).filter(|id| *id > 0)?;
    (plan::written_by(&comment, bot)
        && (comment.edited || node["editor"].is_null())
        && comment.url == format!("https://github.com/{repo}/issues/{number}#issuecomment-{id}"))
    .then_some(comment)
}

fn authoritative_history<'a>(
    history: &'a [Value],
    entry: &Entry,
    key: &str,
    bot: &str,
) -> Option<&'a Value> {
    history.iter().find(|node| {
        marked(text(node, "/body"))
            && plan::comment_id(text(node, "/url")) == entry.id
            && history_comment(node, key, bot).is_some()
    })
}

/// Recognize protocol-shaped text even with a malformed marker, so it can
/// veto an ambiguous match but cannot authorize a minimization.
fn legacy_marker(node: &Value) -> Option<&'static str> {
    [epic_apply::APPLY_MARKER, epic_apply::FINGERPRINT_MARKER]
        .into_iter()
        .find(|m| text(node, "/body").starts_with(&format!("<!-- {m}")))
}

fn is_split_record(node: &Value) -> bool {
    legacy_marker(node).is_some() && !plan::has_fence(text(node, "/body"), epic_apply::APPLY_MARKER)
}

/// Uniqueness covers every claim the apply reader can see: live comments and
/// ledger records, whose receipts the public status shows only as prose.
fn legacy_pairs<'a>(
    history: &'a [Value],
    records: &[Value],
    key: &str,
    bot: &str,
    writers: &[&str],
) -> Vec<[&'a Value; 2]> {
    let mut ordered = BTreeMap::<_, (Vec<_>, Vec<_>)>::new();
    let mut urls = BTreeSet::new();
    for node in history {
        // Duplicated pagination is not complete coverage.
        if node["body"].as_str().is_none()
            || plan::comment_id(text(node, "/url")).is_none()
            || !urls.insert(text(node, "/url"))
        {
            return Vec::new();
        }
    }
    let accepted =
        |node: &Value| epic_apply::claim_writer(node["author"]["login"].as_str(), writers);
    // The bot's public status is expanded into its ledger records before
    // replay sees it. Keep live comment order for legacy fingerprint lookup.
    let comments = history
        .iter()
        .filter(|n| {
            !(marked(text(n, "/body")) && text(n, "/author/login").eq_ignore_ascii_case(bot))
        })
        .chain(
            records
                .iter()
                .filter(|r| !history.iter().any(|n| same_record(r, n))),
        )
        .filter(|n| accepted(n))
        .map(plan::comment)
        .collect::<Result<Vec<_>>>();
    let Ok(comments) = comments else {
        return Vec::new();
    };
    let (receipts, unread) = epic_apply::receipts(&comments, writers);
    if !unread.is_empty() {
        return Vec::new();
    }
    let mut claims = BTreeMap::<_, Vec<_>>::new();
    for receipt in &receipts {
        claims
            .entry(receipt.detail.plan.as_str())
            .or_default()
            .push(receipt.url.as_str());
    }
    for (index, node) in history.iter().enumerate().filter(|(_, n)| accepted(n)) {
        let body = text(node, "/body");
        if let Some(marker) = legacy_marker(node)
            && let Some(status) = body.lines().nth(1)
        {
            let (receipts, fingerprints) = ordered.entry(status.trim_end()).or_default();
            if marker == epic_apply::APPLY_MARKER {
                receipts.push((index, node));
            } else {
                fingerprints.push((index, node));
            }
        }
    }
    ordered
        .into_values()
        .filter_map(|(receipts, fingerprints)| {
            let ([(r, receipt)], [(f, fingerprint)]) =
                (receipts.as_slice(), fingerprints.as_slice())
            else {
                return None;
            };
            let plan = epic_apply::legacy_plan(text(receipt, "/body").lines().next()?)?;
            if r >= f || claims.get(plan).map(Vec::as_slice) != Some(&[text(receipt, "/url")]) {
                return None;
            }
            let r = history_comment(receipt, key, bot)?;
            let f = history_comment(fingerprint, key, bot)?;
            ((r.created, plan::comment_id(&r.url)) < (f.created, plan::comment_id(&f.url))
                && epic_apply::completed_legacy_pair(&r, &f))
            .then_some([*receipt, *fingerprint])
        })
        .collect()
}

fn trusted_history(node: &Value, key: &str, bot: &str) -> Option<plan::Comment> {
    let comment = history_comment(node, key, bot)?;
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
    let kind = kind(&comment.body)?;
    // A split marker is never a self-contained record; only modern apply
    // receipts may carry one, checked below.
    if legacy_marker(node).is_some() && kind != "Last apply" {
        return None;
    }
    match kind {
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
            let [receipt] = receipts.as_slice() else {
                return None;
            };
            if !unread.is_empty() {
                return None;
            }
            // Modern receipts retain the older claim marker for compatibility.
            // That deliberate overlap is distinct from a split protocol mixed
            // with a machine fence or a different claim.
            if let Some(marker) = legacy_marker(node) {
                let mut lines = comment.body.lines();
                if marker != epic_apply::APPLY_MARKER
                    || lines.next().and_then(epic_apply::legacy_plan) != Some(&receipt.detail.plan)
                    || !lines.next().is_some_and(|header| {
                        [
                            "Applying [this epic plan](",
                            "Applied [this epic plan](",
                            "Stopped applying [this epic plan](",
                        ]
                        .iter()
                        .any(|prefix| header.starts_with(prefix))
                    })
                {
                    return None;
                }
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

pub(super) fn same_record(a: &Value, b: &Value) -> bool {
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
