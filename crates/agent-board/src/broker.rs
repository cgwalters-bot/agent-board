//! `broker probe`: whether the inference broker answers, kept as one outage
//! issue (docs/cli.md).
use anyhow::{Context, Result};
use board_forge::broker::{Config, PROBE_TIMEOUT, probe, publish};
use board_forge::release_train::Api;
use clap::Args;
use std::io::Write;

#[derive(Args)]
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
