use anyhow::{Context, Result, ensure};
use board_forge::forgejo::{Board, Client, intake};
use clap::Args;
use std::{io::Write, path::PathBuf};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Args)]
pub struct Intake {
    /// Override stored cursors with this RFC3339 start time.
    #[arg(long)]
    since: Option<String>,
    /// JSON map of repository to since cursor; saved after complete reads only.
    #[arg(long, value_name = "FILE")]
    state: Option<PathBuf>,
}

pub fn run(args: &Intake, cli: &crate::Cli, out: &mut impl Write) -> Result<()> {
    ensure!(
        cli.owner.is_none() && cli.project.is_none() && cli.jq.is_none(),
        "intake uses --board-config; --owner, --project and --jq are unsupported"
    );
    let path = cli
        .board_config
        .as_ref()
        .context("intake requires --board-config FILE")?;
    let value = crate::supervise::read(path)?;
    let Board::Forgejo(board) = Board::from_config(&value)? else {
        anyhow::bail!("intake requires a Forgejo board");
    };
    ensure!(board.intake.is_some(), "board.intake is not configured");
    let footer = board_forge::Footer::from_json(value.get("generatedBy"))?;
    let mut state: intake::State = match &args.state {
        Some(path) => match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("decode intake state")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(error) => return Err(error).context("read intake state"),
        },
        None => Default::default(),
    };
    let original = state.clone();
    let clock = OffsetDateTime::now_utc()
        .replace_nanosecond(0)?
        .format(&Rfc3339)?;
    let reports = intake::read(
        &board,
        &Client::from_env(&board)?,
        &mut state,
        args.since.as_deref(),
        &clock,
        &footer,
    )?;
    for report in reports {
        if let Some(error) = report.error {
            if cli.json.is_some() {
                writeln!(
                    out,
                    "{}",
                    serde_json::json!({"repository": report.repository, "error": error})
                )?;
            } else {
                writeln!(
                    out,
                    "{} error: {}",
                    report.repository,
                    error.replace(['\n', '\r'], " ")
                )?;
            }
        }
        for item in report.items {
            if cli.json.is_some() {
                serde_json::to_writer(&mut *out, &item)?;
                writeln!(out)?;
            } else {
                writeln!(
                    out,
                    "{}#{} {} {} author={}({}) operator={} generated-by={}",
                    item.repository,
                    item.number,
                    item.kind,
                    item.url,
                    item.author_login,
                    item.author_id,
                    item.is_operator,
                    item.generated_by
                )?;
            }
        }
    }
    out.flush()?;
    if state != original
        && let Some(path) = &args.state
    {
        save_state(path, &state)?;
    }
    Ok(())
}

fn save_state(path: &std::path::Path, state: &intake::State) -> Result<()> {
    // Atomic replacement avoids losing old cursors if interrupted while saving.
    // A stale temporary file after interruption must not block later saves.
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|error| anyhow::anyhow!("intake state nonce: {error}"))?;
    let temporary = path.with_extension(format!("intake-{:032x}.tmp", u128::from_le_bytes(nonce)));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .context("create intake state temporary file")?;
    let result = serde_json::to_writer_pretty(&mut file, state)
        .context("encode intake state")
        .and_then(|()| std::fs::rename(&temporary, path).context("save intake state"));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_save_ignores_stale_temporary_files() {
        let directory =
            std::env::temp_dir().join(format!("agent-board-intake-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("state.json");
        let stale = path.with_extension("intake.tmp");
        std::fs::write(&stale, "interrupted").unwrap();
        for clock in ["2026-10-30T01:00:00Z", "2026-10-30T02:00:00Z"] {
            let state = intake::State::from([("owner/work".into(), clock.into())]);
            save_state(&path, &state).unwrap();
            assert_eq!(
                serde_json::from_slice::<intake::State>(&std::fs::read(&path).unwrap()).unwrap(),
                state
            );
            assert_eq!(std::fs::read_to_string(&stale).unwrap(), "interrupted");
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
