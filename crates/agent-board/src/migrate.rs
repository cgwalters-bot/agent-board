//! Standalone migration, never invoked by the scheduled pass.
use anyhow::{Context, Result, ensure};
use board_forge::pass::{Config, Recorder, Replay, migrate};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[derive(clap::Args)]
pub struct Migrate {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Preview tracker issue transfers to currently admitted midstream forks (docs/pass.md).
    MidstreamIssues(Options),
}

#[derive(clap::Args)]
struct Options {
    /// Trusted caller pass configuration; no new target authority.
    #[arg(long, value_name = "FILE", default_value = ".github/pass.json")]
    config: PathBuf,
    /// Transfer issues; otherwise perform read-only preview.
    #[arg(long, requires_all = ["max", "state"], conflicts_with_all = ["record", "replay"])]
    apply: bool,
    /// Positive per-run transfer cap; preview defaults to one.
    #[arg(long, value_name = "N")]
    max: Option<usize>,
    /// Durable transfer journal; retain it across retries. Required for apply.
    #[arg(long, value_name = "FILE")]
    state: Option<PathBuf>,
    /// Preview a recorded set of reads without credentials.
    #[arg(long, value_name = "FILE", conflicts_with = "record")]
    replay: Option<PathBuf>,
    /// Save live preview reads for offline replay (may contain issue bodies).
    #[arg(long, value_name = "FILE")]
    record: Option<PathBuf>,
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

// Serialize before creating the temporary file; rename in the same directory
// after syncing, so a failed write cannot erase the last durable intent.
fn save(path: &Path, journal: &migrate::Journal) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(journal)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("migration journal nonce: {e}"))?;
    let temporary = parent.join(format!("migration-{:032x}.tmp", u128::from_ne_bytes(nonce)));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .context("create migration journal temporary file")?;
    file.write_all(&bytes).context("write migration journal")?;
    file.sync_all().context("sync migration journal")?;
    std::fs::rename(&temporary, path).context("persist migration journal")?;
    std::fs::File::open(parent)?
        .sync_all()
        .context("sync migration journal directory")
}

struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn run(args: &Migrate, out: &mut impl Write) -> Result<()> {
    let Command::MidstreamIssues(args) = &args.command;
    let max = args.max.unwrap_or(1);
    ensure!(max > 0, "--max must be positive");
    // Two applies sharing a journal must not overwrite each other's pending
    // identities. A crashed run leaves the lock for explicit operator recovery.
    let _lock = if args.apply {
        let path = args
            .state
            .as_ref()
            .context("--state required for apply")?
            .with_extension("migration-lock");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .context(
                "migration journal is locked; inspect any crashed run before removing its lock",
            )?;
        Some(Lock(path))
    } else {
        None
    };
    let config = Config::parse_sources(&read(&args.config)?)?;
    let mut journal = match &args.state {
        Some(path) if path.exists() => {
            serde_json::from_str(&read(path)?).context("decode migration journal")?
        }
        _ => migrate::Journal::default(),
    };
    let mut emit = |line: String| -> Result<()> {
        writeln!(out, "{line}")?;
        out.flush().context("flush migration report")
    };
    let mut persist = |journal: &migrate::Journal| -> Result<()> {
        save(
            args.state.as_ref().context("--state required for apply")?,
            journal,
        )
    };
    if let Some(path) = &args.replay {
        let replay = Replay::from_json(&serde_json::from_str(&read(path)?)?)?;
        return migrate::run(
            &replay,
            &config,
            false,
            max,
            &mut journal,
            &mut persist,
            &mut emit,
        );
    }
    let client = crate::pass::pass_client()?;
    if let Some(path) = &args.record {
        let now = time::OffsetDateTime::now_utc();
        let recorder = Recorder::new(&client);
        let result = migrate::run(
            &recorder,
            &config,
            false,
            max,
            &mut journal,
            &mut persist,
            &mut emit,
        );
        std::fs::write(path, recorder.fixture(now)?)
            .with_context(|| format!("write {}", path.display()))?;
        result
    } else {
        migrate::run(
            &client,
            &config,
            args.apply,
            max,
            &mut journal,
            &mut persist,
            &mut emit,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_save_reload_and_replace() -> Result<()> {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("test nonce: {e}"))?;
        let directory = std::env::temp_dir().join(format!(
            "migration-test-{:032x}",
            u128::from_ne_bytes(nonce)
        ));
        std::fs::create_dir(&directory)?;
        let path = directory.join("journal.json");
        let mut value = serde_json::json!({"receipts":{"issue-1":{
            "before":{"id":"issue-1","source":"owner/tracker","number":1,"body":"","title":"repair","updated":"now","board":{},"parent":null,"labels":[],"milestone":null},
            "destination":"owner/fork","upstream":null,"canonical":null
        }}});
        for canonical in [
            serde_json::Value::Null,
            serde_json::json!("https://github.com/owner/fork/issues/99"),
        ] {
            value["receipts"]["issue-1"]["canonical"] = canonical;
            let journal: migrate::Journal = serde_json::from_value(value.clone())?;
            save(&path, &journal)?;
            let loaded: migrate::Journal = serde_json::from_str(&read(&path)?)?;
            assert_eq!(serde_json::to_value(loaded)?, value);
        }
        std::fs::remove_file(path)?;
        std::fs::remove_dir(directory)?;
        Ok(())
    }
}
