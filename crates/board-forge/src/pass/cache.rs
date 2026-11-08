//! Disposable full REST representations in the poll State's response map.
//! The map is the rebuildable URL/representation index, not action state.
use super::*;

pub(super) const REPRESENTATION: &str = "#agent-board-rest-v1";
pub(crate) const MAX_ENTRIES: usize = 1024;
pub(crate) const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 256 * 1024;

#[derive(Default)]
pub(super) struct Responses {
    pub enabled: bool,
    pub context: Option<String>,
    pub entries: BTreeMap<String, Value>,
    pub hits: usize,
    pub misses: usize,
    pub modified: usize,
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

/// Eviction affects responses only, never pending asks, receipts or accounting.
pub(crate) fn bound(entries: &mut BTreeMap<String, Value>) {
    let before = entries.len();
    let mut bytes = 0;
    let mut count = 0;
    entries.retain(|url, entry| {
        let size = url.len() + entry.to_string().len();
        let keep = size <= MAX_ENTRY_BYTES && count < MAX_ENTRIES && bytes + size <= MAX_BYTES;
        if keep {
            bytes += size;
            count += 1;
        }
        keep
    });
    if before != entries.len() {
        eprintln!(
            "cache: evicted {} oversized or excess disposable responses; cold reads",
            before - entries.len()
        );
    }
}

impl Responses {
    pub fn get(&mut self, url: &str) -> Option<Value> {
        let entry = self.entries.get(&format!("{url}{REPRESENTATION}")).cloned();
        if entry.is_some() {
            self.hits += 1;
        } else {
            self.misses += 1;
        }
        entry
    }

    pub fn keep(&mut self, url: &str, response: &RestResponse) {
        if response.status != 200 || response.etag.is_none() {
            return;
        }
        self.entries.insert(
            format!("{url}{REPRESENTATION}"),
            seal(json!({
                "etag": response.etag, "link": response.link, "body": response.body
            })),
        );
        bound(&mut self.entries);
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
                    json!([self.version.get()])
                },
            })
        }

        fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
            self.version.set(self.version.get() + 1);
            bail!("ambiguous write reply")
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
        bounds.shared_responses.borrow_mut().entries.clear();
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
    fn prewrite_reads_bypass_and_ambiguous_mutations_invalidate() {
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
        assert!(client.send("PATCH", &url, &json!({})).is_err());
        assert!(bounds.shared_responses.borrow().entries.is_empty());
        assert_eq!(client.get(&url, None).unwrap().body, json!([2]));
        assert_eq!(forge.conditional.get(), 0);
        let _ = client.graphql_spent(true, Err(anyhow::anyhow!("ambiguous mutation")));
        assert!(bounds.shared_responses.borrow().entries.is_empty());
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
            assert_eq!(changed.intake_waiting, original.intake_waiting);
            assert_eq!(changed.intake_refused, original.intake_refused);
        }
        let mut entries = (0..MAX_ENTRIES + 1)
            .map(|n| (format!("{url}/{n}"), json!({"body": []})))
            .collect();
        bound(&mut entries);
        assert_eq!(entries.len(), MAX_ENTRIES);
        entries.insert("large".into(), json!({"body": "x".repeat(MAX_ENTRY_BYTES)}));
        bound(&mut entries);
        assert!(!entries.contains_key("large"));
    }
}
