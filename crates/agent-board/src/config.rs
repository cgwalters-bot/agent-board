//! Resolve repository sources only in trusted configuration, never in state,
//! proposals or other agent-authored JSON read by the CLI.
use anyhow::{Context, Result, ensure};
use board_forge::github::Client;
use board_forge::repository_lists::{RESOLUTION_SECONDS, Sources};
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};

fn client() -> Result<Client> {
    Ok(Client::from_env()?.retry_within(Instant::now() + Duration::from_secs(RESOLUTION_SECONDS)))
}

pub fn expand(mut value: Value) -> Result<Value> {
    let sources = Sources::take(&mut value)?;
    if !sources.is_empty() {
        ensure!(
            value["board"]["forge"] != "forgejo",
            "organization repository entries require GitHub"
        );
        let mut lines = Vec::new();
        sources.resolve(&mut value, &client()?, &mut lines)?;
        for line in lines {
            eprintln!("{line}");
        }
    }
    Ok(value)
}

pub fn read(path: &Path) -> Result<Value> {
    expand(crate::supervise::read(path)?)
}

pub fn pass(text: &str) -> Result<board_forge::pass::Config> {
    let config = board_forge::pass::Config::parse_sources(text)?;
    if !config.has_repository_sources() {
        return Ok(config);
    }
    let mut lines = Vec::new();
    let resolved = config
        .resolve(&client()?, &mut lines)?
        .context("source configuration was not resolved")?;
    for line in lines {
        eprintln!("{line}");
    }
    Ok(resolved)
}
