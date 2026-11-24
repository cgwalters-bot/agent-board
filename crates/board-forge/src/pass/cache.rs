//! Disposable REST and version-checked GraphQL representations.
//! The map is a rebuildable representation index, not action state.
use super::*;

pub(super) const REPRESENTATION: &str = "#agent-board-rest-v1";
const GRAPHQL_REPRESENTATION: &str = "#agent-board-graphql-item-v1-";
pub(crate) const MAX_ENTRIES: usize = 1024;
pub(crate) const MAX_BYTES: usize = 24 * 1024 * 1024;
pub(super) const MAX_FILE_BYTES: usize = 256 * 1024 * 1024;
// The two maps and the 64-byte scope digest need a fixed amount of JSON framing.
const FILE_OVERHEAD: usize = 256;
pub(crate) const LEGACY_MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 256 * 1024;
const MAX_OBSERVATIONS: usize = BOARD_PAGES * 100;

/// How long before a kept GraphQL item read its version must be for an equal
/// listing version to prove nothing changed since: `updatedAt` has whole
/// seconds, so an edit in the read's own second keeps it, and the pass clock
/// may run up to this far ahead of the forge's.
const READ_SKEW: time::Duration = time::Duration::minutes(5);

pub(super) fn shared_key(key: &str) -> bool {
    key.ends_with(REPRESENTATION)
        || key.split_once('#').is_some_and(|(_, fragment)| {
            fragment.starts_with(GRAPHQL_REPRESENTATION.trim_start_matches('#'))
        })
}

#[derive(Default)]
pub(super) struct Responses {
    pub separate_limit: Option<usize>,
    pub enabled: bool,
    pub context: Option<String>,
    pub entries: BTreeMap<String, Value>,
    index: Index,
    stamp: u64,
    pub hits: usize,
    pub misses: usize,
    pub modified: usize,
    pub graphql_reused: usize,
    pub graphql_listed: usize,
    pub graphql_lookups: usize,
    pub graphql_enabled: bool,
    /// The pass's clock, recorded on each GraphQL item read kept: no later
    /// than the read itself.
    pub now: Option<OffsetDateTime>,
    observations: BTreeMap<String, (usize, Version)>,
    /// Keys this pass evicted and has not kept again, and the misses on them.
    pub evicted: BTreeSet<String>,
    pub evicted_misses: usize,
    oversized: usize,
}

impl Responses {
    fn limit(&self) -> usize {
        self.separate_limit.unwrap_or(MAX_BYTES)
    }
}

#[derive(Debug, PartialEq)]
struct Version {
    id: String,
    updated: OffsetDateTime,
    head: Option<String>,
}

impl Version {
    fn read(id: &Value, updated: &Value, head: Option<&Value>) -> Option<Self> {
        let id = id.as_str().filter(|id| !id.is_empty() && id.len() <= 128)?;
        let updated = OffsetDateTime::parse(updated.as_str()?, &Rfc3339).ok()?;
        let head = match head {
            Some(head) => Some(
                head.as_str()
                    .filter(|oid| oid.len() == 40 && oid.bytes().all(|b| b.is_ascii_hexdigit()))?
                    .to_owned(),
            ),
            None => None,
        };
        Some(Self {
            id: id.to_owned(),
            updated,
            head,
        })
    }
}

/// Explicitly opt in discovery reads whose results are item-local. Checks,
/// relationships, review state and live pre-write queries do not opt in.
#[derive(Clone, Copy)]
pub(super) enum ItemQuery {
    Body,
    Kind,
}

impl ItemQuery {
    pub fn original(self) -> &'static str {
        match self {
            Self::Body => BODY_QUERY,
            Self::Kind => KIND_QUERY,
        }
    }

    pub fn versioned(self) -> String {
        match self {
            Self::Body => BODY_QUERY.replacen("url title", "id updatedAt url title", 1),
            Self::Kind => KIND_QUERY.replacen(
                "__typename",
                "__typename ... on Issue { id updatedAt } ... on PullRequest { id updatedAt headRefOid }",
                1,
            ),
        }
    }

    fn version(self, data: &Value) -> Option<Version> {
        let (node, pull) = match self {
            Self::Body => (data.pointer("/repository/issue")?, false),
            Self::Kind => {
                let item = data.pointer("/repository/issueOrPullRequest")?;
                (
                    item,
                    match item["__typename"].as_str()? {
                        "Issue" => false,
                        "PullRequest" => true,
                        _ => return None,
                    },
                )
            }
        };
        Version::read(
            &node["id"],
            &node["updatedAt"],
            pull.then_some(&node["headRefOid"]),
        )
    }
}

pub(super) fn item_path(repo: &str, number: u64) -> Option<String> {
    validate_repository(repo).ok()?;
    (number > 0).then(|| {
        format!(
            "{API_ROOT}repos/{}/issues/{number}",
            repo.to_ascii_lowercase()
        )
    })
}

pub(crate) fn seal(mut entry: Value) -> Value {
    if let Some(object) = entry.as_object_mut() {
        object.remove("digest");
    }
    entry["digest"] = format!("{:x}", Sha256::digest(entry.to_string())).into();
    entry
}

pub(crate) fn intact(entry: &Value) -> bool {
    entry.is_object()
        && entry.get("body").is_some()
        && entry
            .get("digest")
            .is_none_or(|digest| seal(entry.clone())["digest"] == *digest)
}

/// Whether a response may join the index: an intact API representation,
/// sealed where the state is `signed` (every scoped state is; only an older
/// unscoped file may hold unsigned entries). The one predicate for decoded
/// files and for states carried in memory between previews.
pub(crate) fn admissible(url: &str, entry: &Value, signed: bool) -> bool {
    url.starts_with(API_ROOT)
        && intact(entry)
        && (!signed || entry.get("digest").is_some_and(Value::is_string))
}

/// What [`bound`] evicted and what it kept.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Bounded {
    pub evicted: BTreeSet<String>,
    pub entries: usize,
    pub bytes: usize,
    limit: usize,
}

impl Bounded {
    /// The one line for callers that bound once in a while; the pass sums
    /// its evictions into its summary instead.
    pub fn warn(&self) {
        if !self.evicted.is_empty() {
            eprintln!(
                "cache: evicted {} oversized or excess disposable responses; cold reads",
                self.evicted.len()
            );
        }
    }
}

/// An unstamped (old-format or standalone poll) entry is oldest. Reverse URL
/// order breaks ties on eviction, retaining the old order for unstamped maps.
fn last_used(entry: &Value) -> u64 {
    entry["last_used"].as_u64().unwrap_or_default()
}

fn entry_bytes(url: &str, entry: &Value) -> usize {
    // Count the serialized key (including escaping), colon and comma too.
    json!(url).to_string().len() + entry.to_string().len() + 2
}

#[derive(Default)]
struct Index {
    sizes: BTreeMap<(u64, std::cmp::Reverse<String>), usize>,
    bytes: usize,
}

impl Index {
    fn insert(&mut self, url: &str, entry: &Value, size: usize) {
        self.sizes
            .insert((last_used(entry), std::cmp::Reverse(url.to_owned())), size);
        self.bytes += size;
    }

    fn remove(&mut self, url: &str, entry: &Value) {
        if let Some(size) = self
            .sizes
            .remove(&(last_used(entry), std::cmp::Reverse(url.to_owned())))
        {
            self.bytes -= size;
        }
    }

    fn bound(&mut self, entries: &mut BTreeMap<String, Value>, limit: usize) -> Bounded {
        let mut evicted = BTreeSet::new();
        self.sizes.retain(|(_, url), size| {
            if *size <= MAX_ENTRY_BYTES {
                return true;
            }
            self.bytes -= *size;
            entries.remove(&url.0);
            evicted.insert(url.0.clone());
            false
        });
        while self.bytes > limit || self.sizes.len() > MAX_ENTRIES {
            let Some(((_, url), size)) = self.sizes.pop_first() else {
                break;
            };
            self.bytes -= size;
            entries.remove(&url.0);
            evicted.insert(url.0);
        }
        Bounded {
            evicted,
            entries: entries.len(),
            bytes: self.bytes,
            limit,
        }
    }
}

/// Eviction affects responses only, never pending asks, receipts or accounting.
pub(crate) fn bound(entries: &mut BTreeMap<String, Value>, limit: usize) -> Bounded {
    let mut index = Index::default();
    for (url, entry) in entries.iter() {
        index.insert(url, entry, entry_bytes(url, entry));
    }
    index.bound(entries, limit)
}

/// Legacy embedded format only: leave room for the rest of the state file.
pub(super) fn bound_embedded(state: &mut State) -> Bounded {
    let mut entries = std::mem::take(&mut state.responses);
    let other_bytes = state.to_json().to_string().len();
    let limit = (STATE_BYTES as usize)
        .saturating_sub(other_bytes)
        .min(MAX_BYTES);
    let bounded = bound(&mut entries, limit);
    state.responses = entries;
    bounded
}

/// Bound both maps in the separate file without charging for any pass state.
pub(super) fn bound_separate(state: &mut State, limit: usize) -> Bounded {
    // A scoped read uses the shared index for the REST body. The old pass map
    // is needed only for the GraphQL editor evidence paired with those reads.
    state
        .pass_responses
        .retain(|_, entry| entry.get("comment_evidence").is_some());
    for entry in state.pass_responses.values_mut() {
        *entry = seal(std::mem::take(entry));
    }
    let evidence = bound(&mut state.pass_responses, limit);
    let mut kept = bound(&mut state.responses, limit - evidence.bytes);
    kept.bytes += evidence.bytes;
    kept.limit = limit;
    kept
}

/// Only response representations and their scope travel in the disposable file.
/// The digest checks detect corruption, not a writer who can forge the file.
#[derive(Serialize)]
pub struct ResponseCache {
    scope: String,
    responses: BTreeMap<String, Value>,
    pass_responses: BTreeMap<String, Value>,
}

/// The file as read: entries stay raw JSON text until the scope matches.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCache<'a> {
    scope: String,
    #[serde(borrow)]
    responses: RawEntries<'a>,
    #[serde(borrow)]
    pass_responses: RawEntries<'a>,
}

/// A map refused past the entry count or per-entry size before any entry
/// is decoded. A decoded `Value` can be many times its text's size, so
/// this bounds each entry, not the total: only restore a trusted file.
struct RawEntries<'a>(Vec<(String, &'a serde_json::value::RawValue)>);

impl<'de: 'a, 'a> Deserialize<'de> for RawEntries<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<'a>(std::marker::PhantomData<&'a ()>);
        impl<'de: 'a, 'a> serde::de::Visitor<'de> for Visitor<'a> {
            type Value = RawEntries<'a>;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a map of cached responses")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some((url, entry)) =
                    map.next_entry::<String, &serde_json::value::RawValue>()?
                {
                    if entries.len() == MAX_ENTRIES || entry.get().len() > MAX_ENTRY_BYTES {
                        return Err(serde::de::Error::custom(
                            "response cache exceeds its entry bounds",
                        ));
                    }
                    entries.push((url, entry));
                }
                Ok(RawEntries(entries))
            }
        }
        deserializer.deserialize_map(Visitor(std::marker::PhantomData))
    }
}

impl RawEntries<'_> {
    /// Check each entry's digest as it is decoded, so a corrupt file is
    /// refused after decoding at most one bounded entry. Intact entries were
    /// held in memory by the pass that wrote them.
    fn decode(self) -> Result<BTreeMap<String, Value>> {
        self.0
            .into_iter()
            .map(|(url, raw)| {
                let entry: Value = serde_json::from_str(raw.get())?;
                ensure!(admissible(&url, &entry, true), "invalid cached response");
                Ok((url, entry))
            })
            .collect()
    }
}

impl ResponseCache {
    pub const DEFAULT_BYTES: usize = MAX_BYTES;
    pub const MAX_BYTES: usize = MAX_FILE_BYTES;

    /// Ignore the embedded maps when the caller opts into the separate file.
    /// Missing or bad cache data must not reset cursors or scope in pass state.
    pub fn restore(path: &std::path::Path, limit: usize, state: &mut State) {
        state.responses.clear();
        state.pass_responses.clear();
        // Without a durable scope no file can match; don't read one.
        let Some(scope) = state.response_scope.as_deref() else {
            return;
        };
        let load = || -> Result<Self> {
            use std::io::Read;
            ensure!(
                std::fs::symlink_metadata(path)?.is_file(),
                "response cache must be a regular file"
            );
            let file = std::fs::File::open(path)?;
            let max = (limit.min(Self::MAX_BYTES) + FILE_OVERHEAD) as u64;
            ensure!(file.metadata()?.len() <= max, "response cache is too large");
            let mut bytes = Vec::new();
            file.take(max + 1).read_to_end(&mut bytes)?;
            ensure!(bytes.len() as u64 <= max, "response cache is too large");
            let raw: RawCache = serde_json::from_slice(&bytes)?;
            ensure!(raw.scope == scope, "response cache scope mismatch");
            let mut cache = Self {
                scope: raw.scope,
                responses: raw.responses.decode()?,
                pass_responses: raw.pass_responses.decode()?,
            };
            drop(bytes);
            let evidence = bound(&mut cache.pass_responses, limit);
            let responses = bound(&mut cache.responses, limit - evidence.bytes);
            ensure!(
                evidence.evicted.is_empty() && responses.evicted.is_empty(),
                "response cache exceeds its entry bounds"
            );
            Ok(cache)
        };
        match load() {
            Ok(cache) => {
                state.responses = cache.responses;
                state.pass_responses = cache.pass_responses;
            }
            // Don't print decoder errors that might quote cached content.
            Err(_) => eprintln!(
                "warning: missing, unreadable or out-of-scope pass response cache; cold reads"
            ),
        }
    }

    /// Keep the scope in pass state but take all response data out before saving it.
    pub fn take(state: &mut State) -> Option<Self> {
        let responses = std::mem::take(&mut state.responses);
        let pass_responses = std::mem::take(&mut state.pass_responses);
        Some(Self {
            scope: state.response_scope.clone()?,
            responses,
            pass_responses,
        })
    }

    /// Replace the disposable file atomically; an interrupted write leaves the old one.
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        let mut file =
            tempfile::NamedTempFile::new_in(parent).context("create temporary response cache")?;
        let mut writer = std::io::BufWriter::new(file.as_file_mut());
        serde_json::to_writer(&mut writer, self).context("encode response cache")?;
        use std::io::Write;
        writer.flush().context("flush response cache")?;
        drop(writer);
        file.persist(path).context("replace response cache")?;
        Ok(())
    }
}

impl Responses {
    fn observe_item(
        &mut self,
        repo: &str,
        number: &Value,
        version: Option<Version>,
        writes: usize,
    ) {
        let Some(at) = number.as_u64().and_then(|n| item_path(repo, n)) else {
            return;
        };
        if let Some(version) = version {
            if self.observations.len() < MAX_OBSERVATIONS || self.observations.contains_key(&at) {
                self.observations.insert(at, (writes, version));
            }
        } else {
            self.observations.remove(&at);
        }
    }

    pub fn observe_board(&mut self, content: &Value, writes: usize) {
        self.observe_item(
            text(content, "/repository/nameWithOwner"),
            &content["number"],
            Version::read(&content["id"], &content["updatedAt"], None),
            writes,
        );
    }

    pub fn observe_rest(&mut self, url: &str, body: &Value, writes: usize) {
        let Some((path, _)) = url
            .strip_prefix(API_ROOT)
            .filter(|url| !url.contains('#'))
            .and_then(|url| url.split_once('?'))
        else {
            return;
        };
        let Some((repo, collection)) = path
            .strip_prefix("repos/")
            .and_then(|path| path.rsplit_once('/'))
        else {
            return;
        };
        if !matches!(collection, "issues" | "pulls") {
            return;
        }
        for node in body.as_array().into_iter().flatten() {
            let mut version = Version::read(
                &node["node_id"],
                &node["updated_at"],
                (collection == "pulls").then_some(&node["head"]["sha"]),
            );
            if collection == "issues" && node.get("pull_request").is_some() {
                // This listing cannot attest to a head. It can corroborate
                // the identity and version of a complete, still-live read.
                let same = node["number"]
                    .as_u64()
                    .and_then(|n| item_path(repo, n))
                    .and_then(|at| self.observations.get(&at))
                    .is_some_and(|(generation, prior)| {
                        *generation == writes
                            && prior.head.is_some()
                            && version
                                .as_ref()
                                .is_some_and(|v| prior.id == v.id && prior.updated == v.updated)
                    });
                if same {
                    continue;
                }
                version = None;
            }
            self.observe_item(repo, &node["number"], version, writes);
        }
    }

    pub fn graphql_hit(
        &mut self,
        at: &str,
        query: ItemQuery,
        key: &str,
        writes: usize,
    ) -> Option<Value> {
        self.graphql_lookups += 1;
        let (_, version) = self
            .observations
            .get(at)
            .filter(|(seen, _)| *seen == writes)?;
        self.graphql_listed += 1;
        let entry = self.entries.get(key)?;
        let body = &entry["body"];
        let read_at = entry["read_at"]
            .as_str()
            .and_then(|at| OffsetDateTime::parse(at, &Rfc3339).ok())?;
        if query.version(body).as_ref() != Some(version) || version.updated + READ_SKEW >= read_at {
            return None;
        }
        let body = body.clone();
        self.keep_entry(key.to_owned(), entry.clone());
        self.graphql_reused += 1;
        Some(body)
    }

    pub fn keep_graphql(&mut self, key: String, query: ItemQuery, body: &Value) {
        let Some(read_at) = self.now.and_then(|now| now.format(&Rfc3339).ok()) else {
            return;
        };
        if query.version(body).is_some() {
            self.keep_entry(key, json!({"body": body, "read_at": read_at}));
        }
    }

    pub fn graphql_key(at: &str, versioned: &str) -> String {
        format!(
            "{at}{GRAPHQL_REPRESENTATION}{:x}",
            Sha256::digest(versioned)
        )
    }

    pub fn seed(&mut self, entries: BTreeMap<String, Value>) {
        self.index = Index::default();
        for (key, entry) in &entries {
            self.stamp = self.stamp.max(last_used(entry));
            self.index.insert(key, entry, entry_bytes(key, entry));
        }
        self.entries = entries;
        let limit = self.limit();
        self.index.bound(&mut self.entries, limit);
    }

    fn keep_entry(&mut self, key: String, mut entry: Value) {
        let old = self.entries.remove(&key);
        if let Some(old) = &old {
            self.index.remove(&key, old);
        }
        // Resume after all the saved uses, including after a restart. Order
        // uses within the pass too, so a just-read late URL never loses a tie.
        self.stamp = self.stamp.saturating_add(1);
        entry["last_used"] = self.stamp.into();
        let entry = seal(entry);
        let size = entry_bytes(&key, &entry);
        if size > MAX_ENTRY_BYTES {
            self.oversized += 1;
            if old.is_some() {
                self.evicted.insert(key);
            }
            return;
        }
        self.index.insert(&key, &entry, size);
        self.evicted.remove(&key);
        self.entries.insert(key, entry);
        let limit = self.limit();
        self.evicted
            .append(&mut self.index.bound(&mut self.entries, limit).evicted);
    }

    pub fn get(&mut self, url: &str) -> Option<Value> {
        let key = format!("{url}{REPRESENTATION}");
        let entry = self.entries.get(&key).cloned();
        if let Some(entry) = &entry {
            self.hits += 1;
            self.keep_entry(key, entry.clone());
        } else {
            self.misses += 1;
            self.evicted_misses += usize::from(self.evicted.contains(&key));
        }
        entry
    }

    pub fn entry(&self, url: &str) -> Option<&Value> {
        self.entries.get(&format!("{url}{REPRESENTATION}"))
    }

    pub fn keep_validated(
        &mut self,
        url: &str,
        response: &RestResponse,
        validated: Option<&Value>,
    ) {
        if response.status != 200 || response.etag.is_none() {
            return;
        }
        let key = format!("{url}{REPRESENTATION}");
        let mut entry = json!({
            "etag": response.etag, "link": response.link, "body": response.body
        });
        if let Some(old) = validated.filter(|old| old["comment_evidence"].is_object()) {
            self.add_comment_evidence(
                &key,
                &mut entry,
                &old["comment_query"],
                &old["comment_evidence"],
            );
        }
        self.keep_entry(key, entry);
    }

    pub fn keep_comment_evidence(
        &mut self,
        url: &str,
        response: &RestResponse,
        query: String,
        data: &Value,
    ) {
        let key = format!("{url}{REPRESENTATION}");
        let Some(mut entry) = self
            .entries
            .get(&key)
            .filter(|entry| {
                entry["body"] == response.body && entry["etag"].as_str() == response.etag.as_deref()
            })
            .cloned()
        else {
            return;
        };
        if self.add_comment_evidence(&key, &mut entry, &query.into(), data) {
            self.keep_entry(key, entry);
        }
    }

    /// Optional editor evidence must not force out its REST representation.
    fn add_comment_evidence(
        &self,
        key: &str,
        entry: &mut Value,
        query: &Value,
        evidence: &Value,
    ) -> bool {
        let mut enriched = entry.clone();
        enriched["comment_query"] = query.clone();
        enriched["comment_evidence"] = evidence.clone();
        enriched["last_used"] = self.stamp.saturating_add(1).into();
        enriched = seal(enriched);
        if entry_bytes(key, &enriched) > MAX_ENTRY_BYTES {
            return false;
        }
        *entry = enriched;
        true
    }

    /// The pass's one eviction line, with `last` the bound of the state it
    /// keeps; never a URL, only counts against the caps.
    pub fn evictions(&self, last: &Bounded) -> String {
        format!(
            "cache: evicted {} disposable responses this pass, {} more on save; kept {}/{MAX_ENTRIES} entries, {}/{} bytes on save ({} in pass); {} oversized responses rejected",
            self.evicted.len(),
            last.evicted.difference(&self.evicted).count(),
            last.entries,
            last.bytes,
            last.limit,
            self.limit(),
            self.oversized,
        )
    }

    /// The misses: those on responses this pass evicted, and the rest, which
    /// the cache did not hold (never kept or inherited away).
    pub fn misses(&self) -> String {
        format!(
            "misses {} ({} evicted this pass, {} not cached)",
            self.misses,
            self.evicted_misses,
            self.misses - self.evicted_misses
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Forge {
        identity: Cell<u64>,
        version: Cell<u64>,
        reads: Cell<usize>,
        conditional: Cell<usize>,
        link: RefCell<Option<String>>,
        listing: RefCell<Option<Value>>,
        item: RefCell<Value>,
        graphql_reads: Cell<usize>,
    }

    impl Rest for Forge {
        fn cache_context(&self) -> Option<String> {
            Some(self.identity.get().to_string())
        }

        fn get(&self, _: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
            self.reads.set(self.reads.get() + 1);
            self.conditional
                .set(self.conditional.get() + usize::from(validator.is_some()));
            let etag = self.version.get().to_string();
            let matched = validator == Some(("If-None-Match", etag.as_str()));
            Ok(RestResponse {
                status: if matched { 304 } else { 200 },
                etag: Some(etag),
                last_modified: None,
                link: self.link.borrow().clone(),
                body: if matched {
                    Value::Null
                } else {
                    self.listing
                        .borrow()
                        .clone()
                        .unwrap_or_else(|| json!([self.version.get()]))
                },
            })
        }

        fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
            self.version.set(self.version.get() + 1);
            bail!("ambiguous write reply")
        }
    }

    impl Transport for Forge {
        fn graphql(&self, _: &str, _: Value) -> Result<Value> {
            self.graphql_reads.set(self.graphql_reads.get() + 1);
            Ok(self.item.borrow().clone())
        }
    }

    fn bounds() -> Bounds {
        let bounds = Bounds::new(
            PASS_REQUEST_LIMIT,
            Instant::now() + Duration::from_secs(PASS_SECONDS_LIMIT),
        );
        bounds.shared_responses.borrow_mut().enabled = true;
        bounds.shared_responses.borrow_mut().context = Some("0".into());
        bounds
    }

    #[test]
    fn dispatcher_credentials_never_consume_the_primary_representation() {
        let primary = Forge::default();
        let dispatcher = Forge::default();
        dispatcher.identity.set(1);
        dispatcher.version.set(99);
        let bounds = bounds();
        let client = Budget {
            inner: &primary,
            bounds: &bounds,
        };
        let other = Budget {
            inner: &dispatcher,
            bounds: &bounds,
        };
        let url = format!("{API_ROOT}repos/o/r/issues/1");
        bounds.cache_enabled.set(true);
        bounds
            .responses
            .borrow_mut()
            .insert(url.clone(), json!({"body":[0], "etag":"0"}));
        assert_eq!(client.get(&url, None).unwrap().body, json!([0]));
        assert_eq!(other.get(&url, None).unwrap().body, json!([99]));
        dispatcher.identity.set(2); // Rotating DISPATCH_TOKEN also stays cold.
        assert_eq!(other.get(&url, None).unwrap().body, json!([99]));
        assert_eq!(dispatcher.conditional.get(), 0);
        assert_eq!(bounds.shared_responses.borrow().entries.len(), 1);
        assert_eq!(client.get(&url, None).unwrap().body, json!([0]));
        assert_eq!(primary.conditional.get(), 1);
    }

    #[test]
    fn cold_warm_lost_and_changed_responses_are_live() {
        let forge = Forge::default();
        let bounds = bounds();
        let client = Budget {
            inner: &forge,
            bounds: &bounds,
        };
        let url = format!("{API_ROOT}repos/o/r/issues");
        assert_eq!(client.get(&url, None).unwrap().body, json!([0]));
        assert_eq!(client.get(&url, None).unwrap().body, json!([0]));
        assert_eq!(
            (bounds.spent().requests, bounds.spent().revalidated),
            (1, 1)
        );
        bounds.shared_responses.borrow_mut().seed(BTreeMap::new());
        assert_eq!(client.get(&url, None).unwrap().body, json!([0]));
        forge.version.set(1); // A concurrent edit is never hidden by the cache.
        assert_eq!(client.get(&url, None).unwrap().body, json!([1]));
        assert_eq!(forge.reads.get(), 4);
        assert_eq!(bounds.spent().requests, 3);
    }

    #[test]
    fn warm_pass_keeps_its_working_set_under_the_caps() {
        let forge = Forge::default();
        let cold = bounds();
        cold.cache_enabled.set(true);
        cold.shared_responses.borrow_mut().graphql_enabled = true;
        cold.shared_responses.borrow_mut().now =
            Some(OffsetDateTime::parse("2026-11-08T00:10:00Z", &Rfc3339).unwrap());
        let client = Budget {
            inner: &forge,
            bounds: &cold,
        };
        let item = json!({"repository": {"issue": {
            "id": "I_1", "updatedAt": "2026-11-08T00:00:00Z", "body": "test plan"
        }}});
        *forge.item.borrow_mut() = item.clone();
        assert_eq!(
            client.graphql_item(ItemQuery::Body, "o/r", 1).unwrap(),
            (item.clone(), false)
        );

        // Fits the shared response cap, but writing the same bodies to the
        // legacy pass map as well exceeds the 32 MiB saved-state cap.
        let urls: Vec<_> = (1..=88)
            .map(|n| format!("{API_ROOT}repos/o/r/issues/{n}/comments"))
            .collect();
        *forge.listing.borrow_mut() = Some(json!(["x".repeat(200 * 1024)]));
        for url in &urls {
            assert_eq!(client.get(url, None).unwrap().status, 200);
        }
        let mut state = State {
            interval_seconds: poll::MIN_INTERVAL_SECONDS,
            responses: cold.shared_responses.take().entries,
            pass_responses: cold.next_responses.take(),
            ..State::default()
        };
        state.response_scope = Some("scope".into());
        let saved = bound_embedded(&mut state);
        assert!(saved.entries < MAX_ENTRIES && saved.bytes < MAX_BYTES);
        assert!(saved.evicted.is_empty());

        let warm = bounds();
        let mut responses = warm.shared_responses.borrow_mut();
        responses.graphql_enabled = true;
        responses.seed(State::from_json(&state.to_json()).unwrap().responses);
        responses.observe_board(
            &json!({"number": 1, "id": "I_1", "updatedAt": "2026-11-08T00:00:00Z",
            "repository": {"nameWithOwner": "o/r"}}),
            0,
        );
        drop(responses);
        let client = Budget {
            inner: &forge,
            bounds: &warm,
        };
        assert_eq!(
            client.graphql_item(ItemQuery::Body, "o/r", 1).unwrap(),
            (item, true)
        );
        for url in &urls {
            assert_eq!(client.get(url, None).unwrap().status, 200);
        }
        let responses = warm.shared_responses.borrow();
        assert_eq!(forge.conditional.get(), urls.len());
        assert_eq!((responses.misses, responses.evicted_misses), (0, 0));
        assert!(responses.evicted.is_empty());
    }

    #[test]
    fn link_growth_absence_and_removal_on_304() {
        let forge = Forge::default();
        let bounds = bounds();
        let client = Budget {
            inner: &forge,
            bounds: &bounds,
        };
        let url = format!("{API_ROOT}repos/o/r/issues?per_page=100");
        let next = format!("<{url}&page=2>; rel=\"next\"");
        assert_eq!(client.get(&url, None).unwrap().link, None);
        *forge.link.borrow_mut() = Some(next.clone());
        assert_eq!(client.get(&url, None).unwrap().link, Some(next.clone()));
        *forge.link.borrow_mut() = None;
        assert_eq!(client.get(&url, None).unwrap().link, Some(next));
        *forge.link.borrow_mut() = Some(format!("<{url}>; rel=\"first\""));
        assert!(
            !client
                .get(&url, None)
                .unwrap()
                .link
                .unwrap()
                .contains("next")
        );
    }

    #[test]
    fn prewrite_reads_bypass_and_ambiguous_mutations_keep_validators() {
        let forge = Forge::default();
        let bounds = bounds();
        let client = Budget {
            inner: &forge,
            bounds: &bounds,
        };
        let url = format!("{API_ROOT}repos/o/r/issues/1");
        client.get(&url, None).unwrap();
        forge.version.set(1);
        assert_eq!(client.get_live(&url).unwrap().body, json!([1]));
        bounds.starting.set(true);
        client.get(&url, None).unwrap();
        bounds.starting.set(false);
        bounds.finishing.set(true);
        client.get(&url, None).unwrap();
        bounds.finishing.set(false);
        assert_eq!(forge.conditional.get(), 0);
        // The write may have committed: its resource reads fresh, through the
        // validator it kept.
        assert!(client.send("PATCH", &url, &json!({})).is_err());
        assert_eq!(bounds.shared_responses.borrow().entries.len(), 1);
        assert_eq!(client.get(&url, None).unwrap().body, json!([2]));
        assert_eq!(forge.conditional.get(), 1);
        forge.version.set(3);
        let _ = client.graphql_spent(
            true,
            "mutation",
            0,
            Err(anyhow::anyhow!("ambiguous mutation")),
        );
        assert_eq!(bounds.shared_responses.borrow().entries.len(), 1);
        assert_eq!(client.get(&url, None).unwrap().body, json!([3]));
        assert_eq!(forge.conditional.get(), 2);
    }

    #[test]
    fn corruption_eviction_and_scope_changes_preserve_pending_intake() {
        let url = format!("{API_ROOT}repos/o/r/issues/1{REPRESENTATION}");
        let mut state = State {
            interval_seconds: poll::MIN_INTERVAL_SECONDS,
            ..State::default()
        };
        state
            .intake_waiting
            .insert("o/r#1".into(), "2026-11-08T00:00:00Z".into());
        state.intake_refused.push("o/r#1/issuecomment-2".into());
        state.bind_responses("host-identity-config-one");
        // Decided under one scope, they are not carried to another.
        state.quiet_waits = poll::quiet::from_json(Some(&json!({"o/r#2": {
            "head": "abc", "updated_at": "2026-11-08T00:00:00Z",
            "read_at": "2026-11-08T00:00:00Z", "kind": "promote"}})))
        .unwrap();
        state
            .responses
            .insert(url.clone(), seal(json!({"body": [1], "etag": "1"})));
        let original = state.clone();
        let mut value = state.to_json();
        value["responses"][&url]["body"] = json!([2]);
        let decoded = State::from_json(&value).unwrap();
        assert!(decoded.responses.is_empty());
        assert_eq!(decoded.intake_waiting, original.intake_waiting);
        assert_eq!(decoded.intake_refused, original.intake_refused);
        for scope in [
            "host-two",
            "credential-two",
            "config-two",
            "representation-v2",
        ] {
            let mut changed = original.clone();
            changed.bind_responses(scope);
            assert!(changed.responses.is_empty());
            assert!(changed.quiet_waits.is_empty());
            assert_eq!(changed.intake_waiting, original.intake_waiting);
            assert_eq!(changed.intake_refused, original.intake_refused);
        }
        let mut entries = (0..MAX_ENTRIES + 1)
            .map(|n| (format!("{url}/{n}"), json!({"body": []})))
            .collect();
        let bounded = bound(&mut entries, MAX_BYTES);
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!((bounded.evicted.len(), bounded.entries), (1, MAX_ENTRIES));
        entries.insert("large".into(), json!({"body": "x".repeat(MAX_ENTRY_BYTES)}));
        let bounded = bound(&mut entries, MAX_BYTES);
        assert!(!entries.contains_key("large"));
        assert_eq!(bounded.evicted, BTreeSet::from(["large".to_owned()]));
    }

    #[test]
    fn rebinding_says_whether_the_scope_was_absent_and_never_its_digest() {
        let mut state = State {
            interval_seconds: poll::MIN_INTERVAL_SECONDS,
            ..State::default()
        };
        state
            .responses
            .insert(format!("{API_ROOT}a"), json!({"body": []}));
        assert_eq!(
            state.rebind_line("scope-one").unwrap(),
            "cache: response scope absent; cold reads, dropping 1 responses and 0 pass responses"
        );
        state.bind_responses("scope-one");
        assert_eq!(state.rebind_line("scope-one"), None);
        let line = state.rebind_line("scope-two").unwrap();
        assert_eq!(
            line,
            "cache: response scope changed; cold reads, dropping 0 responses and 0 pass responses"
        );
        assert!(!line.contains("scope-one") && !line.contains("scope-two"));
    }

    #[test]
    fn misses_split_into_evicted_this_pass_and_not_cached() {
        let mut responses = Responses::default();
        let ok = |etag: &str| RestResponse {
            status: 200,
            etag: Some(etag.into()),
            last_modified: None,
            link: None,
            body: json!([]),
        };
        // Refresh the first entry before inserting a late-sorting fresh URL.
        for n in 0..MAX_ENTRIES {
            responses.keep_validated(&format!("{API_ROOT}a/{n:04}"), &ok("1"), None);
        }
        assert!(responses.get(&format!("{API_ROOT}a/0000")).is_some());
        let mut restarted = Responses::default();
        restarted.seed(responses.entries.clone());
        restarted.keep_validated(&format!("{API_ROOT}b"), &ok("1"), None);
        assert!(restarted.get(&format!("{API_ROOT}b")).is_some());
        assert!(restarted.get(&format!("{API_ROOT}a/0000")).is_some());
        assert!(restarted.get(&format!("{API_ROOT}a/0001")).is_none());
        responses.keep_validated(&format!("{API_ROOT}b"), &ok("1"), None);
        assert!(responses.get(&format!("{API_ROOT}a/0001")).is_none());
        assert!(responses.get(&format!("{API_ROOT}c")).is_none());
        for _ in 0..2 {
            assert!(responses.get(&format!("{API_ROOT}oversized")).is_none());
            let mut large = ok("1");
            large.body = json!("x".repeat(MAX_ENTRY_BYTES));
            responses.keep_validated(&format!("{API_ROOT}oversized"), &large, None);
        }
        assert!(responses.get(&format!("{API_ROOT}a/0000")).is_some());
        assert_eq!(
            responses.misses(),
            "misses 4 (1 evicted this pass, 3 not cached)"
        );
        let mut state = responses.entries.clone();
        state.insert(format!("{API_ROOT}0"), json!({"body": []}));
        let last = bound(&mut state, MAX_BYTES);
        let line = responses.evictions(&last);
        assert_eq!(
            line,
            format!(
                "cache: evicted 1 disposable responses this pass, 1 more on save; kept {MAX_ENTRIES}/{MAX_ENTRIES} entries, {}/{MAX_BYTES} bytes on save ({MAX_BYTES} in pass); 2 oversized responses rejected",
                last.bytes
            )
        );
        assert!(!line.contains(API_ROOT));
    }

    #[test]
    fn saved_state_leaves_room_for_other_fields() {
        let mut state = State {
            interval_seconds: poll::MIN_INTERVAL_SECONDS,
            ..State::default()
        };
        state.bind_responses("scope");
        state
            .intake_waiting
            .insert("o/r#1".into(), "2026-11-08T00:00:00Z".into());
        let entry = seal(json!({"body": "x".repeat(192 * 1024)}));
        for n in 0..192 {
            let url = format!("{API_ROOT}repos/o/r/issues/{n}");
            state
                .responses
                .insert(format!("{url}{REPRESENTATION}"), entry.clone());
        }
        let separate = bound_separate(&mut state, ResponseCache::MAX_BYTES);
        assert!(separate.bytes as u64 > STATE_BYTES);
        assert_eq!(separate.entries, 192);
        let kept = bound_embedded(&mut state);
        assert!(kept.bytes > 23 * 1024 * 1024 && kept.bytes <= MAX_BYTES);
        // The older separate pass response map can itself use up to 16 MiB.
        for n in 0..80 {
            state
                .pass_responses
                .insert(format!("{API_ROOT}other/{n}"), entry.clone());
        }
        let kept = bound_embedded(&mut state);
        assert!(!kept.evicted.is_empty());
        let saved = serde_json::to_vec(&state.to_json()).unwrap();
        assert!(saved.len() as u64 <= STATE_BYTES);
        let decoded = State::from_json(&serde_json::from_slice(&saved).unwrap()).unwrap();
        assert_eq!(decoded.responses.len(), kept.entries);
        assert_eq!(decoded.intake_waiting, state.intake_waiting);
        assert_eq!(decoded.pass_responses.len(), 80);
    }

    #[test]
    fn separate_file_round_trips_only_scoped_and_bounded_responses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("responses.json");
        let mut state = State {
            interval_seconds: poll::MIN_INTERVAL_SECONDS,
            response_scope: Some("scope".into()),
            scan_cursor: Some("o/r#1".into()),
            ..State::default()
        };
        let url = format!("{API_ROOT}small{REPRESENTATION}");
        state
            .responses
            .insert(url.clone(), seal(json!({"body": ["distinctive body"]})));
        state.responses.insert(
            format!("{API_ROOT}large{REPRESENTATION}"),
            seal(json!({"body": "a".repeat(MAX_ENTRY_BYTES)})),
        );
        let evidence_url = format!("{API_ROOT}evidence");
        state.pass_responses.insert(
            evidence_url.clone(),
            json!({"body": [], "comment_evidence": {}}),
        );
        let kept = bound_separate(&mut state, MAX_BYTES);
        assert_eq!(kept.entries, 1, "the per-entry cap still applies");
        assert_eq!(kept.limit, MAX_BYTES, "both maps share the file budget");
        let cache = ResponseCache::take(&mut state).unwrap();
        let saved = state.to_json().to_string();
        assert!(!saved.contains("distinctive body"));
        assert!(state.responses.is_empty() && state.pass_responses.is_empty());
        let mut loaded = State::from_json(&serde_json::from_str(&saved).unwrap()).unwrap();
        cache.save(&path).unwrap();
        ResponseCache::restore(&path, MAX_BYTES, &mut loaded);
        assert_eq!(loaded.responses[&url]["body"], json!(["distinctive body"]));
        assert!(admissible(
            &evidence_url,
            &loaded.pass_responses[&evidence_url],
            true
        ));
        let warm = loaded.clone();
        // Written raw: these entries must be refused before they are decoded.
        let write_raw = |responses: String| {
            let file =
                format!(r#"{{"scope":"scope","responses":{{{responses}}},"pass_responses":{{}}}}"#);
            std::fs::write(&path, file).unwrap();
        };
        for problem in [
            "wrong scope",
            "no scope",
            "oversized",
            "dense entry",
            "too many entries",
            "corrupt JSON",
            "changed body",
            "missing",
        ] {
            let mut loaded = warm.clone();
            let limit = match problem {
                "wrong scope" => {
                    loaded.response_scope = Some("other scope".into());
                    MAX_BYTES
                }
                "no scope" => {
                    loaded.response_scope = None;
                    MAX_BYTES
                }
                "oversized" => 1,
                "dense entry" => {
                    let numbers = vec!["0"; MAX_ENTRY_BYTES / 2 + 1].join(",");
                    write_raw(format!(r#""{url}":{{"body":[{numbers}]}}"#));
                    MAX_BYTES
                }
                "too many entries" => {
                    let entries: Vec<_> = (0..=MAX_ENTRIES)
                        .map(|n| format!(r#""{API_ROOT}{n}":{{"body":[]}}"#))
                        .collect();
                    write_raw(entries.join(","));
                    MAX_BYTES
                }
                "corrupt JSON" => {
                    std::fs::write(&path, b"not JSON").unwrap();
                    MAX_BYTES
                }
                "changed body" => {
                    cache.save(&path).unwrap();
                    let bytes = std::fs::read_to_string(&path).unwrap();
                    std::fs::write(&path, bytes.replace("distinctive body", "other body")).unwrap();
                    MAX_BYTES
                }
                "missing" => {
                    std::fs::remove_file(&path).unwrap();
                    MAX_BYTES
                }
                _ => unreachable!(),
            };
            let scope = loaded.response_scope.clone();
            ResponseCache::restore(&path, limit, &mut loaded);
            assert_eq!(loaded.scan_cursor, state.scan_cursor, "{problem}");
            assert_eq!(loaded.response_scope, scope, "{problem}");
            assert!(
                loaded.responses.is_empty() && loaded.pass_responses.is_empty(),
                "{problem}"
            );
        }
    }

    #[test]
    fn graphql_items_reuse_only_matching_current_listings() {
        const UPDATED: &str = "2026-11-08T00:00:00Z";
        const CHANGED: &str = "2026-11-09T00:00:00Z";
        let head = "a".repeat(40);
        let forge = Forge::default();
        let item = |id, at, head: &str| {
            json!({"repository": {"issueOrPullRequest": {
                "__typename": "PullRequest", "id": id, "updatedAt": at, "headRefOid": head,
            }}})
        };
        let old = item("item-1", UPDATED, &head);
        *forge.item.borrow_mut() = old.clone();
        const READ: &str = "2026-11-08T00:10:00Z";
        let cold = bounds();
        cold.shared_responses.borrow_mut().graphql_enabled = true;
        cold.shared_responses.borrow_mut().now =
            Some(OffsetDateTime::parse(READ, &Rfc3339).unwrap());
        let client = Budget {
            inner: &forge,
            bounds: &cold,
        };
        let query = ItemQuery::Kind;
        assert_eq!(
            client.graphql_item(query, "o/r", 1).unwrap(),
            (old.clone(), false)
        );
        assert_eq!(forge.graphql_reads.get(), 1);
        let mut state = State {
            interval_seconds: poll::MIN_INTERVAL_SECONDS,
            ..State::default()
        };
        state.bind_responses("scope");
        state.responses = cold.shared_responses.borrow().entries.clone();
        let saved = State::from_json(&state.to_json()).unwrap().responses;
        assert!(saved.keys().all(|k| shared_key(k)));

        for (enabled, id, at, listed_head, writes, starting, same_context, reuse, settled) in [
            (
                true,
                "item-1",
                UPDATED,
                Some(&head),
                false,
                false,
                true,
                true,
                true,
            ),
            (
                true, "item-1", UPDATED, None, false, false, true, false, true,
            ),
            (
                true,
                "item-2",
                UPDATED,
                Some(&head),
                false,
                false,
                true,
                false,
                true,
            ),
            (
                true,
                "item-1",
                CHANGED,
                Some(&head),
                false,
                false,
                true,
                false,
                true,
            ),
            (
                true,
                "item-1",
                UPDATED,
                Some(&"b".repeat(40)),
                false,
                false,
                true,
                false,
                true,
            ),
            (
                true,
                "item-1",
                UPDATED,
                Some(&head),
                true,
                false,
                true,
                false,
                true,
            ),
            (
                true,
                "item-1",
                UPDATED,
                Some(&head),
                false,
                true,
                true,
                false,
                true,
            ),
            (
                true,
                "item-1",
                UPDATED,
                Some(&head),
                false,
                false,
                false,
                false,
                true,
            ),
            (
                false,
                "item-1",
                UPDATED,
                Some(&head),
                false,
                false,
                true,
                false,
                true,
            ),
            (
                true,
                "item-1",
                UPDATED,
                Some(&head),
                false,
                false,
                true,
                false,
                false,
            ),
        ] {
            forge.identity.set(0);
            forge.graphql_reads.set(0);
            let warm = bounds();
            warm.shared_responses.borrow_mut().graphql_enabled = enabled;
            let mut saved = saved.clone();
            if !settled {
                // Read in the listed version's own second, which a later
                // edit in that second keeps.
                for entry in saved.values_mut() {
                    entry["read_at"] = UPDATED.into();
                }
            }
            warm.shared_responses.borrow_mut().seed(saved);
            let client = Budget {
                inner: &forge,
                bounds: &warm,
            };
            *forge.listing.borrow_mut() = Some(json!([{"number": 1, "node_id": id,
                "updated_at": at, "head": {"sha": listed_head}}]));
            let url = format!("{API_ROOT}repos/o/r/pulls?state=open");
            if reuse {
                // The poll supplies its own validator and hands back the body
                // it just revalidated, not an unrevalidated saved listing.
                forge.version.set(0);
                assert_eq!(
                    client
                        .get(&url, Some(("If-None-Match", "0")))
                        .unwrap()
                        .status,
                    304
                );
                client.revalidated_listing(&url, forge.listing.borrow().as_ref().unwrap());
            } else {
                let validator = Some(("If-None-Match", "wrong"));
                assert_eq!(client.get(&url, validator).unwrap().status, 200);
            }
            if writes {
                assert!(client.send("PATCH", API_ROOT, &json!({})).is_err());
            }
            // The issues listing includes this PR but has no head OID. It
            // must neither erase matching evidence nor revive it after a write.
            client.revalidated_listing(
                &format!("{API_ROOT}repos/o/r/issues?state=open"),
                &json!([{"number": 1, "node_id": id, "updated_at": at, "pull_request": {}}]),
            );
            warm.starting.set(starting);
            forge.identity.set(u64::from(!same_context));
            let current = item("item-2", CHANGED, &head);
            *forge.item.borrow_mut() = current.clone();
            let prior_requests = warm.spent().requests;
            let read = client.graphql_item(query, "O/R", 1).unwrap();
            assert_eq!(read, (if reuse { old.clone() } else { current }, reuse));
            assert_eq!(forge.graphql_reads.get(), usize::from(!reuse));
            assert_eq!(warm.spent().requests - prior_requests, usize::from(!reuse));
            let cache = warm.shared_responses.borrow();
            assert_eq!(cache.graphql_reused, usize::from(reuse));
            let lookup = enabled && !starting && same_context;
            assert_eq!(cache.graphql_lookups, usize::from(lookup));
            assert_eq!(
                cache.graphql_listed,
                usize::from(lookup && !writes && listed_head.is_some())
            );
        }
    }
}
