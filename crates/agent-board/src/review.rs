//! `review send`: post the newest operator-approved review draft on a tracker
//! issue as the bot (docs/review-drafts.md).
use anyhow::{Context, Result, ensure};
use board_forge::github::Transport;
use board_forge::poll::Rest;
use board_forge::review::{Approval, Config, Ticked};
use clap::Args;
use std::io::Write;

#[derive(Args)]
pub struct Send {
    /// Draft issue URL, OWNER/REPO#N, or N with --repo.
    issue: String,
    /// Trusted agent-board.json from a protected default-branch checkout: operator, bot and review.
    #[arg(long)]
    config: std::path::PathBuf,
    /// Print the requests without writing (the default).
    #[arg(long, conflicts_with = "apply")]
    dry_run: bool,
    /// Send with the configured bot's token.
    #[arg(long)]
    apply: bool,
    /// The operator's approval is their ticking the body's checkbox, not a
    /// /send comment: the `issues: edited` event payload of that edit, such
    /// as $GITHUB_EVENT_PATH, which binds the send to the body it left.
    #[arg(long, value_name = "EVENT")]
    approved_by_edit: Option<std::path::PathBuf>,
}

/// Every input is checked before a token is read.
pub fn send<T: Transport + Rest>(
    args: &Send,
    repo: Option<&str>,
    host: &str,
    transport: impl FnOnce() -> Result<T>,
    out: &mut impl Write,
) -> Result<()> {
    ensure!(
        host == board_forge::github::DEFAULT_HOST,
        "review send supports only github.com"
    );
    let issue = board_forge::github::parse_github_issue(&args.issue, repo, host)?;
    let value =
        serde_json::from_str(&std::fs::read_to_string(&args.config).context("read review config")?)
            .context("decode review config")?;
    let config = Config::from_json(&value)?;
    let approval = match &args.approved_by_edit {
        Some(path) => {
            let event = serde_json::from_str(
                &std::fs::read_to_string(path).context("read the approving event")?,
            )
            .context("decode the approving event")?;
            Approval::Edit(Ticked::from_event(&event, &issue, &config)?)
        }
        None => Approval::Command,
    };
    for line in board_forge::review::send(&transport()?, &issue, &config, &approval, args.apply)? {
        serde_json::to_writer(&mut *out, &line)?;
        writeln!(out)?;
    }
    if !args.apply {
        eprintln!("Dry run; nothing was sent. Pass --apply to send.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_is_checked_before_the_token_is_read() {
        let dir = std::env::temp_dir().join(format!("review-send-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("agent-board.json");
        let event = dir.join("event.json");
        let args = |issue: &str| Send {
            issue: issue.into(),
            config: config.clone(),
            dry_run: true,
            apply: false,
            approved_by_edit: Some(event.clone()),
        };
        let valid = r#"{"operator": "operator", "bot": "example-bot", "review": {"targets": ["ostreedev/*"], "checkbox": "Send"}}"#;
        // A sender other than the operator; board-forge's tests cover the rest.
        let visitor = r#"{"action": "edited", "sender": {"login": "visitor"}}"#;
        // Configuration, event, issue, then the error.
        for (text, edit, issue, expected) in [
            ("", visitor, "900", "decode review config"),
            (
                r#"{"operator": "operator", "bot": "example-bot"}"#,
                visitor,
                "900",
                "review is not configured",
            ),
            (
                valid,
                visitor,
                "https://github.com/example/tracker/pull/900",
                "not pull requests",
            ),
            (valid, "", "900", "decode the approving event"),
            (valid, visitor, "900", "the event's edit is not operator's"),
        ] {
            std::fs::write(&config, text).unwrap();
            std::fs::write(&event, edit).unwrap();
            let error = send(
                &args(issue),
                Some("example/tracker"),
                "github.com",
                || -> Result<board_forge::github::Client> { panic!("token read") },
                &mut Vec::new(),
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
