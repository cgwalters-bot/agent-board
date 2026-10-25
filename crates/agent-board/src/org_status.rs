use anyhow::Result;
use board_core::Snapshot;
use board_forge::org_status::{self, REQUEST_LIMIT, Settings, TIMEOUT_SECONDS};
use board_forge::supervise::{self, Bounded, Limits};
use serde_json::Value;
use std::path::PathBuf;

#[derive(clap::Args)]
pub struct Status {
    /// Repository-owned agent-board.json from a trusted default-branch checkout: the dispatch repository and workflow whose runs are agent runs, the followed repositories whose queues are shown, and `pace`, where the usage snapshot is.
    #[arg(long, value_name = "FILE")]
    config: PathBuf,
    /// Concurrent hosted-job limit to compare running jobs against.
    #[arg(long, value_name = "N")]
    job_limit: Option<u64>,
}

pub fn settings(args: &Status, owner: &str) -> Result<Settings> {
    Settings::from_json(
        owner,
        &crate::supervise::read(&args.config)?,
        args.job_limit,
    )
}

/// The report, read with `client`'s token within the pass's bounds;
/// `board` is the project's snapshot, if one was asked for.
pub fn report(
    client: &board_forge::github::Client,
    settings: &Settings,
    board: Option<Result<Snapshot>>,
) -> Value {
    let limits = Limits {
        max_requests: REQUEST_LIMIT,
        collection_timeout_seconds: TIMEOUT_SECONDS,
        ..Limits::default()
    };
    let clock = || supervise::now_millis(time::OffsetDateTime::now_utc());
    let bounded = Bounded::new(client, &limits, &clock);
    org_status::collect(&bounded, settings, time::OffsetDateTime::now_utc(), board)
}
