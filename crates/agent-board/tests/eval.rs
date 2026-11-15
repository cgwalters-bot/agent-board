//! The offline eval command end to end; the summarizer's cases are its unit tests.
use assert_cmd::Command;

const EVAL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../eval");

fn summarize(manifest: &str, runs: &str) -> std::process::Output {
    Command::new(assert_cmd::cargo::cargo_bin!("agent-board"))
        .args(["eval", "summarize", manifest, runs])
        .output()
        .unwrap()
}

#[test]
fn pending_summary_bytes_truncated_oversized_input_and_usage() {
    let manifest = format!("{EVAL}/manifest.json");
    let good = summarize(&manifest, &format!("{EVAL}/runs.json"));
    assert!(
        good.status.success(),
        "{}",
        String::from_utf8_lossy(&good.stderr)
    );
    let summary = std::fs::read(format!("{EVAL}/summary.json")).unwrap();
    assert_eq!(good.stdout, summary);

    let dir = std::env::temp_dir().join(format!("agent-board-eval-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let runs = dir.join("runs.json");
    let runs = runs.to_str().unwrap();
    for (content, message) in [
        ("[{\"version\":".to_owned(), "parse"),
        (" ".repeat(4 * 1024 * 1024 + 1), "input exceeds 4 MiB"),
    ] {
        std::fs::write(runs, content).unwrap();
        let output = summarize(&manifest, runs);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
    }
    std::fs::remove_dir_all(&dir).unwrap();

    let usage = Command::new(assert_cmd::cargo::cargo_bin!("agent-board"))
        .args(["eval", "summarize"])
        .output()
        .unwrap();
    assert!(!usage.status.success() && usage.stdout.is_empty());
    assert!(String::from_utf8_lossy(&usage.stderr).contains("Usage:"));
}
