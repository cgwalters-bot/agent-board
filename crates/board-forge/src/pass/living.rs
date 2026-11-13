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
    out.push_str(&format!("\n{STATE}{} -->", state_id(entry)));
    out
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
        for (key, entry) in &ledger.entries {
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
        })
    }

    fn save(&self) -> Result<()> {
        use std::io::Write;
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

    pub fn pending(&self) -> Vec<Write> {
        self.ledger
            .entries
            .iter()
            .filter_map(|(key, entry)| {
                let id = entry.id?;
                let body = public_body(entry);
                if body.len() > COMMENT_BYTES {
                    return None;
                }
                if entry.published.as_ref() == Some(&body) {
                    return None;
                }
                let (repo, _) = key.rsplit_once('#')?;
                Some(Write {
                    key: key.clone(),
                    method: "PATCH",
                    url: format!("{API_ROOT}repos/{repo}/issues/comments/{id}"),
                    body: json!({"body":body}),
                    created: false,
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
        );
        let result = self.expand_inner(variables, data);
        if result.is_err() {
            (self.ledger.entries, self.duplicates, self.discovered) = before;
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
        let previous_records = self.ledger.entries.get(&key).map_or(0, |e| e.records.len());
        if variables["after"].is_null() {
            self.discovered.remove(&key);
            self.duplicates.remove(&key);
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
        // A read immediately after our write may not show the status yet.
        // Keep local claims authoritative even during that visibility window.
        if let Some(entry) = self.ledger.entries.get(&key) {
            let skip = if variables["after"].is_null() {
                0
            } else {
                previous_records
            };
            expanded.extend(entry.records.iter().skip(skip).cloned());
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
}
