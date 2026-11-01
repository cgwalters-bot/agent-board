//! TLC harness for specs/, ported from specs/check.mjs in the shape of
//! cgwalters-forge/agentic-job's crates/agentic-job/src/bin/specs-check.rs.
//! Usage: `specs-check SPECS_DIR`. Each configuration must end with its exact
//! expected exit code and message, so a parse error, Java failure or passing
//! negative control fails the check instead of looking like a result.

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::fs::{DirBuilder, File};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const TLA_URL: &str = "https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar";

const TLA_SHA256: &str = "936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88";

const MODULE: &str = "DispatchJoin.tla";

const SAFE: &str = "Model checking completed. No error has been found.";

const VIOLATED: &str = "Invariant OneLiveRun is violated.";

/// TLC's exit code for an invariant violation.
const TLC_VIOLATION: i32 = 12;

/// Each configuration with the exit code it must end with.
const CASES: &[(&str, i32)] = &[
    ("Preview", 0),
    ("Current", TLC_VIOLATION),
    ("Broken", TLC_VIOLATION),
];

const TLC_TIMEOUT: Duration = Duration::from_secs(120);

/// Bounds the log read back from one TLC run.
const MAX_OUTPUT_BYTES: u64 = 8 << 20;

const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The message a configuration must print alongside `expected`.
fn expected_message(expected: i32) -> &'static str {
    if expected == 0 { SAFE } else { VIOLATED }
}

/// Whether a run ended as expected; `None` is a run killed or ended by a signal.
fn is_expected(expected: i32, status: Option<i32>, output: &str) -> bool {
    status == Some(expected) && output.contains(expected_message(expected))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn download(jar: &Path) -> Result<()> {
    let status = Command::new("curl")
        .args(["--fail", "--location", "--retry", "3"])
        .args(["--connect-timeout", "30", "--max-time", "180"])
        .arg("--output")
        .arg(jar)
        .arg(TLA_URL)
        .status()
        .context("running curl")?;
    ensure!(status.success(), "TLC download failed: curl {status}");
    let bytes = std::fs::read(jar).with_context(|| format!("reading {}", jar.display()))?;
    ensure!(
        sha256_hex(&bytes) == TLA_SHA256,
        "tla2tools.jar SHA-256 mismatch; refusing to execute it"
    );
    Ok(())
}

fn wait_bounded(child: &mut Child) -> Result<Option<ExitStatus>> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().context("waiting for java")? {
            return Ok(Some(status));
        }
        if start.elapsed() >= TLC_TIMEOUT {
            child.kill().context("killing java")?;
            child.wait().context("waiting for killed java")?;
            return Ok(None);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Runs TLC on one configuration, keeping its combined output in `CONFIG.log`.
fn run_tlc(
    java: &str,
    jar: &Path,
    specs: &Path,
    results: &Path,
    config: &str,
) -> Result<(Option<i32>, String)> {
    let log_path = results.join(format!("{config}.log"));
    let log =
        File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
    let mut child = Command::new(java)
        .args(["-XX:+UseParallelGC", "-Xmx512m", "-cp"])
        .arg(jar)
        .args([
            "tlc2.TLC", "-workers", "1", "-seed", "1", "-fp", "0", "-config",
        ])
        .arg(specs.join(format!("{config}.cfg")))
        .arg("-metadir")
        .arg(results.join(config))
        .arg(specs.join(MODULE))
        .current_dir(results)
        .stdin(Stdio::null())
        .stdout(log.try_clone().context("duplicating the log handle")?)
        .stderr(log)
        .spawn()
        .with_context(|| format!("running {java}"))?;
    let status = wait_bounded(&mut child)?;
    let mut bytes = Vec::new();
    File::open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?
        .take(MAX_OUTPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", log_path.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_OUTPUT_BYTES,
        "{config}: more than {MAX_OUTPUT_BYTES} bytes of TLC output"
    );
    let output = String::from_utf8_lossy(&bytes).into_owned();
    match status {
        Some(status) => Ok((status.code(), output)),
        None => bail!("{config}: TLC did not finish within {TLC_TIMEOUT:?}\n{output}"),
    }
}

fn check(specs: &Path, results: &Path) -> Result<()> {
    let jar = results.join("tla2tools.jar");
    download(&jar)?;
    let java = std::env::var("JAVA_BIN").unwrap_or_else(|_| "java".to_owned());
    for &(config, expected) in CASES {
        let (status, output) = run_tlc(&java, &jar, specs, results, config)?;
        let shown = status.map_or_else(|| "none".to_owned(), |c| c.to_string());
        println!("\n{config}: TLC exit {shown}\n{output}");
        ensure!(
            is_expected(expected, status, &output),
            "{config}: expected exit {expected} and '{}', not a tooling failure",
            expected_message(expected)
        );
    }
    println!("Expected results: Preview safe; Current and Broken violate OneLiveRun.");
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let (Some(specs), None) = (args.next(), args.next()) else {
        bail!("usage: specs-check SPECS_DIR");
    };
    let specs = std::path::absolute(PathBuf::from(specs)).context("resolving SPECS_DIR")?;
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    // Keep the downloaded jar, logs and TLC state files outside the checkout.
    let results = mktemp_dir(Path::new(&home))?;
    println!("Full TLC reports: {}", results.display());
    check(&specs, &results)
}

/// Creates a fresh `agent-board-tlc-*` directory under `home`, as mkdtemp does.
fn mktemp_dir(home: &Path) -> Result<PathBuf> {
    let mut bytes = [0u8; 6];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("generating a directory name: {e}"))?;
    let name: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let dir = home.join(format!("agent-board-tlc-{name}"));
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_results() {
        let cases: &[(i32, Option<i32>, &str, bool)] = &[
            (0, Some(0), SAFE, true),
            (0, Some(0), "", false),
            (0, Some(1), SAFE, false),
            (0, None, SAFE, false),
            (0, Some(0), VIOLATED, false),
            (TLC_VIOLATION, Some(TLC_VIOLATION), VIOLATED, true),
            (
                TLC_VIOLATION,
                Some(TLC_VIOLATION),
                "Invariant Other is violated.",
                false,
            ),
            (TLC_VIOLATION, Some(0), SAFE, false),
            (TLC_VIOLATION, Some(0), VIOLATED, false),
            (TLC_VIOLATION, Some(1), VIOLATED, false),
            (TLC_VIOLATION, None, VIOLATED, false),
        ];
        for &(expected, status, output, ok) in cases {
            assert_eq!(
                is_expected(expected, status, output),
                ok,
                "{expected} {status:?} {output:?}"
            );
        }
    }

    #[test]
    fn cases_match_configs() {
        let specs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../specs");
        let mut found: Vec<String> = std::fs::read_dir(&specs)
            .unwrap()
            .filter_map(|e| {
                let name = e.unwrap().file_name().into_string().unwrap();
                name.strip_suffix(".cfg").map(str::to_owned)
            })
            .collect();
        found.sort();
        let mut cases: Vec<&str> = CASES.iter().map(|&(c, _)| c).collect();
        cases.sort();
        assert_eq!(found, cases);
        assert!(specs.join(MODULE).is_file());
    }

    #[test]
    fn sha256_of_empty() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
