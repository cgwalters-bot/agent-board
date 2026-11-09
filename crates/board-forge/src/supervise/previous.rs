//! Bounded previous-report lookup, ported from tracker supervisor.yml at
//! 82d4ef6ec3f6524b328211ffe8adcf33a72cfe7a, lines 127–158.
//! A failed read is unknown, never proof that no previous report exists.
use super::{Api, Bounded, Limits, encode, millis, validate_path};
use crate::poll::validate_repository;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::{Cursor, Read};
use std::path::{Component, Path};

pub const CANDIDATES: usize = 10;
pub const REQUESTS: u32 = 12;
pub const SECONDS: u64 = 90;
pub const ARCHIVE_BYTES: u64 = 8 * 1024 * 1024;
pub const UNPACKED_BYTES: u64 = 8 * 1024 * 1024;
const ENTRIES: usize = 64;

/// Every identity is supplied by the trusted caller, not inferred from an artifact.
pub struct Policy {
    pub repository: String,
    pub workflow: String,
    pub artifact: String,
    pub branch: String,
    pub events: Vec<String>,
}

impl Policy {
    pub fn validate(&self) -> Result<()> {
        validate_repository(&self.repository)?;
        validate_path(&self.workflow)?;
        ensure!(
            self.workflow.starts_with(".github/workflows/")
                && (self.workflow.ends_with(".yml") || self.workflow.ends_with(".yaml")),
            "workflow must be a full .github/workflows/ path"
        );
        for (name, value) in [("artifact", &self.artifact), ("branch", &self.branch)] {
            ensure!(
                !value.is_empty() && value.len() <= 255 && !value.chars().any(char::is_control),
                "invalid {name}"
            );
        }
        ensure!(
            !self.events.is_empty() && self.events.len() <= 32,
            "supply 1 to 32 events"
        );
        for event in &self.events {
            ensure!(
                !event.is_empty()
                    && event.len() <= 64
                    && event.bytes().all(|c| c.is_ascii_lowercase() || c == b'_'),
                "invalid allowed event"
            );
        }
        Ok(())
    }
}

/// `None` is proved absence; errors, including ineligible-only listings, are unknown.
fn select(api: &dyn Api, p: &Policy) -> Result<Option<u64>> {
    let listing = api.get(&format!(
        "/repos/{}/actions/artifacts?name={}&per_page={CANDIDATES}",
        p.repository,
        encode(&p.artifact)
    ))?;
    let artifacts = listing["artifacts"]
        .as_array()
        .context("artifact listing is not a list")?;
    ensure!(
        artifacts.len() <= CANDIDATES,
        "artifact listing exceeds candidate bound"
    );
    let mut candidates = Vec::new();
    for artifact in artifacts {
        ensure!(
            artifact["name"].as_str() == Some(&p.artifact),
            "artifact name mismatch"
        );
        let expired = artifact["expired"]
            .as_bool()
            .context("artifact missing expired flag")?;
        if !expired {
            candidates.push((
                millis(&artifact["created_at"]).context("artifact missing creation time")?,
                artifact,
            ));
        }
    }
    candidates.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    if candidates.is_empty() {
        // An expired first page does not rule out a live report outside the
        // fixed lookup window. Missing or inconsistent counts are unknown too.
        ensure!(
            listing["total_count"].as_u64() == Some(artifacts.len() as u64),
            "artifact listing does not establish absence within candidate bound"
        );
        return Ok(None);
    }
    for (_, artifact) in candidates {
        let run_id =
            positive(&artifact["workflow_run"]["id"]).context("artifact missing run id")?;
        let run = api.get(&format!("/repos/{}/actions/runs/{run_id}", p.repository))?;
        // The artifact name alone admits another workflow or a fork's main branch.
        let repo_id = positive(&run["repository"]["id"]);
        if positive(&run["id"]) != Some(run_id)
            || run["path"].as_str() != Some(&p.workflow)
            || run["head_branch"].as_str() != Some(&p.branch)
            || !run["repository"]["full_name"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&p.repository))
            || repo_id.is_none()
            || positive(&run["head_repository"]["id"]) != repo_id
            || !run["event"]
                .as_str()
                .is_some_and(|e| p.events.iter().any(|v| v == e))
        {
            continue;
        }
        let id = positive(&artifact["id"]).context("artifact missing id")?;
        ensure!(
            artifact["size_in_bytes"]
                .as_u64()
                .is_some_and(|n| n <= ARCHIVE_BYTES),
            "artifact size missing or too large"
        );
        return Ok(Some(id));
    }
    anyhow::bail!("nonexpired artifacts exist but none has an eligible run")
}

fn positive(v: &Value) -> Option<u64> {
    v.as_u64().filter(|n| *n > 0)
}

/// Download and extract privately; publish a fresh directory with one rename.
/// Successful absence creates no directory. An existing destination is refused,
/// even on absence, so stale state cannot be mistaken for this lookup's result.
pub fn restore(
    api: &dyn Api,
    p: &Policy,
    destination: &Path,
    clock: &dyn Fn() -> i64,
) -> Result<()> {
    p.validate()?;
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure!(
        std::fs::metadata(parent)
            .context("read destination parent")?
            .is_dir(),
        "destination parent must be a directory"
    );
    ensure!(
        std::fs::symlink_metadata(destination)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "destination must not exist"
    );
    let limits = Limits {
        max_requests: REQUESTS,
        collection_timeout_seconds: SECONDS,
        ..Limits::default()
    };
    let deadline = clock() + SECONDS as i64 * 1000;
    let bounded = Bounded::new(api, &limits, clock);
    let Some(id) = select(&bounded, p)? else {
        return Ok(());
    };
    let archive = bounded.download(
        &format!("/repos/{}/actions/artifacts/{id}/zip", p.repository),
        ARCHIVE_BYTES,
    )?;
    ensure!(
        archive.len() as u64 <= ARCHIVE_BYTES,
        "archive exceeds download bound"
    );
    let staging = tempfile::Builder::new()
        .prefix(".previous-")
        .tempdir_in(parent)
        .context("create previous report staging directory")?;
    extract(&archive, staging.path())?;
    ensure!(clock() < deadline, "previous report deadline exhausted");
    std::fs::rename(staging.path(), destination).context("publish previous report directory")?;
    Ok(())
}

fn extract(bytes: &[u8], directory: &Path) -> Result<()> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).context("decode previous report zip")?;
    ensure!(
        !zip.is_empty() && zip.len() <= ENTRIES,
        "archive must contain 1 to {ENTRIES} entries"
    );
    let mut remaining = UNPACKED_BYTES;
    let mut names = BTreeSet::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).context("read archive entry")?;
        let name = entry.name().to_owned();
        let path = Path::new(&name);
        // Never materialize links, platform-specific paths, or traversal. No
        // permissions from the archive are applied to the fresh regular files.
        ensure!(
            name.len() <= 240
                && !name.contains(['\\', ':', '\0'])
                && !path.is_absolute()
                && path.components().all(|c| matches!(c, Component::Normal(_)))
                && entry.enclosed_name().is_some()
                && names.insert(name.clone()),
            "unsafe or duplicate archive path"
        );
        let kind = entry.unix_mode().unwrap_or(0) & 0o170000;
        ensure!(
            kind == 0 || kind == 0o100000 || (kind == 0o040000 && entry.is_dir()),
            "archive contains nonregular entry"
        );
        let target = directory.join(path);
        if entry.is_dir() {
            std::fs::create_dir_all(target).context("create archive directory")?;
            continue;
        }
        ensure!(entry.size() <= remaining, "archive exceeds unpacked bound");
        let mut contents = Vec::new();
        (&mut entry)
            .take(remaining + 1)
            .read_to_end(&mut contents)
            .context("read complete archive entry")?;
        ensure!(
            contents.len() as u64 <= remaining && contents.len() as u64 == entry.size(),
            "archive entry size mismatch or unpacked bound exceeded"
        );
        remaining -= contents.len() as u64;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).context("create archive parent")?;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)
            .context("create archive file")?;
        std::io::Write::write_all(&mut file, &contents).context("write archive file")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
