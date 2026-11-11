//! `agent-board changes` (docs/changes.md): without `--since`, the read-only
//! diff of the followed and midstream repositories since each one's
//! watermark ([`board_forge::changes::diff`]); with it, the earlier list of
//! what landed, closed, opened or changed hold on the live project, which
//! `main` keeps.
use anyhow::{Context, Result, ensure};
use board_forge::changes::diff::{self, Diff, Scope, State};
use board_forge::pass::{Config, Recorder, Replay};
use board_forge::poll::Rest;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The caller's configuration, in its checkout.
pub const DEFAULT_CONFIG: &str = ".github/pass.json";
/// The state file under the cache directory.
pub const STATE_FILE: &str = "changes.json";
/// The cache directory's name under `$XDG_CACHE_HOME` or `~/.cache`.
const CACHE_DIR: &str = "agent-board";

#[derive(clap::Args)]
pub struct Changes {
    /// The pass configuration (docs/pass.md) whose `follow` list and `midstream` owner name the repositories, and whose `operator` has comments flagged; from a trusted checkout, with credentials from GH_TOKEN, GITHUB_TOKEN or gh auth token [default: .github/pass.json].
    #[arg(long, value_name = "FILE", conflicts_with = "since")]
    pub config: Option<PathBuf>,
    /// Where the watermarks and ETags are kept, in changes.json [default: $XDG_CACHE_HOME/agent-board or ~/.cache/agent-board].
    #[arg(long, value_name = "DIR", conflicts_with = "since")]
    pub state: Option<PathBuf>,
    /// Move every read repository's watermark to this read's time; without it the next run lists the same changes again.
    #[arg(long, conflicts_with = "since")]
    pub commit: bool,
    /// Read a recorded diff (a replay fixture) at its clock instead of the forge; needs no token, and --state, so that a fixture's watermarks never land in the cache.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["since", "record"], requires = "state")]
    pub replay: Option<PathBuf>,
    /// Read live, and save every read and its answer to FILE as a replay fixture.
    #[arg(long, value_name = "FILE", conflicts_with = "since")]
    pub record: Option<PathBuf>,
    /// Instead, what landed, closed, opened or changed hold on the live project (--owner, --project) since this time, RFC 3339 (such as 2026-10-23T00:00:00Z), each line a link; --board-config's operator counts as an author whose new pull requests are listed.
    #[arg(long)]
    pub since: Option<String>,
    /// End of --since's window, RFC 3339; without it, the window runs to now.
    #[arg(long, requires = "since")]
    pub until: Option<String>,
    /// --since's output format.
    #[arg(long, default_value = "markdown", value_parser = ["markdown", "json"], requires = "since")]
    pub format: String,
    /// Leave out of --since's list the issues and pull requests of private repositories, for a list that will be published.
    #[arg(long, requires = "since")]
    pub public_only: bool,
    /// Trim --since's Markdown to at most N characters as a generated status update is trimmed: the longest sections lose their last lines first, each ending with `+K more`.
    #[arg(long, value_name = "N", requires = "since")]
    pub max_chars: Option<usize>,
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

/// `$XDG_CACHE_HOME/agent-board`, or `~/.cache/agent-board`.
fn default_state_dir() -> Result<PathBuf> {
    let cache = match std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        Some(cache) => PathBuf::from(cache),
        None => {
            let home = std::env::var_os("HOME").filter(|v| !v.is_empty());
            PathBuf::from(home.context("neither XDG_CACHE_HOME nor HOME is set")?).join(".cache")
        }
    };
    Ok(cache.join(CACHE_DIR))
}

/// The state file, or a fresh state where there is none yet.
fn load_state(path: &Path) -> Result<State> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let value = serde_json::from_str(&text)
                .with_context(|| format!("decode {}", path.display()))?;
            State::from_json(&value).with_context(|| format!("{}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// The configuration's repositories and operator, its organization entries
/// resolved through `client`.
fn scope(config: Config, client: &impl Rest) -> Result<Scope> {
    let config = match config.resolve(client, &mut Vec::new())? {
        Some(resolved) => resolved,
        None => config,
    };
    Ok(Scope {
        repositories: diff::discover(client, &config.follow, config.midstream.as_ref())?,
        operator: config.operator,
    })
}

/// Read the diff, emit it successfully, then persist the state, moving
/// watermarks only with `--commit`. Output errors acknowledge nothing.
pub fn run(args: &Changes, emit: impl FnOnce(&Diff) -> Result<()>) -> Result<()> {
    let path = args
        .config
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG));
    let config =
        Config::parse_sources(&read(&path)?).with_context(|| format!("{}", path.display()))?;
    ensure!(
        config.forgejo_board().is_none(),
        "unsupported_capability: changes reads GitHub repositories only"
    );
    let dir = match &args.state {
        Some(dir) => dir.clone(),
        None => default_state_dir()?,
    };
    let state_path = dir.join(STATE_FILE);
    let mut state = load_state(&state_path)?;
    let diff = match &args.replay {
        Some(path) => {
            // No client or credential: only the fixture, at its clock.
            let value = serde_json::from_str(&read(path)?)
                .with_context(|| format!("decode {}", path.display()))?;
            let replay =
                Replay::from_json(&value).with_context(|| format!("{}", path.display()))?;
            let scope = scope(config, &replay)?;
            diff::read(&replay, &scope, &mut state, replay.now)?
        }
        None => {
            let client = crate::pass::pass_client()?;
            let now = time::OffsetDateTime::now_utc();
            match &args.record {
                Some(path) => {
                    let recorder = Recorder::new(&client);
                    let scope = scope(config, &recorder)?;
                    let diff = diff::read(&recorder, &scope, &mut state, now)?;
                    std::fs::write(path, recorder.fixture(now)?)
                        .with_context(|| format!("write {}", path.display()))?;
                    diff
                }
                None => {
                    let scope = scope(config, &client)?;
                    diff::read(&client, &scope, &mut state, now)?
                }
            }
        }
    };
    emit(&diff)?;
    if args.commit {
        diff.commit(&mut state);
    }
    save_state(&state_path, &state)
}

/// Stage beside the destination so a failed write leaves the previous
/// watermarks intact. Exclusive creation also avoids following a stale link.
fn save_state(path: &Path, state: &State) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(&state.to_json()?)?;
    let dir = path.parent().context("state file has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("generate changes state temporary name: {error}"))?;
    let temporary = dir.join(format!("changes-{:032x}.tmp", u128::from_ne_bytes(random)));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    let result = (|| -> Result<()> {
        file.write_all(&bytes).context("write changes state")?;
        file.sync_all().context("sync changes state")?;
        std::fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_replacement_is_complete_and_failed_publication_leaves_no_temporary_file() {
        let dir = std::env::temp_dir().join(format!("changes-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(STATE_FILE);
        std::fs::write(&path, "old state").unwrap();
        save_state(&path, &State::default()).unwrap();
        assert_eq!(load_state(&path).unwrap(), State::default());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        // A failed rename must preserve the destination and clean up staging.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let old = path.join("old");
        std::fs::write(&old, "previous state").unwrap();
        assert!(save_state(&path, &State::default()).is_err());
        assert_eq!(std::fs::read_to_string(old).unwrap(), "previous state");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
