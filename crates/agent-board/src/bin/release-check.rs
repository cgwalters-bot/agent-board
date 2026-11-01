//! Release version gate, adapted from cgwalters-forge/agentic-job's
//! crates/agentic-job/src/bin/release-check.rs. Reads tag names, never executes them.
//! Rejecting malformed or non-increasing v* tags prevents a misleading upgrade;
//! bounding the input refuses a truncated tag inventory rather than accepting it.

use anyhow::{Context, Result, bail, ensure};
use std::io::Read;

const MAX_INPUT_BYTES: u64 = 1 << 20;

type Version = (u64, u64, u64);

fn parse(s: &str) -> Option<Version> {
    let mut parts = s.split('.');
    let mut next = || {
        let p = parts.next()?;
        let valid = !p.is_empty()
            && p.bytes().all(|b| b.is_ascii_digit())
            && (p == "0" || !p.starts_with('0'));
        if valid { p.parse().ok() } else { None }
    };
    let v = (next()?, next()?, next()?);
    parts.next().is_none().then_some(v)
}

fn read_tags(input: impl Read) -> Result<String> {
    let mut tags = String::new();
    input
        .take(MAX_INPUT_BYTES + 1)
        .read_to_string(&mut tags)
        .context("reading tags")?;
    ensure!(
        tags.len() as u64 <= MAX_INPUT_BYTES,
        "more than {MAX_INPUT_BYTES} bytes of tags"
    );
    Ok(tags)
}

fn check(version: &str, workspace: &str, tags: &str) -> Result<Option<String>> {
    let Some(wanted) = parse(version) else {
        bail!("{version:?} is not MAJOR.MINOR.PATCH");
    };
    ensure!(
        version == workspace,
        "{version} is not the workspace's version {workspace}: raise it in Cargo.toml first"
    );
    let mut newest = None;
    for (tag, number) in tags.lines().filter_map(|t| Some((t, t.strip_prefix('v')?))) {
        let Some(v) = parse(number) else {
            bail!("tag {tag:?} is not vMAJOR.MINOR.PATCH");
        };
        newest = newest.max(Some((v, tag)));
    }
    match newest {
        Some((v, tag)) if v >= wanted => bail!("{version} is not newer than {tag}"),
        newest => Ok(newest.map(|(_, tag)| tag.to_owned())),
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [version] = args.as_slice() else {
        bail!("usage: git tag --list 'v*' | release-check VERSION");
    };
    let tags = read_tags(std::io::stdin())?;
    println!(
        "{}",
        check(version, env!("CARGO_PKG_VERSION"), &tags)?.unwrap_or_default()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_tags() {
        for (version, workspace, tags, expected) in [
            ("0.1.0", "0.1.0", "", Ok(None)),
            (
                "0.1.0",
                "0.1.0",
                "build-abc\nv0.0.9\nv0.0.10\n",
                Ok(Some("v0.0.10")),
            ),
            ("0.0.9", "0.0.9", "v0.0.10\n", Err("not newer")),
            ("0.1.0", "0.1.0", "v0.1.0\n", Err("not newer")),
            ("0.1.0", "0.1.0", "v1.0\n", Err("not vMAJOR")),
            ("0.1.0", "0.1.0", "v0.0.1-rc.1\n", Err("not vMAJOR")),
            ("0.1.0", "0.2.0", "", Err("not the workspace")),
            ("0.1.0\n", "0.1.0", "", Err("not MAJOR")),
        ] {
            match (check(version, workspace, tags), expected) {
                (Ok(got), Ok(want)) => assert_eq!(got.as_deref(), want),
                (Err(got), Err(want)) => assert!(got.to_string().contains(want), "{got}"),
                (got, want) => panic!("{version}: {got:?}, expected {want:?}"),
            }
        }
    }

    #[test]
    fn plain_versions_only() {
        for s in [
            "v1.0.0",
            "1.0",
            "1.0.0.0",
            "01.0.0",
            "1.0.0-rc.1",
            "1.0.0+build",
            "1..0",
            " 1.0.0",
            "+1.0.0",
            "1.0.99999999999999999999",
        ] {
            assert_eq!(parse(s), None, "{s}");
        }
        assert_eq!(parse("1.10.0"), Some((1, 10, 0)));
    }

    #[test]
    fn bounded_tags() {
        assert_eq!(read_tags("v1.0.0\n".as_bytes()).unwrap(), "v1.0.0\n");
        assert!(read_tags(vec![b'v'; MAX_INPUT_BYTES as usize].as_slice()).is_ok());
        assert!(read_tags(vec![b'v'; MAX_INPUT_BYTES as usize + 1].as_slice()).is_err());
    }
}
