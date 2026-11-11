//! One public status comment; durable replay records stay in the caller's ledger.
//!
//! The ledger is written before the forge write. An interrupted first POST is
//! deliberately not retried: a person must reconcile its unknown comment id.
use super::*;
use crate::epic_apply;
use serde::Serialize;
use std::path::{Path, PathBuf};

pub(crate) const MARKER: &str = "<!-- agent-board-status v1 -->";
const SCHEMA: &str = "agent-board/status/v1";
const MAX_RECORDS: usize = 1000;
const MAX_BYTES: u64 = 4 * 1024 * 1024;
const DISPLAY_CHARS: usize = 2000;

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: Option<u64>,
    records: Vec<Value>,
    published: Option<String>,
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
    fn missing_state_and_foreign_edits_refuse_without_a_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let mut store = Store::load(path.to_str().unwrap(), "bot", at()).unwrap();
        let variables = json!({"owner":"o", "name":"r", "number":1});
        let mut data = json!({"repository":{"issue":{"comments":{"nodes":[{
            "body":MARKER,"url":"https://github.com/o/r/issues/1#issuecomment-30",
            "author":{"login":"bot"},"lastEditedAt":null
        }]}}}});
        assert!(store.expand(&variables, &mut data).is_err());
        let write = store
            .prepare("POST", URL, &dispatched(10))
            .unwrap()
            .unwrap();
        assert!(
            Store::load(path.to_str().unwrap(), "bot", at()).is_err(),
            "ambiguous first POST must not be retried"
        );
        store.finish(&write, &mut (201, json!({"id":30}))).unwrap();
        data["repository"]["issue"]["comments"]["nodes"][0]["lastEditedAt"] =
            "2026-11-01T01:00:00Z".into();
        assert!(store.expand(&variables, &mut data).is_err());
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
            "livingStatus must name a durable ledger file"
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
        }
    }
    out
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
            ledger.entries.values().all(|e| e.id.is_some()),
            "status comment creation was interrupted; reconcile the ledger before continuing"
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
                entry.id.context("missing status id")?
            );
            ensure!(
                !entry.records.is_empty(),
                "status ledger entry has no records"
            );
            for record in &entry.records {
                let comment = plan::comment(record).context("invalid status ledger record")?;
                ensure!(
                    comment.url == url && plan::written_by(&comment, bot),
                    "status ledger record identity differs"
                );
            }
        }
        Ok(Self {
            path,
            ledger,
            at: now.format(&time::format_description::well_known::Rfc3339)?,
            saved: 0,
        })
    }

    fn save(&self) -> Result<()> {
        use std::io::Write;
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
        let Some(text) = body["body"].as_str() else {
            return Ok(None);
        };
        if text.starts_with(MARKER) {
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
                let Some((key, _)) = self
                    .ledger
                    .entries
                    .iter()
                    .find(|(k, e)| k.starts_with(&prefix) && e.id == Some(id))
                else {
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
                record["url"] =
                    format!("https://github.com/{repo}/issues/{number}#issuecomment-{id}").into();
            }
            self.save()?;
        }
        self.acknowledge(write)?;
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

    /// Replace only our verified status node with the durable protocol records.
    /// Missing state or a foreign editor is a refusal, never an empty history.
    pub fn expand(&self, variables: &Value, data: &mut Value) -> Result<()> {
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
        for node in nodes.iter() {
            let body = text(node, "/body");
            let known = self
                .ledger
                .entries
                .get(&key)
                .is_some_and(|entry| plan::comment_id(text(node, "/url")) == entry.id);
            if !known
                && (!body.starts_with(MARKER)
                    || !text(node, "/author/login").eq_ignore_ascii_case(&self.ledger.bot))
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
            let entry = self.ledger.entries.get(&key).context(
                "status comment exists but its ledger is missing; reconcile before continuing",
            )?;
            ensure!(
                plan::comment_id(text(node, "/url")) == entry.id,
                "status comment id differs from ledger"
            );
        }
        // A read immediately after our write may not show the status yet.
        // Keep local claims authoritative even during that visibility window.
        if variables["after"].is_null()
            && let Some(entry) = self.ledger.entries.get(&key)
        {
            expanded.extend(entry.records.iter().cloned());
        }
        *nodes = expanded;
        Ok(())
    }
}
