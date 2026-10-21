//! Stacked implement runs (cgwalters-forge/agent-board#133): slices of an
//! epic that change the same files build on each other's pull requests
//! instead of conflicting while they wait. A job plan's `stacks_on` names the
//! lower slice, an issue in the plan's repository; dispatch starts the run
//! from the head branch of the bot's open pull request that closes it, and
//! the poll retargets the upper pull request once the lower one merges
//! ([`crate::poll`]), and never merges one whose base is not the default branch.
//!
//! Off unless a plan names `stacks_on`. The base is bounded to a `dispatch/`
//! branch of the target repository itself that the bot opened a pull request
//! from: a visitor's pull request that says it closes the lower issue must not
//! become the code a run starts from, nor may a plan point a run at an
//! arbitrary branch such as a release branch. A pull request's base can only
//! be a branch of its own repository, so one from the bot's fork, which is
//! where dispatch pull requests come from today, is not stacked on: its
//! slice's run waits until the caller opens it from the repository. The
//! caller's bounds check the base again. See docs/job-plans.md#stacking.
use crate::github::Transport;
use crate::poll::validate_repository;
use anyhow::{Context, Result, bail, ensure};
use board_core::ItemRef;
use serde_json::{Value, json};

/// The branches a run's pull request comes from, and so the only bases a
/// stacked run may have.
pub const STACK_BRANCH_PREFIX: &str = "dispatch/";
/// Open pull requests read, newest first, to find the lower slice's: a lower
/// one older than these is not found, and the run waits.
pub const STACK_PULL_READS: u64 = 50;
/// Characters of a base branch name.
const BRANCH_LIMIT: usize = 200;
/// The words by which a pull request's description closes an issue on merge.
const CLOSING: &[&str] = &[
    "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
];

const LOWER_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!, $pulls: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) { state }
    pullRequests(states: OPEN, first: $pulls, orderBy: {field: CREATED_AT, direction: DESC}) {
      nodes { number body headRefName isCrossRepository author { login } }
    }
  }
}"#;

/// The lower slice's open pull request, which a stacked run starts from.
#[derive(Clone, Debug, PartialEq)]
pub struct Base {
    pub pull: u64,
    pub url: String,
    /// Its head branch: the run's base, and its pull request's.
    pub branch: String,
}

/// What a stacked plan is dispatched on.
#[derive(Clone, Debug, PartialEq)]
pub enum Stack {
    /// The lower slice's open pull request.
    On(Base),
    /// The lower slice is closed: its work merged or was dropped, so the run
    /// starts from the default branch like any other.
    Unstacked(String),
    /// Nothing to start from yet: dispatch waits, and the next pass decides again.
    Wait(String),
}

/// A branch a stacked run may start from and its pull request target. It
/// goes into a workflow input, so it is kept to plain ref characters.
pub fn validate_branch(branch: &str) -> Result<()> {
    let name = branch.strip_prefix(STACK_BRANCH_PREFIX).unwrap_or_default();
    ensure!(
        !name.is_empty()
            && branch.len() <= BRANCH_LIMIT
            && !branch.contains("..")
            && !branch.contains("//")
            && !branch.ends_with(['/', '.'])
            && !branch.ends_with(".lock")
            && branch
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)),
        "{branch:?} is not a {STACK_BRANCH_PREFIX} branch a run may stack on"
    );
    Ok(())
}

/// Whether a pull request's description closes `number` in `repo`, as
/// GitHub reads it: anywhere in the text, a closing keyword (any case,
/// optionally followed by a colon) then `#N`, `OWNER/REPO#N` or
/// `https://github.com/OWNER/REPO/issues/N`. Each keyword closes the one
/// reference after it, so `Closes #41, closes #42` closes both and
/// `Closes #41, #42` only #41, as on GitHub.
fn closes(body: &str, repo: &str, number: u64) -> bool {
    let references = [
        format!("#{number}"),
        format!("{repo}#{number}"),
        format!("https://github.com/{repo}/issues/{number}"),
    ];
    let mut words = body.split_whitespace().peekable();
    while let Some(word) = words.next() {
        // A list marker, quote or bracket before the keyword: `- Closes`, `(fixes`.
        let word = word.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
        // `Closes: #41`, `Closes:#41` and `Closes #41` alike.
        let (keyword, attached) = word.split_once(':').unwrap_or((word, ""));
        if !CLOSING.contains(&keyword.to_ascii_lowercase().as_str()) {
            continue;
        }
        let reference = match attached {
            "" => words.peek().copied().unwrap_or_default(),
            attached => attached,
        };
        let reference = reference.trim_end_matches([',', '.', ';', ':', ')', ']', '!', '?']);
        if references.iter().any(|r| r.eq_ignore_ascii_case(reference)) {
            return true;
        }
    }
    false
}

/// Where the run of a plan on `dest` that stacks on `lower` (an issue in
/// `dest`'s repository) starts: the head branch of the one open pull request
/// of `bot` among the newest [`STACK_PULL_READS`] that closes `lower`.
/// Unlike the overlap check, a read that fails is an error: dispatching a
/// stacked run on the default branch would bring back the conflicts it avoids.
pub fn resolve<T: Transport>(client: &T, dest: &ItemRef, lower: u64, bot: &str) -> Result<Stack> {
    validate_repository(&dest.repository)?;
    let (owner, name) = dest
        .repository
        .split_once('/')
        .context("issue repository")?;
    let issue = ItemRef {
        number: lower,
        ..dest.clone()
    };
    let data = client.graphql(
        LOWER_QUERY,
        json!({"owner": owner, "name": name, "number": lower, "pulls": STACK_PULL_READS}),
    )?;
    let state = data
        .pointer("/repository/issue/state")
        .and_then(Value::as_str)
        .with_context(|| format!("{} is not an issue", issue.display()))?;
    if state != "OPEN" {
        return Ok(Stack::Unstacked(format!(
            "{} is closed, so it runs on the default branch",
            issue.display()
        )));
    }
    let nodes = data
        .pointer("/repository/pullRequests/nodes")
        .and_then(Value::as_array)
        .context("pull requests have no nodes")?;
    let mut found = Vec::new();
    for pull in nodes {
        let author = pull.pointer("/author/login").and_then(Value::as_str);
        let branch = pull["headRefName"].as_str().unwrap_or_default();
        if author.is_some_and(|a| a.eq_ignore_ascii_case(bot))
            && pull["isCrossRepository"] == false
            && branch.starts_with(STACK_BRANCH_PREFIX)
            && closes(
                pull["body"].as_str().unwrap_or_default(),
                &dest.repository,
                lower,
            )
        {
            let number = pull["number"]
                .as_u64()
                .filter(|n| *n > 0)
                .context("pull request has no number")?;
            validate_branch(branch)?;
            found.push(Base {
                pull: number,
                url: format!("https://github.com/{}/pull/{number}", dest.repository),
                branch: branch.into(),
            });
        }
    }
    match found.as_slice() {
        [] => Ok(Stack::Wait(format!(
            "it stacks on {}, which has no open pull request of {bot} from a {STACK_BRANCH_PREFIX} branch of {} among the newest {STACK_PULL_READS}",
            issue.display(),
            dest.repository
        ))),
        [base] => Ok(Stack::On(base.clone())),
        _ => bail!(
            "{} open pull requests of {bot} close {}; close all but one to stack on it",
            found.len(),
            issue.display()
        ),
    }
}

/// The task of a stacked run: the plan's brief, then where it starts and
/// what its pull request says, so a reader of the upper one sees the series.
pub fn brief(brief: &str, dest: &ItemRef, lower: u64, base: &Base) -> String {
    format!(
        "{brief}\n\nThis work stacks on {}#{} ({}), the pull request for #{lower}: the run starts from its branch `{}`, and its pull request targets that branch, not the default branch. Build on its commits without changing them. Begin the pull request description with \"Stacks on #{}.\"",
        dest.repository, base.pull, base.url, base.branch, base.pull
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Forge(Value);
    impl Transport for Forge {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            assert_eq!(query, LOWER_QUERY);
            assert_eq!(variables["number"], 41);
            assert_eq!(variables["pulls"], STACK_PULL_READS);
            Ok(self.0.clone())
        }
    }

    /// A pull request from a branch of the repository itself.
    fn pull(number: u64, author: &str, branch: &str, body: &str) -> Value {
        json!({"number": number, "author": {"login": author}, "headRefName": branch,
            "body": body, "isCrossRepository": false})
    }

    #[test]
    fn resolve_finds_only_the_bots_dispatch_pull_request() {
        let dest = ItemRef {
            host: "github.com".into(),
            repository: "example/widgets".into(),
            number: 42,
        };
        let base = |pull: u64, branch: &str| {
            Ok(Stack::On(Base {
                pull,
                url: format!("https://github.com/example/widgets/pull/{pull}"),
                branch: branch.into(),
            }))
        };
        let wait = Ok(Stack::Wait(
            "it stacks on example/widgets#41, which has no open pull request of bot from a dispatch/ branch of example/widgets among the newest 50".into(),
        ));
        // The bot's fork has a dispatch/ branch of that name too, but a pull
        // request's base can only be a branch of the repository itself.
        let mut forked = pull(13, "bot", "dispatch/f", "Closes #41");
        forked["isCrossRepository"] = true.into();
        let mut unknown = pull(14, "bot", "dispatch/g", "Closes #41");
        unknown["isCrossRepository"] = Value::Null;
        // The lower issue's state and the open pull requests; then the stack or error.
        for (state, pulls, expected) in [
            (
                "OPEN",
                vec![pull(7, "bot", "dispatch/41-a", "Work.\n\nCloses #41")],
                base(7, "dispatch/41-a"),
            ),
            (
                "OPEN",
                vec![pull(
                    7,
                    "Bot",
                    "dispatch/x",
                    "Fixes: https://github.com/example/widgets/issues/41.",
                )],
                base(7, "dispatch/x"),
            ),
            ("OPEN", vec![], wait.clone()),
            // Another issue, a mention, another author or branch is not the lower slice's.
            (
                "OPEN",
                vec![
                    pull(7, "bot", "dispatch/a", "Closes #410"),
                    pull(8, "bot", "dispatch/b", "See #41"),
                    pull(9, "visitor", "dispatch/c", "Closes #41"),
                    pull(10, "bot", "main", "Closes #41"),
                    pull(11, "bot", "dispatch/d", "Closes other/widgets#41"),
                    pull(12, "bot", "dispatch/e", "Part of #41"),
                    forked,
                    unknown,
                ],
                wait.clone(),
            ),
            (
                "CLOSED",
                vec![pull(7, "bot", "dispatch/a", "Closes #41")],
                Ok(Stack::Unstacked(
                    "example/widgets#41 is closed, so it runs on the default branch".into(),
                )),
            ),
            (
                "OPEN",
                vec![
                    pull(7, "bot", "dispatch/a", "Closes #41"),
                    pull(8, "bot", "dispatch/b", "Closes #41"),
                ],
                Err("2 open pull requests of bot close example/widgets#41"),
            ),
            (
                "OPEN",
                vec![pull(7, "bot", "dispatch/../main", "Closes #41")],
                Err("is not a dispatch/ branch"),
            ),
        ] {
            let forge = Forge(json!({"repository": {"issue": {"state": state},
                "pullRequests": {"nodes": pulls}}}));
            match (resolve(&forge, &dest, 41, "bot"), expected) {
                (Ok(stack), Ok(expected)) => assert_eq!(stack, expected),
                (Err(error), Err(expected)) => {
                    assert!(format!("{error:#}").contains(expected), "{error:#}")
                }
                (result, expected) => panic!("{result:?} != {expected:?}"),
            }
        }
        let missing = Forge(json!({"repository": {"issue": null}}));
        assert!(resolve(&missing, &dest, 41, "bot").is_err());
    }

    #[test]
    fn closes_table() {
        for (body, expected) in [
            ("Closes #41", true),
            ("closes #41.", true),
            ("- Closes #41", true),
            ("* fixes: #41", true),
            ("Resolved:#41", true),
            ("This closes #41", true),
            ("Work.\n\nThis change fixes #41 and more.", true),
            ("Closes #40, closes #41", true),
            ("Closes #41, closes #42", true),
            ("(closes example/widgets#41)", true),
            ("Closes EXAMPLE/Widgets#41", true),
            ("Fix https://github.com/example/widgets/issues/41!", true),
            ("Closes #40, #41", false),
            ("Closes #410", false),
            ("Closes #4", false),
            ("See #41", false),
            ("Part of #41", false),
            ("Encloses #41", false),
            ("Closes other/widgets#41", false),
            ("closes\n\n", false),
            ("", false),
        ] {
            assert_eq!(closes(body, "example/widgets", 41), expected, "{body:?}");
        }
    }

    #[test]
    fn branch_table() {
        for branch in ["dispatch/41", "dispatch/a/b-1_2.x"] {
            validate_branch(branch).unwrap();
        }
        for branch in [
            "main",
            "dispatch/",
            "dispatch/a..b",
            "dispatch//a",
            "dispatch/a.lock",
            "dispatch/a/",
            "dispatch/a b",
            "dispatch/a\nb",
            "refs/heads/dispatch/a",
            &format!("dispatch/{}", "a".repeat(BRANCH_LIMIT)),
        ] {
            assert!(validate_branch(branch).is_err(), "{branch:?}");
        }
    }
}
