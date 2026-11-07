//! `--fix-bounds`: the caller's agentic-job bounds for its `fix` kind
//! (tracker's `.github/agentic-job/fix.toml`), read so that the poll starts
//! no fix-kind run that agentic-job's policy step would refuse for its
//! repository or head branch (agent-board#264). The policy step stays the
//! boundary; this only saves the run it would refuse. The flag repeats, one
//! file per bounds the caller dispatches with, read as a [`FixBoundsSet`].
//!
//! Only the top-level `repos` and `outputs.push_to_pull_request_branch`'s
//! `branches` and `own_author`, under `[github]` since agentic-job v0.6.5,
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

/// The login whose own pull requests a fix may push to in a repository
/// `repos` does not list (agentic-job#3164), as agentic-job's policy step
/// admits them; `branches` still applies.
const OWN_AUTHOR: &str = "outputs.push_to_pull_request_branch.own_author";

/// Characters that make a glob anywhere but in a field's wildcard.
const GLOB_CHARS: &[char] = &['*', '?', '[', '{'];

/// The repositories and head branches a `fix` run may push to.
#[derive(Clone, Debug, PartialEq)]
pub struct FixBounds {
    repos: Vec<String>,
    branches: Vec<String>,
    own_author: Option<String>,
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
        // agentic-job v0.6.5 moved `outputs` under the forge's section.
        let push = value
            .get("github")
            .and_then(|g| g.get("outputs"))
            .or_else(|| value.get("outputs"))
            .and_then(|o| o.get("push_to_pull_request_branch"));
        let own_author = match push.and_then(|p| p.get("own_author")) {
            None => None,
            Some(login) => {
                let login = login
                    .as_str()
                    .filter(|l| !l.is_empty())
                    .with_context(|| format!("{OWN_AUTHOR} is not a login"))?;
                Some(login.to_owned())
            }
        };
        Ok(Self {
            repos: patterns(&REPOS, value.get("repos"))?,
            branches: patterns(&BRANCHES, push.and_then(|p| p.get("branches")))?,
            own_author,
        })
    }

    /// Whether a fix run may push to `repo`.
    pub fn admits_repo(&self, repo: &str) -> bool {
        admits(&REPOS, &self.repos, repo)
    }

    /// Whether a fix run may push to `repo`'s pull request by `author` from
    /// `branch`: in a repository `repos` lists, or else on [`OWN_AUTHOR`]'s
    /// own pull request.
    fn admits_pull(&self, repo: &str, author: &str, branch: &str) -> bool {
        (self.admits_repo(repo) || self.own(author)) && admits(&BRANCHES, &self.branches, branch)
    }

    /// Whether `author` is this file's [`OWN_AUTHOR`], as agentic-job's
    /// policy step compares logins: ignoring case.
    fn own(&self, author: &str) -> bool {
        (self.own_author.as_deref()).is_some_and(|own| own.eq_ignore_ascii_case(author))
    }
}

/// Every `--fix-bounds` file's bounds, each kept whole: a pull request is
/// inside where any one file admits both its repository and its head branch,
/// never a repository from one file paired with a branch from another, as
/// the caller's dispatch picks one file per repository. Empty (the default)
/// checks nothing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FixBoundsSet {
    files: Vec<FixBounds>,
}

impl FixBoundsSet {
    pub fn new(files: Vec<FixBounds>) -> Self {
        Self { files }
    }

    /// The bounds files at `paths`, from the caller's trusted checkout.
    pub fn read(paths: &[impl AsRef<Path>]) -> Result<Self> {
        let files = paths
            .iter()
            .map(|p| FixBounds::read(p.as_ref()))
            .collect::<Result<_>>()?;
        Ok(Self { files })
    }

    /// Whether any file lets a fix run push to `repo`.
    pub fn admits_repo(&self, repo: &str) -> bool {
        self.files.iter().any(|f| f.admits_repo(repo))
    }

    /// Why a fix run on `repo`'s pull request by `author` from `branch` is
    /// outside every file's bounds: its repository where no file admits it
    /// (by `repos`, or by `own_author` for the author's own pull request),
    /// else its head branch; none where one file admits both, or there are
    /// no files.
    pub fn refusal(&self, repo: &str, author: &str, branch: &str) -> Option<String> {
        let files = &self.files;
        if files.is_empty() || files.iter().any(|f| f.admits_pull(repo, author, branch)) {
            None
        } else if self.admits_repo(repo) || files.iter().any(|f| f.own(author)) {
            Some(format!("head branch {branch:?} is outside the fix bounds"))
        } else {
            Some(format!("repository {repo} is outside the fix bounds"))
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

    const BOT: &str = "cgwalters-bot";

    const TRACKER: &str = r#"
hosts = ["github.com"]
repos = ["cgwalters-forge/tracker", "bootc-dev/*"]
bases = ["main"]

[outputs]
push_to_pull_request_branch = { max = 1, branches = ["dispatch/**", "bot/**", "main"], replacement_max_commits = 5 }
add_comment = { max = 1 }
"#;

    /// tracker's bounds for its midstream forks, beside [`TRACKER`]'s.
    const MIDSTREAM: &str = r#"
repos = ["cgwalters-forge/bootc", "cgwalters-forge/composefs-rs"]

[outputs]
push_to_pull_request_branch = { max = 1, branches = ["bot/**"] }
"#;

    fn set(files: &[&str]) -> FixBoundsSet {
        FixBoundsSet::new(files.iter().map(|f| FixBounds::parse(f).unwrap()).collect())
    }

    #[test]
    fn refusal_table() {
        let bounds = set(&[TRACKER]);
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
                bounds.refusal(repo, BOT, branch).as_deref(),
                want,
                "{repo} {branch}"
            );
        }
    }

    #[test]
    fn repeated_refusal_table() {
        let bounds = set(&[TRACKER, MIDSTREAM]);
        for (repo, branch, want) in [
            ("cgwalters-forge/tracker", "dispatch/x", None),
            // The midstream fork's bot branch, admitted by the second file.
            ("cgwalters-forge/bootc", "bot/x", None),
            ("CGWalters-Forge/composefs-rs", "bot/a/b", None),
            (
                "cgwalters-forge/tracker",
                "pass-follow-midstream",
                Some(r#"head branch "pass-follow-midstream" is outside the fix bounds"#),
            ),
            // The repository only the second file admits, with a branch only
            // the first does: no one file admits the pair.
            (
                "cgwalters-forge/bootc",
                "dispatch/x",
                Some(r#"head branch "dispatch/x" is outside the fix bounds"#),
            ),
            (
                "cgwalters-forge/agent-board",
                "bot/x",
                Some("repository cgwalters-forge/agent-board is outside the fix bounds"),
            ),
        ] {
            assert_eq!(
                bounds.refusal(repo, BOT, branch).as_deref(),
                want,
                "{repo} {branch}"
            );
        }
        for (repo, want) in [
            ("cgwalters-forge/tracker", true),
            ("cgwalters-forge/bootc", true),
            ("cgwalters-forge/agent-board", false),
        ] {
            assert_eq!(bounds.admits_repo(repo), want, "{repo}");
            assert!(!set(&[]).admits_repo(repo), "{repo}");
        }
        assert_eq!(set(&[]).refusal("a/b", BOT, "c"), None);
    }

    /// [`TRACKER`] with agentic-job#3164's `own_author`: the bot's own pull
    /// request is inside in a repository `repos` does not list, still only
    /// from a branch `branches` admits; anyone else's is not.
    #[test]
    fn own_author_refusal_table() {
        let own = TRACKER.replace(
            "replacement_max_commits = 5",
            &format!("replacement_max_commits = 5, own_author = \"{BOT}\""),
        );
        const OUTSIDE: &str = "repository bootc-devx/bootc is outside the fix bounds";
        let branch = r#"head branch "ci-x" is outside the fix bounds"#;
        // The bounds files, the repository, the author, the branch, and why
        // it is refused.
        for (files, repo, author, head, want) in [
            (&[own.as_str()][..], "bootc-devx/bootc", BOT, "bot/x", None),
            (
                &[own.as_str()],
                "bootc-devx/bootc",
                "CGWalters-Bot",
                "bot/x",
                None,
            ),
            (
                &[own.as_str()],
                "bootc-devx/bootc",
                BOT,
                "ci-x",
                Some(branch),
            ),
            (
                &[own.as_str()],
                "bootc-devx/bootc",
                "someone",
                "bot/x",
                Some(OUTSIDE),
            ),
            (
                &[own.as_str()],
                "bootc-devx/bootc",
                "",
                "bot/x",
                Some(OUTSIDE),
            ),
            // A listed repository admits anyone's, as without the key.
            (&[own.as_str()], "bootc-dev/bootc", "someone", "bot/x", None),
            // Without the key, the bot's own is outside as before.
            (&[TRACKER], "bootc-devx/bootc", BOT, "bot/x", Some(OUTSIDE)),
            // The file naming the author must admit the branch too, not
            // another file.
            (
                &[MIDSTREAM, own.as_str()],
                "bootc-devx/bootc",
                BOT,
                "main",
                None,
            ),
            (
                &[own.as_str(), MIDSTREAM],
                "bootc-devx/bootc",
                BOT,
                "x/y",
                Some(r#"head branch "x/y" is outside the fix bounds"#),
            ),
        ] {
            assert_eq!(
                set(files).refusal(repo, author, head).as_deref(),
                want,
                "{files:?} {repo} {author} {head}"
            );
        }
    }

    /// `[outputs]` and agentic-job v0.6.5's `[github.outputs]` read the same.
    #[test]
    fn outputs_at_the_top_or_under_github() {
        let body = format!(
            "[{{}}.push_to_pull_request_branch]\nbranches = [\"dispatch/**\"]\nown_author = \"{BOT}\"\n"
        );
        let [old, new] = ["outputs", "github.outputs"].map(|table| {
            let text = format!("repos = [\"o/r\"]\n{}", body.replace("{}", table));
            FixBounds::parse(&text).unwrap()
        });
        assert_eq!(old, new);
        assert!(new.admits_pull("o/r", "someone", "dispatch/x"));
        assert!(!new.admits_pull("o/r", "someone", "main"));
        assert!(new.admits_pull("p/q", BOT, "dispatch/x"));
        assert!(!new.admits_pull("p/q", "someone", "dispatch/x"));
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
            (
                "an own_author of no login",
                "repos = [\"a/b\"]\n[outputs.push_to_pull_request_branch]\nbranches = [\"x\"]\nown_author = \"\"".to_owned(),
                "outputs.push_to_pull_request_branch.own_author is not a login",
            ),
            (
                "an own_author list",
                "repos = [\"a/b\"]\n[outputs.push_to_pull_request_branch]\nbranches = [\"x\"]\nown_author = [\"bot\"]".to_owned(),
                "outputs.push_to_pull_request_branch.own_author is not a login",
            ),
            ("not TOML", "repos = [".to_owned(), "decode TOML"),
        ] {
            let error = FixBounds::parse(&text).unwrap_err();
            assert_eq!(format!("{error}"), want, "{name}");
        }
    }
}
