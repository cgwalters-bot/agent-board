use super::*;
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::io::Write;

const LIST: &str = "/repos/example/control/actions/artifacts?name=health-report&per_page=10";
const RUN: &str = "/repos/example/control/actions/runs/202";
const DOWNLOAD: &str = "/repos/example/control/actions/artifacts/102/zip";

// Sanitized recorded-response shapes; tests replay them without credentials or network.
struct Replay {
    responses: Value,
    calls: RefCell<Vec<String>>,
    fail: Option<&'static str>,
    archive: Vec<u8>,
    clock: Cell<i64>,
    advance: i64,
}

impl Replay {
    fn new() -> Self {
        Self {
            responses: serde_json::from_str(include_str!(
                "../../../../../fixtures/supervise/previous.json"
            ))
            .unwrap(),
            calls: RefCell::new(Vec::new()),
            fail: None,
            archive: archive(&[("health.json", b"[]"), ("times.json", b"{\"sweep\":null}")]),
            clock: Cell::new(0),
            advance: 0,
        }
    }

    fn call(&self, path: &str) -> Result<()> {
        self.calls.borrow_mut().push(path.to_owned());
        self.clock.set(self.clock.get() + self.advance);
        ensure!(self.fail != Some(path), "replayed failed read/download");
        Ok(())
    }
}

impl Api for Replay {
    fn get(&self, path: &str) -> Result<Value> {
        self.call(path)?;
        self.responses
            .get(path)
            .cloned()
            .context("unrecorded request")
    }
    fn download(&self, path: &str, limit: u64) -> Result<Vec<u8>> {
        self.call(path)?;
        assert_eq!(path, DOWNLOAD);
        assert_eq!(limit, ARCHIVE_BYTES);
        Ok(self.archive.clone())
    }
    fn graphql(&self, _: &str, _: Value) -> Result<Value> {
        panic!("unexpected GraphQL")
    }
    fn send(&self, _: &str, _: &str, _: &Value) -> Result<Value> {
        panic!("unexpected write")
    }
}

fn policy() -> Policy {
    Policy {
        repository: "example/control".into(),
        workflow: ".github/workflows/health.yml".into(),
        artifact: "health-report".into(),
        branch: "default".into(),
        events: vec![
            "schedule".into(),
            "workflow_dispatch".into(),
            "issue_comment".into(),
        ],
    }
}

fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn run(replay: &Replay, destination: &Path) -> Result<()> {
    restore(replay, &policy(), destination, &|| replay.clock.get())
}

#[test]
fn replays_newest_first_skipping_expired_and_selects_only_first_eligible() {
    let replay = Replay::new();
    let root = tempfile::tempdir().unwrap();
    let out = root.path().join("previous");
    run(&replay, &out).unwrap();
    assert_eq!(*replay.calls.borrow(), [LIST, RUN, DOWNLOAD]);
    assert_eq!(std::fs::read(out.join("health.json")).unwrap(), b"[]");
    assert_eq!(
        std::fs::read(out.join("times.json")).unwrap(),
        b"{\"sweep\":null}"
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    // Never reuse a previous destination, even on proved absence.
    assert!(run(&replay, &out).is_err());
    assert_eq!(replay.calls.borrow().len(), 3);
}

#[test]
fn foreign_runs_are_skipped_but_not_called_absence() {
    for (pointer, value) in [
        ("/id", json!(999)),
        ("/path", json!(".github/workflows/foreign.yml")),
        ("/head_branch", json!("foreign")),
        ("/repository/full_name", json!("foreign/control")),
        ("/repository/id", Value::Null),
        ("/repository/id", json!(0)),
        ("/head_repository/id", json!(99)),
        ("/head_repository/id", Value::Null),
        ("/event", json!("pull_request")),
        ("/event", Value::Null),
    ] {
        let mut replay = Replay::new();
        *replay.responses[RUN].pointer_mut(pointer).unwrap() = value;
        assert_eq!(select(&replay, &policy()).unwrap(), Some(101), "{pointer}");
        assert_eq!(
            *replay.calls.borrow(),
            [LIST, RUN, "/repos/example/control/actions/runs/201"]
        );
        replay.responses[LIST]["artifacts"] = json!([replay.responses[LIST]["artifacts"][2]]);
        assert!(select(&replay, &policy()).is_err(), "{pointer}");
    }
    for event in ["schedule", "workflow_dispatch", "issue_comment"] {
        let mut replay = Replay::new();
        replay.responses[RUN]["event"] = json!(event);
        assert_eq!(select(&replay, &policy()).unwrap(), Some(102));
    }
}

#[test]
fn absence_is_only_an_empty_nonexpired_list_and_creates_nothing() {
    for artifacts in [
        json!([]),
        json!([{"id":103,"name":"health-report","expired":true}]),
        json!(vec![
            json!({"name":"health-report","expired":true});
            CANDIDATES
        ]),
    ] {
        let mut replay = Replay::new();
        replay.responses[LIST]["total_count"] = json!(artifacts.as_array().unwrap().len());
        replay.responses[LIST]["artifacts"] = artifacts;
        let root = tempfile::tempdir().unwrap();
        run(&replay, &root.path().join("previous")).unwrap();
        assert_eq!(*replay.calls.borrow(), [LIST]);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
    for value in [
        Value::Null,
        json!({}),
        json!({"artifacts":null}),
        json!({"artifacts":[{}]}),
    ] {
        let mut replay = Replay::new();
        replay.responses[LIST] = value;
        assert!(select(&replay, &policy()).is_err());
    }
}

#[test]
fn incomplete_expired_listings_are_unknown() {
    for count in [json!(11), Value::Null, json!(-1), json!("10"), json!(9)] {
        let mut replay = Replay::new();
        let expired = replay.responses[LIST]["artifacts"][1].clone();
        replay.responses[LIST]["artifacts"] = json!(vec![expired; CANDIDATES]);
        replay.responses[LIST]["total_count"] = count.clone();
        // A live report may exist beyond this page. Never fetch a second page
        // or publish absence when the fixed window cannot establish exhaustion.
        let root = tempfile::tempdir().unwrap();
        assert!(
            run(&replay, &root.path().join("previous")).is_err(),
            "{count}"
        );
        assert_eq!(*replay.calls.borrow(), [LIST]);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
    let mut replay = Replay::new();
    replay.responses[LIST] = json!({"artifacts":[]});
    assert!(select(&replay, &policy()).is_err());
}

#[test]
fn destination_parent_must_exist_even_on_absence() {
    for absent in [false, true] {
        let mut replay = Replay::new();
        if absent {
            replay.responses[LIST] = json!({"total_count":0,"artifacts":[]});
        }
        let root = tempfile::tempdir().unwrap();
        for parent in ["missing", "file"] {
            if parent == "file" {
                std::fs::write(root.path().join(parent), b"").unwrap();
            }
            let error = run(&replay, &root.path().join(parent).join("previous")).unwrap_err();
            assert!(error.to_string().contains("parent"), "{error:#}");
            assert!(replay.calls.borrow().is_empty());
        }
    }
}

#[test]
fn failures_never_publish_a_partial_directory_or_fall_back() {
    for fail in [LIST, RUN, DOWNLOAD] {
        let mut replay = Replay::new();
        replay.fail = Some(fail);
        let root = tempfile::tempdir().unwrap();
        assert!(run(&replay, &root.path().join("previous")).is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        assert_eq!(replay.calls.borrow().last().unwrap(), fail);
    }
    let mut replay = Replay::new();
    replay.advance = 45_000;
    let root = tempfile::tempdir().unwrap();
    assert!(run(&replay, &root.path().join("previous")).is_err());
    assert_eq!(*replay.calls.borrow(), [LIST, RUN]);
    replay.advance = 30_000;
    replay.clock.set(0);
    assert!(run(&replay, &root.path().join("previous")).is_err());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn listing_and_request_bounds_are_fixed() {
    let mut replay = Replay::new();
    let candidate = replay.responses[LIST]["artifacts"][2].clone();
    replay.responses[LIST]["artifacts"] = json!(vec![candidate.clone(); 10]);
    replay.responses[RUN]["event"] = json!("pull_request");
    assert!(select(&replay, &policy()).is_err());
    assert_eq!(replay.calls.borrow().len(), 11);
    replay.calls.borrow_mut().clear();
    replay.responses[LIST]["artifacts"] = json!(vec![candidate; 11]);
    assert!(select(&replay, &policy()).is_err());
    assert_eq!(replay.calls.borrow().len(), 1);
}

#[test]
fn unsafe_corrupt_and_oversized_archives_leave_no_output() {
    let mut corrupt = archive(&[("health.json", b"complete-health-report")]);
    // Damage file data without updating its CRC.
    let at = corrupt.windows(4).position(|v| v == b"PK\x03\x04").unwrap();
    let name_len = u16::from_le_bytes([corrupt[at + 26], corrupt[at + 27]]) as usize;
    let extra_len = u16::from_le_bytes([corrupt[at + 28], corrupt[at + 29]]) as usize;
    corrupt[at + 30 + name_len + extra_len] ^= 0xff;
    let mut symlink = zip::ZipWriter::new(Cursor::new(Vec::new()));
    symlink
        .add_symlink("link", "outside", zip::write::SimpleFileOptions::default())
        .unwrap();
    let symlink = symlink.finish().unwrap().into_inner();
    let oversized = vec![0; UNPACKED_BYTES as usize + 1];
    let many: Vec<_> = (0..=ENTRIES)
        .map(|i| (format!("{i}.json"), b"".as_slice()))
        .collect();
    let many_refs: Vec<_> = many
        .iter()
        .map(|(name, bytes)| (name.as_str(), *bytes))
        .collect();
    for bytes in [
        b"not a zip".to_vec(),
        corrupt,
        symlink,
        archive(&[("health.json", b"[]"), ("../escape", b"bad")]),
        archive(&[("/absolute", b"bad")]),
        archive(&[("C:drive", b"bad")]),
        archive(&[("dir\\escape", b"bad")]),
        archive(&[("huge", &oversized)]),
        archive(&many_refs),
        archive(&[]),
        vec![0; ARCHIVE_BYTES as usize + 1],
    ] {
        let mut replay = Replay::new();
        replay.archive = bytes;
        let root = tempfile::tempdir().unwrap();
        assert!(run(&replay, &root.path().join("previous")).is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}

#[test]
fn malformed_candidates_and_inputs_fail_closed() {
    for (key, value) in [
        ("name", json!("foreign")),
        ("expired", Value::Null),
        ("created_at", json!("invalid")),
        ("workflow_run", json!({"id":0})),
        ("id", json!(0)),
        ("size_in_bytes", json!(ARCHIVE_BYTES + 1)),
    ] {
        let mut replay = Replay::new();
        replay.responses[LIST]["artifacts"][2][key] = value;
        assert!(select(&replay, &policy()).is_err(), "{key}");
    }
    let mut p = policy();
    p.repository = "../foreign".into();
    assert!(p.validate().is_err());
    p = policy();
    p.workflow = "health.yml".into();
    assert!(p.validate().is_err());
    p = policy();
    p.events.clear();
    assert!(p.validate().is_err());
    p = policy();
    p.artifact.clear();
    assert!(p.validate().is_err());
}
