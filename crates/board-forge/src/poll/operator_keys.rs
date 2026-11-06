//! Key exemptions refine the existing operator-path gate, not a second gate.
//! A `FILE#/pointer` entry gates the whole JSON file except the values it
//! names, so feature switches can change on review alone while every other
//! value, including one a later release adds, still waits on the operator.
use super::*;

/// A small config, not arbitrary repository content. Each changed selected file
/// costs at most two contents reads, within the poll's global request limit.
const MAX_JSON_BYTES: usize = 64 * 1024;
pub(super) const UNCHANGED: &str = "operatorKeysUnchanged";

pub(super) fn selector(entry: &str) -> (&str, Option<&str>) {
    match entry.split_once('#') {
        Some((file, pointer)) => (file, Some(pointer)),
        None => (entry, None),
    }
}

pub(super) fn valid_pointer(pointer: &str) -> bool {
    if !pointer.starts_with('/') {
        return false;
    }
    let mut chars = pointer.chars();
    while let Some(c) = chars.next() {
        if c == '~' && !matches!(chars.next(), Some('0' | '1')) {
            return false;
        }
    }
    true
}

/// Drop each exempt object member; a pointer into an array exempts nothing.
fn without(mut doc: Value, pointers: &[&str]) -> Value {
    for pointer in pointers {
        let Some((parent, key)) = pointer.rsplit_once('/') else {
            continue;
        };
        if let Some(Value::Object(map)) = doc.pointer_mut(parent) {
            map.remove(&key.replace("~1", "/").replace("~0", "~"));
        }
    }
    doc
}

fn document(file: &Value) -> Result<Value> {
    ensure!(
        file["type"] == "file"
            && file["encoding"] == "base64"
            && file["size"]
                .as_u64()
                .is_some_and(|n| n <= MAX_JSON_BYTES as u64),
        "not a small regular JSON file"
    );
    let content = file["content"].as_str().context("missing content")?;
    // Bound allocation even if the API's size metadata is wrong, including
    // the line breaks in a base64 contents response.
    ensure!(
        content.len() <= MAX_JSON_BYTES * 2,
        "encoded JSON too large"
    );
    let bytes = crate::supervise::health::base64_decode(content)?;
    ensure!(bytes.len() <= MAX_JSON_BYTES, "JSON too large");
    let value: Value = serde_json::from_slice(&bytes).context("invalid JSON")?;
    ensure!(value.is_object(), "JSON config is not an object");
    Ok(value)
}

impl<R: Rest, G: Transport> Pass<'_, R, G> {
    /// Compare exact base/head commits, never moving refs or patch fragments.
    /// Missing/invalid content stays held by `operator_paths`, as do creations,
    /// deletions, renames and unread file lists. Only a successful comparison
    /// can release a file; whole-file entries remain independent.
    pub(super) fn operator_keys(&mut self, pull: &mut Value) {
        pull[UNCHANGED] = json!({});
        let Some(changed) = changed(pull) else {
            return;
        };
        let entries = self.config.policy.operator_paths.clone();
        let files: BTreeSet<&str> = entries
            .iter()
            .filter_map(|entry| {
                let (file, pointer) = selector(entry);
                (pointer.is_some() && changed.contains(file)).then_some(file)
            })
            .collect();
        for file in files {
            if !nodes(pull, "/files/nodes")
                .iter()
                .any(|f| f["path"] == file && f["changeType"] == "MODIFIED")
            {
                continue;
            }
            let read = |this: &mut Self, repository: &str, sha: &str| -> Result<Value> {
                validate_repository(repository)?;
                ensure!(
                    sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
                    "missing commit oid"
                );
                // Form encoding already escapes literal `+` as `%2B`, but a
                // path needs `%20` for spaces, not form encoding's `+`.
                let path = url::form_urlencoded::byte_serialize(file.as_bytes())
                    .collect::<String>()
                    .replace('+', "%20");
                document(&this.get(&format!("repos/{repository}/contents/{path}?ref={sha}"))?)
            };
            let base = read(
                self,
                text(pull, "/baseRepository/nameWithOwner"),
                text(pull, "/baseRefOid"),
            );
            let head = read(
                self,
                text(pull, "/headRepository/nameWithOwner"),
                text(pull, "/headRefOid"),
            );
            if let (Ok(base), Ok(head)) = (base, head) {
                let exempt: Vec<&str> = (entries.iter())
                    .filter_map(|entry| match selector(entry) {
                        (selected, pointer) if selected == file => pointer,
                        _ => None,
                    })
                    .collect();
                pull[UNCHANGED][file] = json!(without(base, &exempt) == without(head, &exempt));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_are_small_regular_json_objects() {
        let wrapper = |text: &str| {
            json!({"type": "file", "encoding": "base64", "size": text.len(),
            "content": crate::supervise::tests::base64(text)})
        };
        for text in ["{", "null", "[]", "{\"budget\":1} trailing"] {
            assert!(document(&wrapper(text)).is_err(), "{text}");
        }
        for (key, value) in [
            ("type", json!("symlink")),
            ("encoding", json!("none")),
            ("size", json!(MAX_JSON_BYTES + 1)),
            ("size", Value::Null),
            ("content", json!("!")),
            ("content", json!(" ".repeat(MAX_JSON_BYTES * 2 + 1))),
        ] {
            let mut file = wrapper("{}");
            file[key] = value;
            assert!(document(&file).is_err(), "{key}");
        }
        let mut oversized = wrapper(&format!("{{\"a\":\"{}\"}}", "x".repeat(MAX_JSON_BYTES)));
        oversized["size"] = 2.into();
        assert!(document(&oversized).is_err());
        let base = document(&wrapper(r#"{"a/b":{"~key":null}}"#)).unwrap();
        assert_eq!(base.pointer("/a~1b/~0key"), Some(&Value::Null));
        assert_ne!(base.pointer("/a~1b/~0key"), base.pointer("/missing"));
        assert!(document(&wrapper("{}")).is_ok());
    }

    #[test]
    fn exemptions_drop_only_named_object_members() {
        let doc = json!({"a/b": {"~k": 1, "x": 2}, "list": [1, 2], "y": 3});
        for (pointers, expected) in [
            (
                vec!["/a~1b/~0k"],
                json!({"a/b": {"x": 2}, "list": [1, 2], "y": 3}),
            ),
            (vec!["/a~1b", "/a~1b/x"], json!({"list": [1, 2], "y": 3})),
            (vec!["/list/0", "/missing", "/y/z"], doc.clone()),
        ] {
            assert_eq!(without(doc.clone(), &pointers), expected, "{pointers:?}");
        }
    }
}
