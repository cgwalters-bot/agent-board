//! Isolated Git replay. Never check out or execute a candidate's code, hooks or
//! configuration, and never update its approved ref. Only create a new ref.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    fs::{self, File},
    path::PathBuf,
    process::{Command as Process, Stdio},
    time::{Duration, Instant},
};

const GIT_SECONDS: u64 = 120;
const FETCH_DEPTH: &str = "1001";

pub(super) trait Rewrite {
    fn prepare(
        &mut self,
        config: &Config,
        target: &pass::promote::Fork,
        pull: &Value,
        compare: &Value,
    ) -> Result<(String, String)>;
    fn publish(&mut self, repo: &str, branch: &str, head: &str) -> Result<()>;
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
        compare: &Value,
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
        let base = compare["base_commit"]["sha"]
            .as_str()
            .filter(|s| oid(s))
            .context("invalid publication base")?;
        for (repo, sha) in [(repo, head), (target.upstream.as_str(), base)] {
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
        self.replay(config, pull, compare)
    }

    fn publish(&mut self, repo: &str, branch: &str, head: &str) -> Result<()> {
        // Empty expected value is create-only. A retry accepts an existing ref
        // only at this exact deterministic head; never force-update any ref.
        let url = format!("https://github.com/{repo}.git");
        let reference = format!("refs/heads/{branch}");
        let existing = self.run(&["ls-remote", &url, &reference], &[])?;
        if !existing.is_empty() {
            ensure!(
                existing == format!("{head}\t{reference}"),
                "promotion branch already exists at another head"
            );
            return Ok(());
        }
        self.run(
            &[
                "push",
                &format!("--force-with-lease={reference}:"),
                &url,
                &format!("{head}:{reference}"),
            ],
            &[],
        )?;
        Ok(())
    }
}

impl Git {
    fn replay(&self, config: &Config, pull: &Value, compare: &Value) -> Result<(String, String)> {
        let head = pull["head"]["sha"]
            .as_str()
            .filter(|s| oid(s))
            .context("invalid head")?;
        let base = compare["base_commit"]["sha"]
            .as_str()
            .filter(|s| oid(s))
            .context("invalid publication base")?;
        let ancestor = compare["merge_base_commit"]["sha"]
            .as_str()
            .filter(|s| oid(s))
            .context("invalid merge base")?;
        let signoff = config
            .promote
            .as_ref()
            .and_then(|p| p.signoff.as_ref())
            .context("missing sign-off identity")?;
        let commits = compare["commits"].as_array().context("missing commits")?;
        let mut parent = ancestor.to_owned();
        let mut rebased = base.to_owned();
        // merge-tree performs a three-way merge of each original patch onto the
        // rebased parent. Synthetic commits share the *original* parent, so Git
        // cannot accidentally replay the earlier patches a second time.
        for c in commits {
            let sha = c["sha"]
                .as_str()
                .filter(|s| oid(s))
                .context("invalid commit")?;
            ensure!(
                c["parents"]
                    .as_array()
                    .is_some_and(|p| p.len() == 1 && p[0]["sha"] == parent),
                "promotion requires a linear commit chain"
            );
            ensure!(
                c["author"]["login"]
                    .as_str()
                    .is_some_and(|a| a.eq_ignore_ascii_case(&config.bot)
                        || a.eq_ignore_ascii_case(&config.operator)),
                "promotion refuses another author's commit"
            );
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
        Ok((rebased, format!("bot/promote/{head}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replay_preserves_authors_and_trailers_signs_every_commit_and_holds_conflicts() {
        let config =
            Config::parse(include_str!("../../../../fixtures/promote/config.json")).unwrap();
        let mut git = Git::default();
        git.init().unwrap();
        let identity = [
            ("GIT_AUTHOR_NAME", "Original author"),
            ("GIT_AUTHOR_EMAIL", "author@example.invalid"),
            ("GIT_COMMITTER_NAME", "Original committer"),
            ("GIT_COMMITTER_EMAIL", "committer@example.invalid"),
            ("GIT_AUTHOR_DATE", "2026-11-10T00:00:00Z"),
            ("GIT_COMMITTER_DATE", "2026-11-10T00:00:00Z"),
        ];
        let commit = |parent: Option<&str>, path: &str, content: &str, message: &str| {
            let file = git.directory.as_ref().unwrap().join("blob-input");
            fs::write(&file, content).unwrap();
            let blob = git
                .run(&["hash-object", "-w", file.to_str().unwrap()], &[])
                .unwrap();
            if let Some(parent) = parent {
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
            if let Some(parent) = parent {
                args.extend(["-p", parent]);
            }
            git.run(&args, &identity).unwrap()
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
            let compare = json!({"base_commit":{"sha":base},"merge_base_commit":{"sha":ancestor},"commits":[
                {"sha":first,"parents":[{"sha":ancestor}],"author":{"login":config.bot},"commit":{"message":"First\n\nGenerated-by: preserved"}},
                {"sha":second,"parents":[{"sha":first}],"author":{"login":config.bot},"commit":{"message":"Second"}}
            ]});
            let result = git.replay(&config, &pull, &compare);
            if conflict {
                assert!(result.is_err());
                continue;
            }
            let (head, branch) = result.unwrap();
            assert_eq!(
                (head.clone(), branch),
                git.replay(&config, &pull, &compare).unwrap(),
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
}
