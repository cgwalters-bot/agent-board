//! Disposable REST and version-checked GraphQL representations in poll state.
//! The map is a rebuildable representation index, not action state.
use super::*;

pub(super) const REPRESENTATION: &str = "#agent-board-rest-v1";
const GRAPHQL_REPRESENTATION: &str = "#agent-board-graphql-item-v1-";
pub(crate) const MAX_ENTRIES: usize = 1024;
pub(crate) const MAX_BYTES: usize = 24 * 1024 * 1024;
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
    pub enabled: bool,
    pub context: Option<String>,
    pub entries: BTreeMap<String, Value>,
    index: Index,
    stamp: u64,
    pub hits: usize,
    pub misses: usize,
    pub modified: usize,
    pub graphql_reused: usize,
    pub graphql_enabled: bool,
    /// The pass's clock, recorded on each GraphQL item read kept: no later
    /// than the read itself.
    pub now: Option<OffsetDateTime>,
    observations: BTreeMap<String, (usize, Version)>,
    /// Keys this pass evicted and has not kept again, and the misses on them.
    pub evicted: BTreeSet<String>,
    pub evicted_misses: usize,
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

#[derive(Default)]
struct Index {
    sizes: BTreeMap<(u64, std::cmp::Reverse<String>), usize>,
    bytes: usize,
}

impl Index {
    fn insert(&mut self, url: &str, entry: &Value) {
        // Count the serialized key (including escaping), colon and comma too.
        let size = json!(url).to_string().len() + entry.to_string().len() + 2;
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
        }
    }
}

/// Eviction affects responses only, never pending asks, receipts or accounting.
pub(crate) fn bound(entries: &mut BTreeMap<String, Value>, limit: usize) -> Bounded {
    let mut index = Index::default();
    for (url, entry) in entries.iter() {
        index.insert(url, entry);
    }
    index.bound(entries, limit)
}

/// Reserve room for the rest of the saved state before keeping responses.
pub(super) fn bound_state(state: &mut State) -> Bounded {
    let mut entries = std::mem::take(&mut state.responses);
    let other_bytes = state.to_json().to_string().len();
    let limit = (STATE_BYTES as usize)
        .saturating_sub(other_bytes)
        .min(MAX_BYTES);
    let bounded = bound(&mut entries, limit);
    state.responses = entries;
    bounded
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
            // The issues endpoint shows PRs too, but cannot attest to their heads.
            let version = (!(collection == "issues" && node.get("pull_request").is_some()))
                .then(|| {
                    Version::read(
                        &node["node_id"],
                        &node["updated_at"],
                        (collection == "pulls").then_some(&node["head"]["sha"]),
                    )
                })
                .flatten();
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
        let (observed_writes, version) = self.observations.get(at)?;
        let entry = self.entries.get(key)?;
        let body = &entry["body"];
        let read_at = entry["read_at"]
            .as_str()
            .and_then(|at| OffsetDateTime::parse(at, &Rfc3339).ok())?;
        if *observed_writes != writes
            || query.version(body).as_ref() != Some(version)
            || version.updated + READ_SKEW >= read_at
        {
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
            self.index.insert(key, entry);
        }
        self.entries = entries;
        self.index.bound(&mut self.entries, MAX_BYTES);
    }

    fn keep_entry(&mut self, key: String, mut entry: Value) {
        if let Some(old) = self.entries.get(&key) {
            self.index.remove(&key, old);
        }
        // Resume after all the saved uses, including after a restart. Order
        // uses within the pass too, so a just-read late URL never loses a tie.
        self.stamp = self.stamp.saturating_add(1);
        entry["last_used"] = self.stamp.into();
        let entry = seal(entry);
        self.index.insert(&key, &entry);
        self.evicted.remove(&key);
        self.entries.insert(key, entry);
        self.evicted
            .append(&mut self.index.bound(&mut self.entries, MAX_BYTES).evicted);
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

    pub fn keep(&mut self, url: &str, response: &RestResponse) {
        if response.status != 200 || response.etag.is_none() {
            return;
        }
        let key = format!("{url}{REPRESENTATION}");
        self.keep_entry(
            key,
            json!({
                    "etag": response.etag, "link": response.link, "body": response.body
            }),
        );
    }

    /// The pass's one eviction line, with `last` the bound of the state it
    /// keeps; never a URL, only counts against the caps.
    pub fn evictions(&self, last: &Bounded) -> String {
        format!(
            "cache: evicted {} disposable responses this pass, {} more on save; kept {}/{MAX_ENTRIES} entries, {}/{MAX_BYTES} bytes",
            self.evicted.len(),
            last.evicted.difference(&self.evicted).count(),
            last.entries,
            last.bytes,
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
        let _ = client.graphql_spent(true, Err(anyhow::anyhow!("ambiguous mutation")));
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
            responses.keep(&format!("{API_ROOT}a/{n:04}"), &ok("1"));
        }
        assert!(responses.get(&format!("{API_ROOT}a/0000")).is_some());
        let mut restarted = Responses::default();
        restarted.seed(responses.entries.clone());
        restarted.keep(&format!("{API_ROOT}b"), &ok("1"));
        assert!(restarted.get(&format!("{API_ROOT}b")).is_some());
        assert!(restarted.get(&format!("{API_ROOT}a/0000")).is_some());
        assert!(restarted.get(&format!("{API_ROOT}a/0001")).is_none());
        responses.keep(&format!("{API_ROOT}b"), &ok("1"));
        assert!(responses.get(&format!("{API_ROOT}a/0001")).is_none());
        assert!(responses.get(&format!("{API_ROOT}c")).is_none());
        assert!(responses.get(&format!("{API_ROOT}a/0000")).is_some());
        assert_eq!(
            responses.misses(),
            "misses 2 (1 evicted this pass, 1 not cached)"
        );
        let mut state = responses.entries.clone();
        state.insert(format!("{API_ROOT}0"), json!({"body": []}));
        let last = bound(&mut state, MAX_BYTES);
        let line = responses.evictions(&last);
        assert_eq!(
            line,
            format!(
                "cache: evicted 1 disposable responses this pass, 1 more on save; kept {MAX_ENTRIES}/{MAX_ENTRIES} entries, {}/{MAX_BYTES} bytes",
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
        let kept = bound_state(&mut state);
        assert!(kept.bytes > 23 * 1024 * 1024 && kept.bytes <= MAX_BYTES);
        // The older separate pass response map can itself use up to 16 MiB.
        for n in 0..80 {
            state
                .pass_responses
                .insert(format!("{API_ROOT}other/{n}"), entry.clone());
        }
        let kept = bound_state(&mut state);
        assert!(!kept.evicted.is_empty());
        let saved = serde_json::to_vec(&state.to_json()).unwrap();
        assert!(saved.len() as u64 <= STATE_BYTES);
        let decoded = State::from_json(&serde_json::from_slice(&saved).unwrap()).unwrap();
        assert_eq!(decoded.responses.len(), kept.entries);
        assert_eq!(decoded.intake_waiting, state.intake_waiting);
        assert_eq!(decoded.pass_responses.len(), 80);
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
            warm.starting.set(starting);
            forge.identity.set(u64::from(!same_context));
            let current = item("item-2", CHANGED, &head);
            *forge.item.borrow_mut() = current.clone();
            let prior_requests = warm.spent().requests;
            let read = client.graphql_item(query, "O/R", 1).unwrap();
            assert_eq!(read, (if reuse { old.clone() } else { current }, reuse));
            assert_eq!(forge.graphql_reads.get(), usize::from(!reuse));
            assert_eq!(warm.spent().requests - prior_requests, usize::from(!reuse));
            assert_eq!(
                warm.shared_responses.borrow().graphql_reused,
                usize::from(reuse)
            );
        }
    }
}
