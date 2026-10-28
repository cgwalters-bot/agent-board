//! `--fix-bounds`: the caller's agentic-job bounds for its `fix` kind
//! (tracker's `.github/agentic-job/fix.toml`), read so that the poll starts
//! no fix-kind run that agentic-job's policy step would refuse for its
//! repository or head branch (agent-board#264). The policy step stays the
//! boundary; this only saves the run it would refuse.
//!
//! Only the top-level `repos` and `outputs.push_to_pull_request_branch.branches`
//! are read; every other key is agentic-job's. A glob is evaluated only in
//! the forms below, and any other is refused rather than guessed at.
use anyhow::{Context, Result, ensure};
use std::path::Path;

/// One list the poll reads, and the glob forms it evaluates.
struct Field {
    /// Its path in the bounds file.
    what: &'static str,
    /// The one wildcard suffix it takes, after a name with no glob in it.
    wildcard: &'static str,
    /// Whether the wildcard spans `/`, as `**` does.
    deep: bool,
    /// Whether names compare case-insensitively, as GitHub's owner and
    /// repository names do ([`super::comments::in_scope`]).
    fold: bool,
}

/// `OWNER/REPO`, or `OWNER/*`: any repository of the owner.
const REPOS: Field = Field {
    what: "repos",
    wildcard: "/*",
    deep: false,
    fold: true,
};

/// An exact branch, or `PREFIX/**`: any branch that starts with `PREFIX/`.
const BRANCHES: Field = Field {
    what: "outputs.push_to_pull_request_branch.branches",
    wildcard: "/**",
    deep: true,
    fold: false,
};

/// Characters that make a glob anywhere but in a field's wildcard.
const GLOB_CHARS: &[char] = &['*', '?', '[', '{'];

/// The repositories and head branches a `fix` run may push to.
#[derive(Clone, Debug, PartialEq)]
pub struct FixBounds {
    repos: Vec<String>,
    branches: Vec<String>,
}

impl FixBounds {
    /// The bounds file at `path`, from the caller's trusted checkout.
    pub fn read(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("fix bounds {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let value: toml::Value = toml::from_str(text).context("decode TOML")?;
        let branches = value
            .get("outputs")
            .and_then(|o| o.get("push_to_pull_request_branch"))
            .and_then(|p| p.get("branches"));
        Ok(Self {
            repos: patterns(&REPOS, value.get("repos"))?,
            branches: patterns(&BRANCHES, branches)?,
        })
    }

    /// Whether a fix run may push to `repo`.
    pub fn admits_repo(&self, repo: &str) -> bool {
        admits(&REPOS, &self.repos, repo)
    }

    /// Why a fix run on `repo`'s pull request from `branch` is outside the
    /// bounds; none where it is inside.
    pub fn refusal(&self, repo: &str, branch: &str) -> Option<String> {
        if !admits(&REPOS, &self.repos, repo) {
            Some(format!("repository {repo} is outside the fix bounds"))
        } else if !admits(&BRANCHES, &self.branches, branch) {
            Some(format!("head branch {branch:?} is outside the fix bounds"))
        } else {
            None
        }
    }
}

/// `field`'s list of patterns, each in a form [`matches`] evaluates exactly.
fn patterns(field: &Field, value: Option<&toml::Value>) -> Result<Vec<String>> {
    let what = field.what;
    let list = value
        .with_context(|| format!("{what} is missing"))?
        .as_array()
        .with_context(|| format!("{what} is not an array of strings"))?;
    list.iter()
        .map(|v| {
            let pattern = v
                .as_str()
                .with_context(|| format!("{what} is not an array of strings"))?;
            let stem = pattern.strip_suffix(field.wildcard).unwrap_or(pattern);
            ensure!(
                !stem.is_empty() && !stem.contains(GLOB_CHARS),
                "{what}: {pattern:?} is not an exact name or NAME{}, the only glob read",
                field.wildcard
            );
            Ok(pattern.to_owned())
        })
        .collect()
}

fn admits(field: &Field, patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| matches(field, p, name))
}

/// Whether `name` is `pattern`, or under its stem where it ends in the
/// field's wildcard.
fn matches(field: &Field, pattern: &str, name: &str) -> bool {
    let eq = |a: &str, b: &str| match field.fold {
        true => a.eq_ignore_ascii_case(b),
        false => a == b,
    };
    let Some(stem) = pattern.strip_suffix(field.wildcard) else {
        return eq(pattern, name);
    };
    let under = name
        .get(..stem.len())
        .filter(|head| eq(head, stem))
        .and_then(|_| name[stem.len()..].strip_prefix('/'));
    under.is_some_and(|rest| !rest.is_empty() && (field.deep || !rest.contains('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACKER: &str = r#"
hosts = ["github.com"]
repos = ["cgwalters-forge/tracker", "bootc-dev/*"]
bases = ["main"]

[outputs]
push_to_pull_request_branch = { max = 1, branches = ["dispatch/**", "bot/**", "main"], replacement_max_commits = 5 }
add_comment = { max = 1 }
"#;

    #[test]
    fn refusal_table() {
        let bounds = FixBounds::parse(TRACKER).unwrap();
        for (repo, branch, want) in [
            ("cgwalters-forge/tracker", "dispatch/x", None),
            ("CGWalters-Forge/Tracker", "dispatch/a/b", None),
            ("bootc-dev/bootc", "bot/x", None),
            ("Bootc-Dev/bootc", "main", None),
            (
                "bootc-dev/bootc",
                "ci-drop-grub-cc",
                Some(r#"head branch "ci-drop-grub-cc" is outside the fix bounds"#),
            ),
            (
                "bootc-dev/bootc",
                "Dispatch/x",
                Some(r#"head branch "Dispatch/x" is outside the fix bounds"#),
            ),
            (
                "bootc-dev/bootc",
                "dispatch",
                Some(r#"head branch "dispatch" is outside the fix bounds"#),
            ),
            (
                "bootc-dev/bootc",
                "dispatchx/y",
                Some(r#"head branch "dispatchx/y" is outside the fix bounds"#),
            ),
            (
                "cgwalters-forge/bootc",
                "dispatch/x",
                Some("repository cgwalters-forge/bootc is outside the fix bounds"),
            ),
            (
                "bootc-devx/bootc",
                "dispatch/x",
                Some("repository bootc-devx/bootc is outside the fix bounds"),
            ),
        ] {
            assert_eq!(
                bounds.refusal(repo, branch).as_deref(),
                want,
                "{repo} {branch}"
            );
        }
    }

    #[test]
    fn parse_failures() {
        let branches = |list: &str| {
            format!(
                "repos = [\"a/b\"]\n[outputs]\npush_to_pull_request_branch = {{ branches = {list} }}\n"
            )
        };
        for (name, text, want) in [
            (
                "no repos",
                "[outputs]\npush_to_pull_request_branch = { branches = [] }".to_owned(),
                "repos is missing",
            ),
            (
                "no branches",
                "repos = [\"a/b\"]".to_owned(),
                "outputs.push_to_pull_request_branch.branches is missing",
            ),
            (
                "repos not an array",
                "repos = \"a/b\"".to_owned(),
                "repos is not an array of strings",
            ),
            (
                "branches of numbers",
                branches("[1]"),
                "outputs.push_to_pull_request_branch.branches is not an array of strings",
            ),
            (
                "a single-segment branch glob",
                branches("[\"dispatch/*\"]"),
                "outputs.push_to_pull_request_branch.branches: \"dispatch/*\" is not an exact name or NAME/**, the only glob read",
            ),
            (
                "a glob mid-name",
                branches("[\"dis*/**\"]"),
                "outputs.push_to_pull_request_branch.branches: \"dis*/**\" is not an exact name or NAME/**, the only glob read",
            ),
            (
                "a bare wildcard",
                branches("[\"/**\"]"),
                "outputs.push_to_pull_request_branch.branches: \"/**\" is not an exact name or NAME/**, the only glob read",
            ),
            (
                "a deep repository glob",
                "repos = [\"bootc-dev/**\"]".to_owned(),
                "repos: \"bootc-dev/**\" is not an exact name or NAME/*, the only glob read",
            ),
            (
                "a character class",
                "repos = [\"bootc-dev/b[o]otc\"]".to_owned(),
                "repos: \"bootc-dev/b[o]otc\" is not an exact name or NAME/*, the only glob read",
            ),
            (
                "an alternation",
                "repos = [\"{a,b}/c\"]".to_owned(),
                "repos: \"{a,b}/c\" is not an exact name or NAME/*, the only glob read",
            ),
            ("not TOML", "repos = [".to_owned(), "decode TOML"),
        ] {
            let error = FixBounds::parse(&text).unwrap_err();
            assert_eq!(format!("{error}"), want, "{name}");
        }
    }
}
