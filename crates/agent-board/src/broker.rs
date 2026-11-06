//! `broker probe`: whether the inference broker answers, kept as one outage
//! issue (docs/cli.md).
use anyhow::{Context, Result};
use board_forge::broker::{Config, PROBE_TIMEOUT, probe, publish};
use board_forge::release_train::Api;
use clap::Args;
use std::io::Write;

pub const PROBE_ABOUT: &str = "Probe the inference broker and keep one outage issue open while it is unreachable.

Run from a scheduled workflow that can reach the broker, for example every 15 minutes from the tailnet. GH_TOKEN (the bot's) is required even for preview: it reads the token's login and the open outage issues. Without --apply, each intended write is printed as 'would: ...'; with --apply, open or refresh an outage issue while unreachable, or close it and comment on recovery.

The probe tries TCP, then one unauthenticated HTTP GET, without redirects or an environment proxy. Each TCP connection attempt and the HTTP request have a 10-second timeout. Any HTTP response, including 401, 404, 500 or a redirect, counts as reachable. The first line is 'broker: reachable' or 'broker: unreachable'. An unreachable broker exits 1 after any writes; other errors also fail the command.

Read configuration from the caller's trusted default branch. All three JSON keys are required and unknown keys are refused: url is an HTTP(S) address without credentials; repository is OWNER/REPO for the outage issue; label is its label (created if missing), normally outage for poll's brokerOutage. Read the broker address from the deployment environment when preparing this file, rather than copying a production address into code.";

#[derive(Args)]
#[command(
    after_long_help = "Examples:\n  Preview using the caller's trusted configuration (still reads GitHub):\n    agent-board broker probe --config broker.json\n\n  Publish outage or recovery updates from the scheduled workflow:\n    agent-board broker probe --config broker.json --apply\n\n  Configuration shape (replace the example address for your deployment):\n    {\"url\":\"http://broker.example:8080/\",\"repository\":\"OWNER/REPO\",\"label\":\"outage\"}"
)]
pub struct Probe {
    /// The probe's configuration from a trusted default-branch checkout: the broker's url, and the outage issue's repository and label.
    #[arg(long, value_name = "FILE")]
    config: std::path::PathBuf,
    /// Open, refresh or close the outage issue with GH_TOKEN, the bot's; without it the probe reads and prints each write.
    #[arg(long)]
    apply: bool,
}

/// The configuration is checked before a token is read.
pub fn run<A: Api>(
    args: &Probe,
    api: impl FnOnce() -> Result<A>,
    out: &mut impl Write,
) -> Result<()> {
    let path = args.config.display();
    let text = std::fs::read_to_string(&args.config).with_context(|| format!("read {path}"))?;
    let value = serde_json::from_str(&text).with_context(|| format!("decode {path}"))?;
    let config = Config::from_json(&value).with_context(|| format!("check {path}"))?;
    let api = api()?;
    let up = probe(&config.url, PROBE_TIMEOUT);
    let now = time::OffsetDateTime::now_utc();
    publish(&api, &config, up, args.apply, now, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_is_checked_before_the_token_is_read() {
        let dir = std::env::temp_dir().join(format!("broker-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("broker.json");
        for (text, expected) in [
            ("", "decode"),
            (r#"{"url": "http://broker.example/"}"#, "needs repository"),
        ] {
            std::fs::write(&config, text).unwrap();
            let args = Probe {
                config: config.clone(),
                apply: false,
            };
            let error = run(
                &args,
                || -> Result<board_forge::github::Client> { panic!("token read") },
                &mut Vec::new(),
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
