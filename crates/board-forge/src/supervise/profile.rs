//! The supervisor agent's task as the caller's workflow hands it to the
//! wrapper, moved from tracker's `supervisor-profile.cjs` and `session.cjs`:
//! a profile and skill read at one resolved commit, then the trigger's
//! task and the checked operator request ([`supervisor_task`]), or the
//! session's rules and opening prompt ([`session`]).
use super::{Api, IssueRef, agent::MAX_TASK_BYTES};
use crate::poll::{check_keys, validate_repository};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

/// The files read from the profile repository, agenthome's layout. The skill
/// is included too: pools other than opencode's do not install a
/// repository's skills.
pub const PROFILE_PATH: &str = ".github/agentic-job/profiles/supervisor.md";
pub const SKILL_PATH: &str = "skills/supervisor/SKILL.md";
/// Each file's bound, as the script's buffer was.
const MAX_FILE_BYTES: usize = 64 * 1024;
/// The session task's bound.
const MAX_SESSION_TASK_BYTES: usize = 60_000;
/// The idle timeout a session may ask for, in minutes.
const MAX_IDLE_MINUTES: u64 = 60;
const PROFILE_KEYS: &[&str] = &["repository", "ref"];
const SESSION_KEYS: &[&str] = &["enabled", "repository", "issue", "idleMinutes"];

/// `agent.profile`: the repository holding the profile and a full commit or
/// branch name. Branches are resolved once before either file is read.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub repository: String,
    pub reference: String,
}

/// The exact text and commit used for this run, not a moving branch.
pub struct LoadedProfile {
    pub commit: String,
    pub text: String,
}

pub fn is_commit(reference: &str) -> bool {
    reference.len() == 40
        && reference
            .bytes()
            .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

impl Profile {
    pub(crate) fn from_json(value: Option<&Value>) -> Result<Option<Self>> {
        let Some(value) = value.filter(|v| !v.is_null()) else {
            return Ok(None);
        };
        let what = "supervisor agent.profile";
        check_keys(&[PROFILE_KEYS], value, what)?;
        let repository = value["repository"]
            .as_str()
            .with_context(|| format!("{what}: repository"))?;
        validate_repository(repository).context(what)?;
        let reference = value["ref"]
            .as_str()
            .with_context(|| format!("{what}: ref must be a full commit id or branch name"))?;
        if !is_commit(reference) {
            // Prevent a ref from injecting a query or escaping the branch API
            // path. Only plain branch names, not URLs or revision expressions.
            crate::overlap::validate_branch(reference).context(what)?;
            ensure!(
                !reference.starts_with("refs/")
                    && reference.split('/').all(|part| !part.is_empty()
                        && !part.starts_with('.')
                        && !part.ends_with(['.', '/'])
                        && !part.ends_with(".lock")),
                "{what}: ref must be a full commit id or branch name"
            );
        }
        Ok(Some(Self {
            repository: repository.to_owned(),
            reference: reference.to_owned(),
        }))
    }

    /// The profile, then the skill, at the same commit even if the branch moves.
    pub fn read(&self, api: &dyn Api) -> Result<LoadedProfile> {
        let commit = if is_commit(&self.reference) {
            self.reference.clone()
        } else {
            let branch = api
                .get(&format!(
                    "/repos/{}/git/ref/heads/{}",
                    self.repository, self.reference
                ))
                .context("resolve supervisor profile branch")?;
            ensure!(
                branch["object"]["type"] == "commit",
                "profile branch does not point to a commit"
            );
            branch["object"]["sha"]
                .as_str()
                .filter(|s| is_commit(s))
                .context("profile branch has no full commit id")?
                .to_owned()
        };
        let text = format!(
            "{}\n{}",
            self.file(api, PROFILE_PATH, &commit)?,
            self.file(api, SKILL_PATH, &commit)?
        );
        Ok(LoadedProfile { commit, text })
    }

    /// Carry the composition step's commit into execution without resolving the
    /// branch again. A branch caller must wire the trusted step output through.
    pub fn execution_commit(&self, composed: Option<&str>) -> Result<String> {
        let commit = composed
            .or_else(|| is_commit(&self.reference).then_some(self.reference.as_str()))
            .context("branch profile requires --profile-commit from supervise compose")?;
        ensure!(is_commit(commit), "profile commit must be a full commit id");
        ensure!(
            !is_commit(&self.reference) || commit == self.reference,
            "profile commit differs from configured pin"
        );
        Ok(commit.to_owned())
    }

    pub fn provenance(&self, commit: &str) -> String {
        format!("Supervisor profile: {} at {commit}", self.repository)
    }

    fn file(&self, api: &dyn Api, path: &str, commit: &str) -> Result<String> {
        let what = format!("{}/{path} at {commit}", self.repository);
        let file = api
            .get(&format!(
                "/repos/{}/contents/{path}?ref={}",
                self.repository, commit
            ))
            .with_context(|| format!("read {what}"))?;
        ensure!(file["encoding"] == "base64", "{what}: not a small file");
        let bytes = super::health::base64_decode(file["content"].as_str().unwrap_or_default())?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_FILE_BYTES,
            "{what}: empty or over {MAX_FILE_BYTES} bytes"
        );
        String::from_utf8(bytes).with_context(|| format!("{what}: not UTF-8"))
    }
}

/// The operator request the task carries: an issue comment's body, admitted
/// only from the operator on the locked requests issue, else the dispatch's
/// prompt. The workflow's `if` gates the same before checkout; this repeats
/// it against the trusted files.
pub fn request(event: &Value, prompt: &str, operator: &str, requests: &IssueRef) -> Result<String> {
    let comment = &event["comment"];
    if comment.is_null() {
        return Ok(prompt.to_owned());
    }
    let issue = &event["issue"];
    ensure!(
        comment["user"]["login"] == operator
            && issue["number"].as_u64() == Some(requests.issue)
            && issue["locked"] == true
            && issue["pull_request"].is_null(),
        "not an operator comment on the locked requests issue"
    );
    comment["body"]
        .as_str()
        .map(str::to_owned)
        .context("comment has no body")
}

/// The supervisor agent's task: the profile and skill, then the trigger's
/// task (the playbook, policy and report) and the operator's public request.
/// The skill says what to read first; the caller's own notes belong there,
/// not in code.
pub fn supervisor_task(profile: &str, task: &str, request: &str) -> Result<String> {
    let text = format!("{profile}\n\n{task}\n\nOperator request (public):\n{request}\n");
    ensure!(
        text.len() <= MAX_TASK_BYTES,
        "supervisor task exceeds {MAX_TASK_BYTES} bytes"
    );
    Ok(text)
}

/// What a session's `$GITHUB_OUTPUT` carries, in order.
pub type Outputs = Vec<(&'static str, String)>;

/// An operator session: the supervisor talking with the operator. `config`
/// is the caller's session file, `rules` the session's own profile, which
/// comes first so that the agent reads it before the supervisor's profile
/// tells it to start a pass. `idle` is the dispatch's input, empty for the
/// file's default; `prompt` the opening prompt, task data.
#[allow(clippy::too_many_arguments)]
pub fn session(
    config: &Value,
    pools: &Value,
    rules: &str,
    state: Option<&IssueRef>,
    requests: &IssueRef,
    profile: &str,
    pool: &str,
    idle: &str,
    prompt: &str,
) -> Result<Outputs> {
    check_keys(&[SESSION_KEYS], config, "session config")?;
    ensure!(
        config["enabled"] == true,
        "sessions are disabled in the session config"
    );
    let repository = config["repository"]
        .as_str()
        .context("session config: repository")?;
    validate_repository(repository).context("session config")?;
    let issue = config["issue"]
        .as_u64()
        .filter(|n| *n > 0)
        .context("session config: issue")?;
    let minutes = if idle.is_empty() {
        config["idleMinutes"]
            .as_u64()
            .context("session config: idleMinutes")?
    } else {
        // Digits only: no sign, space, leading zero or second line.
        ensure!(
            idle.len() <= 2 && !idle.starts_with('0') && idle.bytes().all(|c| c.is_ascii_digit()),
            "idle minutes must be 1 to {MAX_IDLE_MINUTES}"
        );
        idle.parse()?
    };
    ensure!(
        (1..=MAX_IDLE_MINUTES).contains(&minutes),
        "idle minutes must be 1 to {MAX_IDLE_MINUTES}"
    );
    let Some(chosen) = pools.get(pool).filter(|p| p.is_object()) else {
        bail!("unknown pool {pool:?}");
    };
    let field = |key: &str| -> Result<String> {
        match &chosen[key] {
            Value::String(s) => Ok(s.clone()),
            Value::Number(n) if key == "budget" => Ok(n.to_string()),
            _ => bail!("pool {pool} has no {key}"),
        }
    };
    let state = state.context("supervisor agent.state is not configured")?;
    let task = format!(
        "{rules}\nState issue: {}#{}. Requests issue: {}#{}.\n\n{profile}\n\nOpening prompt (task data):\n{prompt}",
        state.repository, state.issue, requests.repository, requests.issue
    );
    ensure!(
        task.len() <= MAX_SESSION_TASK_BYTES,
        "session task exceeds {MAX_SESSION_TASK_BYTES} bytes"
    );
    Ok(vec![
        ("repo", repository.to_owned()),
        ("item", issue.to_string()),
        ("idle", minutes.to_string()),
        ("agent", field("agent")?),
        ("model", field("model")?),
        ("budget", field("budget")?),
        ("task", task),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::tests::base64;
    use serde_json::json;
    use std::collections::BTreeMap;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/../../fixtures/supervise/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    /// The contents API, answering only the paths it holds.
    struct Contents(BTreeMap<String, Value>);

    impl Api for Contents {
        fn get(&self, path: &str) -> Result<Value> {
            self.0
                .get(path)
                .cloned()
                .with_context(|| format!("404 {path}"))
        }
        fn graphql(&self, _: &str, _: Value) -> Result<Value> {
            bail!("no graphql")
        }
        fn download(&self, _: &str, _: u64) -> Result<Vec<u8>> {
            bail!("no download")
        }
        fn send(&self, _: &str, _: &str, _: &Value) -> Result<Value> {
            bail!("no writes")
        }
    }

    fn home() -> Contents {
        let file = |name: &str| json!({"encoding": "base64", "content": base64(&fixture(name))});
        Contents(BTreeMap::from([
            (
                format!("/repos/example/home/contents/{PROFILE_PATH}?ref={COMMIT}"),
                file("profile.md"),
            ),
            (
                format!("/repos/example/home/contents/{SKILL_PATH}?ref={COMMIT}"),
                file("skill.md"),
            ),
        ]))
    }

    fn profile() -> Profile {
        Profile {
            repository: "example/home".into(),
            reference: COMMIT.into(),
        }
    }

    fn requests() -> IssueRef {
        IssueRef {
            repository: "example/control".into(),
            issue: 5,
        }
    }

    #[test]
    fn branches_resolve_once_before_both_files_are_read() {
        struct MovingBranch {
            home: Contents,
            resolutions: std::cell::Cell<usize>,
        }
        impl Api for MovingBranch {
            fn get(&self, path: &str) -> Result<Value> {
                if path == "/repos/example/home/git/ref/heads/main" {
                    let reads = self.resolutions.get();
                    self.resolutions.set(reads + 1);
                    return Ok(json!({"object": {"type": "commit", "sha":
                        if reads == 0 { COMMIT.to_owned() } else { "f".repeat(40) }}}));
                }
                self.home.get(path)
            }
            fn graphql(&self, _: &str, _: Value) -> Result<Value> {
                bail!("no graphql")
            }
            fn download(&self, _: &str, _: u64) -> Result<Vec<u8>> {
                bail!("no download")
            }
            fn send(&self, _: &str, _: &str, _: &Value) -> Result<Value> {
                bail!("no writes")
            }
        }
        let api = MovingBranch {
            home: home(),
            resolutions: std::cell::Cell::new(0),
        };
        let p = Profile::from_json(Some(&json!({"repository": "example/home", "ref": "main"})))
            .unwrap()
            .unwrap();
        let loaded = p.read(&api).unwrap();
        assert_eq!(api.resolutions.get(), 1);
        assert_eq!(loaded.commit, COMMIT);
        assert_eq!(loaded.text, profile().read(&home()).unwrap().text);
        assert!(p.execution_commit(None).is_err());
        assert_eq!(p.execution_commit(Some(&loaded.commit)).unwrap(), COMMIT);
        assert_eq!(profile().execution_commit(None).unwrap(), COMMIT);
        assert!(profile().execution_commit(Some(&"f".repeat(40))).is_err());
        for commit in ["main", "../main", "https://example.com"] {
            assert!(p.execution_commit(Some(commit)).is_err());
        }
    }

    #[test]
    fn only_commit_ids_and_plain_branch_names_are_accepted() {
        let parse = |reference: &Value| {
            Profile::from_json(Some(&json!({
                "repository": "example/home", "ref": reference
            })))
        };
        for reference in [COMMIT, "main", "release/v1", "topic-1.2"] {
            let p = parse(&json!(reference)).unwrap().unwrap();
            assert_eq!(p.reference, reference);
        }
        for reference in [
            "",
            "../main",
            "/tmp/main",
            "./main",
            "https://example.com/main",
            "main?ref=other",
            "main#fragment",
            "main\n",
            "HEAD~1",
            "refs/tags/v1",
            "topic//name",
            "topic/.hidden",
            "topic/name.lock",
            "topic.",
        ] {
            assert!(parse(&json!(reference)).is_err(), "{reference:?}");
        }
        for reference in [Value::Null, json!(1), json!(true), json!("x".repeat(201))] {
            assert!(parse(&reference).is_err(), "{reference}");
        }
        for object in [
            json!({}),
            json!({"type": "tag", "sha": COMMIT}),
            json!({"type": "commit", "sha": "main"}),
        ] {
            let api = Contents(BTreeMap::from([(
                "/repos/example/home/git/ref/heads/main".into(),
                json!({"object": object}),
            )]));
            let p = Profile {
                reference: "main".into(),
                ..profile()
            };
            assert!(p.read(&api).is_err());
        }
    }

    #[test]
    fn the_supervisor_task_composes_the_pinned_profile_and_the_operator_request() {
        let loaded = profile().read(&home()).unwrap();
        assert_eq!(loaded.commit, COMMIT);
        let text = loaded.text;
        let event = json!({"comment": {"user": {"login": "owner"}, "body": "Look at #9."},
            "issue": {"number": 5, "locked": true}});
        let request = request(&event, "", "owner", &requests()).unwrap();
        let task = supervisor_task(&text, "PLAYBOOK", &request).unwrap();
        assert_eq!(task, fixture("supervisor-task.md"));
        // Another commit or repository is not read.
        let moved = Profile {
            reference: "f".repeat(40),
            ..profile()
        };
        assert!(moved.read(&home()).is_err());
        let mut big = home();
        let key = format!("/repos/example/home/contents/{SKILL_PATH}?ref={COMMIT}");
        big.0
            .insert(key.clone(), json!({"encoding": "none", "content": ""}));
        assert!(profile().read(&big).is_err());
        big.0.insert(
            key,
            json!({"encoding": "base64", "content": base64(&"x".repeat(MAX_FILE_BYTES + 1))}),
        );
        assert!(profile().read(&big).is_err());
        assert!(supervisor_task("", &"x".repeat(MAX_TASK_BYTES), "").is_err());
    }

    #[test]
    fn only_the_operator_on_the_locked_requests_issue_makes_a_request() {
        let event = json!({"comment": {"user": {"login": "owner"}, "body": "do it"},
            "issue": {"number": 5, "locked": true}});
        assert_eq!(
            request(&event, "ignored", "owner", &requests()).unwrap(),
            "do it"
        );
        // A dispatch carries its prompt.
        assert_eq!(
            request(&json!({"inputs": {}}), "manual", "owner", &requests()).unwrap(),
            "manual"
        );
        for (pointer, value) in [
            ("/comment/user/login", json!("Owner")),
            ("/comment/user/login", json!("other")),
            ("/issue/number", json!(6)),
            ("/issue/locked", json!(false)),
            ("/issue/pull_request", json!({"url": "x"})),
            ("/comment/body", Value::Null),
        ] {
            let mut changed = event.clone();
            match changed.pointer_mut(pointer) {
                Some(slot) => *slot = value.clone(),
                None => changed["issue"]["pull_request"] = value.clone(),
            }
            assert!(
                request(&changed, "", "owner", &requests()).is_err(),
                "{pointer}={value}"
            );
        }
    }

    #[test]
    fn a_session_is_bounded_by_its_config_and_pools() {
        let config = json!({"enabled": true, "repository": "example/control", "issue": 7, "idleMinutes": 10});
        let pools = json!({"large": {"agent": "a", "model": "m", "budget": 100, "tier": 1},
            "small": {"agent": "b", "model": "n", "budget": "50"}});
        let state = IssueRef {
            repository: "example/control".into(),
            issue: 8,
        };
        let run = |config: &Value, pool: &str, idle: &str, prompt: &str| {
            session(
                config,
                &pools,
                "RULES",
                Some(&state),
                &requests(),
                "PROFILE",
                pool,
                idle,
                prompt,
            )
        };
        let prompt = "hello\nrepo=attacker/repo";
        let out: BTreeMap<_, _> = run(&config, "large", "", prompt)
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(
            out,
            BTreeMap::from([
                ("repo", "example/control".to_owned()),
                ("item", "7".to_owned()),
                ("idle", "10".to_owned()),
                ("agent", "a".to_owned()),
                ("model", "m".to_owned()),
                ("budget", "100".to_owned()),
                (
                    "task",
                    format!(
                        "RULES\nState issue: example/control#8. Requests issue: example/control#5.\n\nPROFILE\n\nOpening prompt (task data):\n{prompt}"
                    )
                ),
            ])
        );
        assert_eq!(run(&config, "small", "60", "").unwrap()[2].1, "60");
        assert_eq!(run(&config, "small", "", "").unwrap()[5].1, "50");
        for idle in ["0", "05", "61", "-1", "1.5", " 10", "1\nx=y", "100"] {
            assert!(run(&config, "large", idle, "").is_err(), "{idle:?}");
        }
        for pool in ["unknown", "__proto__", "../large", ""] {
            assert!(run(&config, pool, "", "").is_err(), "{pool:?}");
        }
        for (key, value) in [
            ("enabled", json!(false)),
            ("enabled", json!("true")),
            ("repository", json!("bad")),
            ("repository", json!("a/b\nx=y")),
            ("issue", json!(0)),
            ("issue", json!(1.5)),
            ("idleMinutes", json!(61)),
            ("listen", json!(true)),
        ] {
            let mut changed = config.clone();
            changed[key] = value.clone();
            assert!(run(&changed, "large", "", "").is_err(), "{key}={value}");
        }
        assert!(run(&config, "large", "", &"é".repeat(30_000)).is_err());
        assert!(
            session(&config, &pools, "", None, &requests(), "", "large", "", "").is_err(),
            "a session needs the state issue"
        );
    }
}
