//! `release-train`: one pass of the release train (docs/release-train.md).
use anyhow::{Context, Result};
use board_forge::release_train::{Api, Config, pass};
use clap::Args;
use std::io::Write;

#[derive(Args)]
pub struct ReleaseTrain {
    /// The train's configuration from a trusted default-branch checkout: the bot, the release repository, the wrappers and callers to pin, labels and thresholds.
    #[arg(long, value_name = "FILE")]
    config: std::path::PathBuf,
    /// Take the pass's step with GH_TOKEN, the bot's; without it the pass only reads and prints.
    #[arg(long)]
    apply: bool,
}

/// The configuration is checked before a token is read.
pub fn run<A: Api>(
    args: &ReleaseTrain,
    api: impl FnOnce() -> Result<A>,
    out: &mut impl Write,
) -> Result<()> {
    let path = args.config.display();
    let text = std::fs::read_to_string(&args.config).with_context(|| format!("read {path}"))?;
    let value = serde_json::from_str(&text).with_context(|| format!("decode {path}"))?;
    let config = Config::from_json(&value).with_context(|| format!("check {path}"))?;
    pass(
        &api()?,
        &config,
        args.apply,
        time::OffsetDateTime::now_utc(),
        out,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_is_checked_before_the_token_is_read() {
        let dir = std::env::temp_dir().join(format!("release-train-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("release-train.json");
        for (text, expected) in [
            ("", "decode"),
            (r#"{"bot": "example-bot"}"#, "release is not an object"),
        ] {
            std::fs::write(&config, text).unwrap();
            let args = ReleaseTrain {
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
