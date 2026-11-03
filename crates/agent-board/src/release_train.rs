//! `release-train`: one pass of the release train (docs/release-train.md).
use anyhow::{Context, Result, ensure};
use board_forge::release_train::{Api, Config, pass};
use clap::Args;
use std::io::Write;

struct VerifiedRegistry<A>(A);

/// External tools do the existing registry and Sigstore protocols, not a
/// second attestation implementation. Two commands, at most 65 seconds each;
/// timeout also kills a verifier that does not respond to termination.
fn checked_tool(tool: &str, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("timeout")
        .args(["--kill-after=5s", "60s", tool])
        .args(args)
        .output()
        .with_context(|| format!("run bounded {tool} (requires timeout, skopeo and gh)"))?;
    ensure!(
        output.status.success(),
        "{tool} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).with_context(|| format!("{tool} output is not UTF-8"))
}

impl<A: Api> Api for VerifiedRegistry<A> {
    fn request(
        &self,
        method: &str,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.0.request(method, path, body)
    }

    fn secure_host_digest(&self, version: &str, source: &str, workflow: &str) -> Result<String> {
        verified_digest(version, source, workflow, checked_tool)
    }
}

fn verified_digest(
    version: &str,
    source: &str,
    workflow: &str,
    mut tool: impl FnMut(&str, &[&str]) -> Result<String>,
) -> Result<String> {
    let image = "ghcr.io/cgwalters-forge/secure-host";
    let digest = tool(
        "skopeo",
        &[
            "inspect",
            "--format",
            "{{.Digest}}",
            &format!("docker://{image}:v{version}"),
        ],
    )?;
    let digest = digest.trim();
    ensure!(
        digest.strip_prefix("sha256:").is_some_and(|s| s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
        "registry returned an invalid sha256 digest"
    );
    // Verify the immutable digest, never a second lookup of the mutable
    // tag. Bind both producer identity and source to the reviewed release.
    tool(
        "gh",
        &[
            "attestation",
            "verify",
            &format!("oci://{image}@{digest}"),
            "--repo",
            "cgwalters-forge/agentic-job",
            "--cert-identity",
            &format!(
                "https://github.com/cgwalters-forge/agentic-job/.github/workflows/{workflow}@refs/heads/main"
            ),
            "--source-ref",
            "refs/heads/main",
            "--source-digest",
            source,
            "--deny-self-hosted-runners",
        ],
    )?;
    Ok(digest.into())
}

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
        &VerifiedRegistry(api()?),
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
    fn registry_verification_uses_the_resolved_digest_and_existing_release_policy() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let source = "b".repeat(40);
        let mut calls = vec![];
        let resolved = verified_digest("0.6.9", &source, "release.yml", |tool, args| {
            calls.push((
                tool.to_owned(),
                args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            ));
            Ok(if tool == "skopeo" {
                format!("{digest}\n")
            } else {
                String::new()
            })
        })
        .unwrap();
        assert_eq!(resolved, digest);
        assert_eq!(
            calls[0],
            (
                "skopeo".into(),
                vec![
                    "inspect".into(),
                    "--format".into(),
                    "{{.Digest}}".into(),
                    "docker://ghcr.io/cgwalters-forge/secure-host:v0.6.9".into()
                ]
            )
        );
        assert_eq!(calls[1], ("gh".into(), vec!["attestation".into(), "verify".into(), format!("oci://ghcr.io/cgwalters-forge/secure-host@{digest}"), "--repo".into(), "cgwalters-forge/agentic-job".into(), "--cert-identity".into(), "https://github.com/cgwalters-forge/agentic-job/.github/workflows/release.yml@refs/heads/main".into(), "--source-ref".into(), "refs/heads/main".into(), "--source-digest".into(), source, "--deny-self-hosted-runners".into()]));
        for failed in ["skopeo", "gh"] {
            assert!(
                verified_digest("0.6.9", "source", "release.yml", |tool, _| {
                    ensure!(tool != failed, "fixture failure");
                    Ok(digest.clone())
                })
                .is_err()
            );
        }
    }

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
