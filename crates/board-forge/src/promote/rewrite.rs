//! Isolated Git replay. Never check out or execute a candidate's code, hooks or
//! configuration, and never update its approved ref. An authenticated prior
//! publication may replace its separate bot branch under an exact lease.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    path::PathBuf,
    process::{Command as Process, Stdio},
    time::{Duration, Instant},
};

const GIT_SECONDS: u64 = 120;
const FETCH_DEPTH: &str = "1001";

/// The deterministic publication branch of an approved head. It names the
/// approved commit only: a rewrite onto a later upstream base is a new head
/// on the same name, so a retry must read its receipt before rebuilding.
pub(super) fn branch(head: &str) -> String {
    format!("bot/promote/{head}")
}

/// The PR's own commits: those on the reviewed head that its fork base (the
/// fork's default branch) does not reach, as exact SHAs. A fork's marker and
/// merge commits are never promoted. [`own`] is the only place this list is
/// derived. Upstream validation can select a suffix without discarding the
/// approved fork range; all publication steps use the selected suffix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Commits {
    pub(super) fork_base: String,
    /// Where the commits branch from the fork base's history.
    parent: String,
    /// The fork comparison's rows, oldest first; the last is the head.
    rows: Vec<Value>,
    /// Start of the publication suffix and its parent, if a prefix is upstream.
    suffix: Option<(usize, String)>,
}

/// What a receipt records of a publication: the PR's remaining commits and the
/// upstream commit they were validated or replayed on. The publication
/// carries exactly these commits only while upstream contains `onto`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Cut {
    pub(super) fork_base: String,
    pub(super) commits: Vec<String>,
    pub(super) onto: String,
}

/// Compute the PR's own commits once, from one bounded comparison of the
/// fork base with the approved head.
pub(super) fn own(client: &impl Rest, repo: &str, fork_base: &str, head: &str) -> Result<Commits> {
    let fork = comparison(
        client,
        &format!("repos/{repo}/compare/{fork_base}...{head}"),
        head,
    )?;
    ensure!(
        fork["base_commit"]["sha"] == fork_base,
        "fork comparison base mismatch"
    );
    let rows = fork["commits"]
        .as_array()
        .context("missing fork comparison commits")?
        .clone();
    hold!(
        !rows.is_empty(),
        "no commits of its own beyond fork base {fork_base}"
    );
    Ok(Commits {
        fork_base: fork_base.into(),
        parent: fork["merge_base_commit"]["sha"]
            .as_str()
            .filter(|s| oid(s))
            .context("invalid fork merge base")?
            .into(),
        rows,
        suffix: None,
    })
}

impl Commits {
    /// A complete upstream comparison omits commits it already reaches. Drop
    /// only a prefix of the approved range, never a hole, and never adopt a
    /// row from upstream. Keep the full fork evidence so revalidation can check a new tip
    /// without assuming the prefix is still upstream.
    pub(super) fn unintegrated<'a>(&mut self, upstream: &'a Value) -> Result<Vec<&'a str>> {
        let shas: Vec<_> = upstream["commits"]
            .as_array()
            .context("missing compare commits")?
            .iter()
            .map(|c| c["sha"].as_str().context("invalid compare commit sha"))
            .collect::<Result<_>>()?;
        let upstream_shas: BTreeSet<_> = shas.iter().copied().collect();
        let (integrated, remaining) = self.rows.split_at(
            self.rows
                .iter()
                .take_while(|c| !c["sha"].as_str().is_some_and(|s| upstream_shas.contains(s)))
                .count(),
        );
        let first = remaining
            .first()
            .context("no approved commits to publish")?;
        hold!(
            remaining
                .iter()
                .all(|c| c["sha"].as_str().is_some_and(|s| upstream_shas.contains(s))),
            "already-upstream PR commits do not form a prefix"
        );
        self.suffix = if integrated.is_empty() {
            None
        } else {
            let parents = first["parents"]
                .as_array()
                .context("missing commit parents")?;
            let parent = match parents.as_slice() {
                [p] => p["sha"]
                    .as_str()
                    .filter(|s| oid(s))
                    .context("invalid commit parent")?,
                // A direct publication may start with a merge. DCO still
                // validates the DAG and replay still rejects merges.
                _ => upstream["merge_base_commit"]["sha"]
                    .as_str()
                    .context("missing upstream merge base")?,
            };
            hold!(
                !upstream_shas.contains(parent)
                    && (parent == self.parent || integrated.iter().any(|c| c["sha"] == parent)),
                "remaining PR commits do not start at an integrated parent"
            );
            Some((integrated.len(), parent.to_owned()))
        };
        Ok(shas)
    }

    pub(super) fn parent(&self) -> &str {
        self.suffix
            .as_ref()
            .map_or(&self.parent, |(_, parent)| parent)
    }

    pub(super) fn rows(&self) -> &[Value] {
        &self.rows[self.suffix.as_ref().map_or(0, |(start, _)| *start)..]
    }

    pub(super) fn shas(&self) -> Vec<&str> {
        self.rows()
            .iter()
            .filter_map(|c| c["sha"].as_str())
            .collect()
    }

    /// The head these commits end at; [`comparison`] proved it is the head
    /// compared.
    pub(super) fn head(&self) -> &str {
        self.rows
            .last()
            .and_then(|c| c["sha"].as_str())
            .unwrap_or_default()
    }

    /// These commits as a comparison against upstream commit `onto`, as tests
    /// record a replay.
    #[cfg(test)]
    pub(super) fn onto(&self, onto: &str) -> Value {
        serde_json::json!({
            "base_commit": {"sha": onto},
            "merge_base_commit": {"sha": self.parent()},
            "ahead_by": self.rows().len(),
            "total_commits": self.rows().len(),
            "commits": self.rows(),
        })
    }

    pub(super) fn cut(&self, onto: &str) -> Cut {
        Cut {
            fork_base: self.fork_base.clone(),
            commits: self.shas().into_iter().map(str::to_owned).collect(),
            onto: onto.into(),
        }
    }

    /// Whether `cut` recorded exactly these commits.
    pub(super) fn recorded(&self, cut: &Cut) -> bool {
        cut.commits.iter().map(String::as_str).eq(self.shas())
    }
}

/// What a replay needs before any Git run: a linear chain from the merge base
/// of commits by the bot or operator. A preview reads this; only a conflict is
/// left to the replay itself.
pub(super) fn replayable(config: &Config, commits: &Commits) -> Result<()> {
    config
        .promote
        .as_ref()
        .and_then(|p| p.signoff.as_ref())
        .context("missing sign-off identity")?;
    let mut parent = commits.parent();
    for c in commits.rows() {
        let sha = c["sha"]
            .as_str()
            .filter(|s| oid(s))
            .context("invalid commit")?;
        hold!(
            c["parents"]
                .as_array()
                .is_some_and(|p| p.len() == 1 && p[0]["sha"] == parent),
            "promotion requires a linear commit chain"
        );
        hold!(
            c["author"]["login"]
                .as_str()
                .is_some_and(|a| a.eq_ignore_ascii_case(&config.bot)
                    || a.eq_ignore_ascii_case(&config.operator)),
            "promotion refuses another author's commit"
        );
        parent = sha;
    }
    Ok(())
}

pub(super) trait Rewrite {
    fn prepare(
        &mut self,
        config: &Config,
        target: &pass::promote::Fork,
        pull: &Value,
        commits: &Commits,
        onto: &str,
    ) -> Result<(String, String)>;
    fn publish(
        &mut self,
        repo: &str,
        branch: &str,
        head: &str,
        previous: Option<&str>,
    ) -> Result<()>;
}

#[derive(Default)]
pub(super) struct Git {
    directory: Option<PathBuf>,
    deadline: Option<Instant>,
    authorization: Option<String>,
}

impl Drop for Git {
    fn drop(&mut self) {
        if let Some(path) = &self.directory {
            let _ = fs::remove_dir_all(path);
        }
    }
}

impl Git {
    fn init(&mut self) -> Result<()> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|e| anyhow::anyhow!("promotion temporary directory randomness: {e}"))?;
        let path = std::env::temp_dir().join(format!(
            "agent-board-promote-{:032x}",
            u128::from_ne_bytes(random)
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .context("create isolated promotion directory")?;
        self.directory = Some(path);
        self.deadline = Some(Instant::now() + Duration::from_secs(GIT_SECONDS));
        self.run(&["init", "--bare", "."], &[])?;
        Ok(())
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
        let directory = self
            .directory
            .as_ref()
            .context("promotion Git not initialized")?;
        let output = File::create(directory.join("command-output")).context("Git output")?;
        let mut command = Process::new("git");
        command
            .current_dir(directory)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", directory)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "protocol.allow=never",
                "-c",
                "protocol.https.allow=always",
                "-c",
                "commit.gpgSign=false",
            ])
            .args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(Stdio::null());
        if let Some(auth) = &self.authorization {
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader")
                .env("GIT_CONFIG_VALUE_0", auth);
        }
        let mut child = command
            .spawn()
            .context("start promotion Git (git must be installed)")?;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if self.deadline.is_some_and(|d| Instant::now() >= d) {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("promotion Git exceeded {GIT_SECONDS}s");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        ensure!(
            status.success(),
            "promotion Git {} failed (a conflicting rebase holds; no branch is published)",
            args.first().copied().unwrap_or("command")
        );
        ensure!(
            output.metadata()?.len() <= 1024 * 1024,
            "oversized promotion Git output"
        );
        Ok(fs::read_to_string(directory.join("command-output"))?
            .trim_end()
            .into())
    }
}

impl Rewrite for Git {
    fn prepare(
        &mut self,
        config: &Config,
        target: &pass::promote::Fork,
        pull: &Value,
        commits: &Commits,
        onto: &str,
    ) -> Result<(String, String)> {
        self.init()?;
        let token = std::env::var("GH_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("GITHUB_TOKEN").ok().filter(|s| !s.is_empty()))
            .context("promotion Git requires GH_TOKEN or GITHUB_TOKEN")?;
        self.authorization = Some(format!(
            "AUTHORIZATION: basic {}",
            STANDARD.encode(format!("x-access-token:{token}"))
        ));
        let repo = pull["head"]["repo"]["full_name"]
            .as_str()
            .context("missing head repository")?;
        let head = pull["head"]["sha"].as_str().context("missing head")?;
        ensure!(oid(onto), "invalid publication base");
        for (repo, sha) in [(repo, head), (target.upstream.as_str(), onto)] {
            self.run(
                &[
                    "fetch",
                    "--no-tags",
                    "--depth",
                    FETCH_DEPTH,
                    &format!("https://github.com/{repo}.git"),
                    sha,
                ],
                &[],
            )?;
        }
        self.replay(config, pull, commits, onto)
    }

    fn publish(
        &mut self,
        repo: &str,
        branch: &str,
        head: &str,
        previous: Option<&str>,
    ) -> Result<()> {
        // Initial publication is create-only. A repair's authenticated receipt
        // supplies the exact previous head of its separate publication branch.
        let url = format!("https://github.com/{repo}.git");
        let reference = format!("refs/heads/{branch}");
        let existing = self.run(&["ls-remote", &url, &reference], &[])?;
        if !existing.is_empty() {
            if existing == format!("{head}\t{reference}") {
                return Ok(());
            }
            ensure!(
                previous.is_some_and(|old| existing == format!("{old}\t{reference}")),
                "promotion branch already exists at another head"
            );
        }
        self.run(
            &[
                "push",
                &format!("--force-with-lease={reference}:{}", previous.unwrap_or("")),
                &url,
                &format!("{head}:{reference}"),
            ],
            &[],
        )?;
        Ok(())
    }
}

impl Git {
    /// Replay exactly `commits` onto upstream commit `base`.
    fn replay(
        &self,
        config: &Config,
        pull: &Value,
        commits: &Commits,
        base: &str,
    ) -> Result<(String, String)> {
        let head = pull["head"]["sha"]
            .as_str()
            .filter(|s| oid(s))
            .context("invalid head")?;
        ensure!(commits.head() == head, "replay range is for another head");
        ensure!(oid(base), "invalid publication base");
        let signoff = config
            .promote
            .as_ref()
            .and_then(|p| p.signoff.as_ref())
            .context("missing sign-off identity")?;
        replayable(config, commits)?;
        let mut parent = commits.parent().to_owned();
        let mut rebased = base.to_owned();
        // merge-tree performs a three-way merge of each original patch onto the
        // rebased parent. Synthetic commits share the *original* parent, so Git
        // cannot accidentally replay the earlier patches a second time.
        for c in commits.rows() {
            let sha = c["sha"]
                .as_str()
                .filter(|s| oid(s))
                .context("invalid commit")?;
            let tree = self.run(&["rev-parse", &format!("{rebased}^{{tree}}")], &[])?;
            let date = self.run(&["show", "-s", "--format=%cI", sha], &[])?;
            let identity = [
                ("GIT_AUTHOR_NAME", signoff.name.as_str()),
                ("GIT_AUTHOR_EMAIL", signoff.email.as_str()),
                ("GIT_COMMITTER_NAME", signoff.name.as_str()),
                ("GIT_COMMITTER_EMAIL", signoff.email.as_str()),
                ("GIT_AUTHOR_DATE", date.as_str()),
                ("GIT_COMMITTER_DATE", date.as_str()),
            ];
            let synthetic = self.run(
                &[
                    "commit-tree",
                    &tree,
                    "-p",
                    &parent,
                    "-m",
                    "Promotion replay base",
                ],
                &identity,
            )?;
            let merged = self.run(&["merge-tree", "--write-tree", &synthetic, sha], &[])?;
            let tree = merged
                .lines()
                .next()
                .filter(|s| oid(s))
                .context("invalid rebased tree")?;
            let message = c["commit"]["message"].as_str().context("missing message")?;
            let trailer = format!("Signed-off-by: {} <{}>", signoff.name, signoff.email);
            let message = if message
                .trim_end()
                .lines()
                .rev()
                .take_while(|l| !l.trim().is_empty())
                .any(|l| l.eq_ignore_ascii_case(&trailer))
            {
                message.to_owned()
            } else {
                format!("{}\n\n{trailer}\n", message.trim_end())
            };
            let name = self.run(&["show", "-s", "--format=%an", sha], &[])?;
            let email = self.run(&["show", "-s", "--format=%ae", sha], &[])?;
            let author_date = self.run(&["show", "-s", "--format=%aI", sha], &[])?;
            let mut identity = identity;
            identity[0] = ("GIT_AUTHOR_NAME", &name);
            identity[1] = ("GIT_AUTHOR_EMAIL", &email);
            identity[4] = ("GIT_AUTHOR_DATE", &author_date);
            rebased = self.run(
                &["commit-tree", tree, "-p", &rebased, "-m", &message],
                &identity,
            )?;
            parent = sha.to_owned();
        }
        ensure!(parent == head, "incomplete promotion replay");
        Ok((rebased, branch(head)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> Config {
        Config::parse(include_str!("../../../../fixtures/promote/config.json")).unwrap()
    }

    /// A commit on the first parent's tree (empty without one) that writes
    /// `content` to `path`.
    fn commit(git: &Git, parents: &[&str], path: &str, content: &str, message: &str) -> String {
        let identity = [
            ("GIT_AUTHOR_NAME", "Original author"),
            ("GIT_AUTHOR_EMAIL", "author@example.invalid"),
            ("GIT_COMMITTER_NAME", "Original committer"),
            ("GIT_COMMITTER_EMAIL", "committer@example.invalid"),
            ("GIT_AUTHOR_DATE", "2026-11-10T00:00:00Z"),
            ("GIT_COMMITTER_DATE", "2026-11-10T00:00:00Z"),
        ];
        let file = git.directory.as_ref().unwrap().join("blob-input");
        fs::write(&file, content).unwrap();
        let blob = git
            .run(&["hash-object", "-w", file.to_str().unwrap()], &[])
            .unwrap();
        if let Some(parent) = parents.first() {
            git.run(&["read-tree", parent], &[]).unwrap();
        }
        git.run(
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},{path}"),
            ],
            &[],
        )
        .unwrap();
        let tree = git.run(&["write-tree"], &[]).unwrap();
        let mut args = vec!["commit-tree", &tree, "-m", message];
        for parent in parents {
            args.extend(["-p", parent]);
        }
        git.run(&args, &identity).unwrap()
    }

    #[test]
    fn replay_preserves_authors_and_trailers_signs_every_commit_and_holds_conflicts() {
        let config = config();
        let mut git = Git::default();
        git.init().unwrap();
        let commit = |parent: Option<&str>, path: &str, content: &str, message: &str| {
            commit(&git, parent.as_slice(), path, content, message)
        };
        let ancestor = commit(None, "shared", "original\n", "Initial");
        let first = commit(
            Some(&ancestor),
            "shared",
            "topic\n",
            "First\n\nGenerated-by: preserved",
        );
        let second = commit(Some(&first), "topic", "second\n", "Second");
        let pull = json!({"head":{"sha":second}});
        for conflict in [false, true] {
            let base = commit(
                Some(&ancestor),
                if conflict { "shared" } else { "upstream" },
                "upstream\n",
                "Upstream change",
            );
            let commits = Commits {
                fork_base: ancestor.clone(),
                parent: ancestor.clone(),
                rows: vec![
                    json!({"sha":first,"parents":[{"sha":ancestor}],"author":{"login":config.bot},"commit":{"message":"First\n\nGenerated-by: preserved"}}),
                    json!({"sha":second,"parents":[{"sha":first}],"author":{"login":config.bot},"commit":{"message":"Second"}}),
                ],
                suffix: None,
            };
            let result = git.replay(&config, &pull, &commits, &base);
            if conflict {
                assert!(result.is_err());
                continue;
            }
            let (head, branch) = result.unwrap();
            assert_eq!(
                (head.clone(), branch),
                git.replay(&config, &pull, &commits, &base).unwrap(),
                "retry must be deterministic"
            );
            let commits = git
                .run(&["rev-list", "--reverse", &format!("{base}..{head}")], &[])
                .unwrap();
            assert_eq!(commits.lines().count(), 2);
            for sha in commits.lines() {
                let message = git.run(&["show", "-s", "--format=%B", sha], &[]).unwrap();
                assert!(
                    message.contains("Signed-off-by: Preview fixture <preview@example.invalid>")
                );
                assert_eq!(
                    git.run(&["show", "-s", "--format=%an <%ae>", sha], &[])
                        .unwrap(),
                    "Original author <author@example.invalid>"
                );
            }
            assert!(
                git.run(&["log", "--format=%B", &format!("{base}..{head}")], &[])
                    .unwrap()
                    .contains("Generated-by: preserved")
            );
            assert_eq!(
                git.run(&["show", &format!("{head}:upstream")], &[])
                    .unwrap(),
                "upstream"
            );
            assert_eq!(
                git.run(&["show", &format!("{head}:shared")], &[]).unwrap(),
                "topic"
            );
            assert_eq!(
                git.run(&["show", &format!("{head}:topic")], &[]).unwrap(),
                "second"
            );
        }
    }

    /// cgwalters-forge/ostree#10: a fork whose main carries another author's
    /// merge and a marker commit replays only the PR's commit.
    #[test]
    fn replay_skips_the_forks_own_base_commits() {
        let config = config();
        let mut git = Git::default();
        git.init().unwrap();
        let ancestor = commit(&git, &[], "shared", "original\n", "Initial");
        let side = commit(&git, &[&ancestor], "side", "side\n", "Fork change");
        let merge = commit(&git, &[&ancestor, &side], "side", "side\n", "Merge #6");
        let marker = commit(&git, &[&merge], "marker", "marker\n", "Marker");
        let pr = commit(&git, &[&marker], "shared", "topic\n", "The PR");
        let base = commit(&git, &[&ancestor], "upstream", "upstream\n", "Upstream");
        let row = |sha: &str, parents: &[&str], login: &str| {
            json!({"sha": sha, "parents": parents.iter().map(|p| json!({"sha": p})).collect::<Vec<_>>(),
                "author": {"login": login}, "commit": {"message": "Message"}})
        };
        // Cut at the upstream merge base, the fork's commits would be replayed
        // too, and the merge holds.
        let whole = Commits {
            fork_base: ancestor.clone(),
            parent: ancestor.clone(),
            rows: vec![
                row(&side, &[&ancestor], "visitor"),
                row(&merge, &[&ancestor, &side], "visitor"),
                row(&marker, &[&merge], &config.bot),
                row(&pr, &[&marker], &config.bot),
            ],
            suffix: None,
        };
        assert!(replayable(&config, &whole).is_err());
        // The fork comparison at the marker holds the PR's commit alone.
        let pull = json!({"head": {"sha": pr}, "base": {"sha": marker}});
        let own = Commits {
            fork_base: marker.clone(),
            parent: marker.clone(),
            rows: vec![row(&pr, &[&marker], &config.bot)],
            suffix: None,
        };
        assert_eq!(own.shas(), [pr.as_str()]);
        assert_eq!(own.head(), pr);
        let compare = own.onto(&base);
        assert_eq!(compare["merge_base_commit"]["sha"], marker);
        assert_eq!(
            (
                compare["ahead_by"].clone(),
                compare["total_commits"].clone()
            ),
            (json!(1), json!(1))
        );
        let cut = own.cut(&base);
        assert!(own.recorded(&cut));
        assert!(!whole.recorded(&cut));
        replayable(&config, &own).unwrap();
        assert!(
            git.replay(&config, &json!({"head": {"sha": marker}}), &own, &base)
                .is_err(),
            "a range is replayed only for its own head"
        );
        let (head, _) = git.replay(&config, &pull, &own, &base).unwrap();
        let replayed = git
            .run(&["rev-list", &format!("{base}..{head}")], &[])
            .unwrap();
        assert_eq!(replayed.lines().count(), 1);
        let files = git.run(&["ls-tree", "--name-only", &head], &[]).unwrap();
        assert_eq!(files.lines().collect::<Vec<_>>(), ["shared", "upstream"]);
        assert_eq!(
            git.run(&["show", &format!("{head}:shared")], &[]).unwrap(),
            "topic"
        );
        // An upstream change to the PR's file still holds as a conflict.
        let conflicting = commit(&git, &[&ancestor], "shared", "upstream\n", "Upstream");
        assert!(git.replay(&config, &pull, &own, &conflicting).is_err());
    }

    #[test]
    fn revalidation_keeps_the_original_fork_range() {
        let [base, a, b, c] = ['a', 'b', 'c', 'd'].map(|ch| ch.to_string().repeat(40));
        let row = |sha: &str, parent: &str| json!({"sha":sha,"parents":[{"sha":parent}]});
        let mut commits = Commits {
            fork_base: base.clone(),
            parent: base.clone(),
            rows: vec![row(&a, &base), row(&b, &a), row(&c, &b)],
            suffix: None,
        };
        let comparison = |rows: Vec<Value>| json!({"commits":rows});
        commits
            .unintegrated(&comparison(vec![row(&c, &b)]))
            .unwrap();
        assert_eq!(commits.shas(), [&c]);
        assert_eq!(commits.parent(), b);
        let cut = commits.cut(&b);

        // A rewind cannot keep using a suffix derived from the previous tip.
        commits
            .unintegrated(&comparison(vec![row(&a, &base), row(&b, &a), row(&c, &b)]))
            .unwrap();
        assert_eq!(commits.shas(), [&a, &b, &c]);
        assert_eq!(commits.parent(), base);
        assert!(!commits.recorded(&cut));

        // An absent commit inside the remaining range is not a prefix.
        let error = commits
            .unintegrated(&comparison(vec![row(&a, &base), row(&c, &b)]))
            .unwrap_err();
        assert!(error.to_string().contains("do not form a prefix"));
    }
}
