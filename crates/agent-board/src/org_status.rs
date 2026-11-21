use anyhow::{Context, Result, bail, ensure};
use board_core::Snapshot;
use board_forge::github::Client;
use board_forge::org_status::{self, REQUEST_LIMIT, Settings, TIMEOUT_SECONDS};
use board_forge::poll::{Rest, RestResponse};
use board_forge::repository_lists::{RESOLUTION_SECONDS, Sources, check_source_fields};
use board_forge::supervise::{self, Bounded, Limits};
use serde_json::Value;
use std::cell::Cell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Discovery is bounded separately from the census so expanding policy
/// cannot exhaust the census budget before any section is collected.
struct Discovery {
    client: Client,
    remaining: Cell<u32>,
}

impl Discovery {
    fn new() -> Result<Self> {
        let deadline = Instant::now() + Duration::from_secs(RESOLUTION_SECONDS);
        Ok(Self {
            client: Client::from_env()?.retry_within(deadline),
            remaining: Cell::new(REQUEST_LIMIT),
        })
    }
}

impl Rest for Discovery {
    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
        let left = self
            .remaining
            .get()
            .checked_sub(1)
            .context("status repository discovery request limit reached")?;
        self.remaining.set(left);
        self.client.get(url, validator)
    }

    fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
        bail!("status repository discovery is read-only")
    }
}

#[derive(clap::Args)]
pub struct Status {
    /// The GitHub pass configuration (or standalone poll configuration) from a trusted default-branch checkout.
    #[arg(long, value_name = "FILE")]
    config: PathBuf,
    /// Concurrent hosted-job limit to compare running jobs against.
    #[arg(long, value_name = "N")]
    job_limit: Option<u64>,
    /// Show repository resolution and individual jobs, runs, and queue details.
    #[arg(long)]
    pub verbose: bool,
}

pub fn settings(args: &Status, owner: &str, fields: Option<&str>) -> Result<Settings> {
    Settings::check_args(owner, args.job_limit)?;
    let check_publications = |pools, needs_you| -> Result<()> {
        for field in fields.into_iter().flat_map(|f| f.split(',')) {
            ensure!(
                field != "pools" || pools,
                "--json pools needs the config's pace or usage publication"
            );
            ensure!(
                field != "needs_you" || needs_you,
                "--json needs_you needs holds.needsYou in the config"
            );
        }
        Ok(())
    };
    let path = &args.config;
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut lines = Vec::new();
    let settings = match board_forge::pass::Config::parse_sources(&text) {
        Ok(config) => {
            ensure!(
                config.forgejo_board().is_none(),
                "unsupported_capability: status is GitHub-only"
            );
            check_publications(
                config.usage.is_some(),
                config.holds.as_ref().is_some_and(|h| h.needs_you),
            )?;
            let config = if config.has_repository_sources() {
                config
                    .resolve(&Discovery::new()?, &mut lines)?
                    .context("pass repository sources were not resolved")?
            } else {
                config
            };
            Settings::from_pass(owner, &config, args.job_limit)?
        }
        Err(pass_error) => {
            // Keep accepting the older standalone poll format, but do not
            // let a malformed pass file fall back to an unchecked JSON map.
            check_source_fields(&text).with_context(|| format!("decode {}", path.display()))?;
            let mut config: Value = serde_json::from_str(&text).context("status config")?;
            board_forge::poll::check_config(&config)
                .map_err(|_| pass_error)
                .with_context(|| format!("{}", path.display()))?;
            let sources = Sources::take(&mut config)?;
            let settings = Settings::from_poll_sources(owner, &config, args.job_limit)?;
            check_publications(settings.pace.is_some(), false)?;
            if !sources.is_empty() {
                sources.resolve(&mut config, &Discovery::new()?, &mut lines)?;
            }
            Settings::from_json(owner, &config, args.job_limit)?
        }
    };
    if args.verbose {
        for line in lines {
            eprintln!("{line}");
        }
    }
    Ok(settings)
}

/// The report, read with `client`'s token within the pass's bounds;
/// `board` is the project's snapshot, if one was asked for.
pub fn report(client: &Client, settings: &Settings, board: Option<Result<Snapshot>>) -> Value {
    let limits = Limits {
        max_requests: REQUEST_LIMIT,
        collection_timeout_seconds: TIMEOUT_SECONDS,
        ..Limits::default()
    };
    let clock = || supervise::now_millis(time::OffsetDateTime::now_utc());
    let bounded = Bounded::new(client, &limits, &clock);
    org_status::collect(&bounded, settings, time::OffsetDateTime::now_utc(), board)
}
