//! The supervisor agent's task as the caller's workflow hands it to the
//! wrapper, moved from tracker's `supervisor-profile.cjs` and `session.cjs`:
//! a reviewed profile and skill read at a pinned commit, then the trigger's
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

/// `agent.profile`: the repository holding the profile and a full commit of
/// it, so that a config edit cannot point the read at a moving branch or
/// another API path.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub repository: String,
    pub commit: String,
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
        let commit = value["ref"]
            .as_str()
            .filter(|r| r.len() == 40 && r.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')))
            .with_context(|| format!("{what}: ref must be a full commit id"))?;
        Ok(Some(Self {
            repository: repository.to_owned(),
            commit: commit.to_owned(),
        }))
    }

    /// The profile, then the skill, as the agent reads them.
    pub fn read(&self, api: &dyn Api) -> Result<String> {
        Ok(format!(
            "{}\n{}",
            self.file(api, PROFILE_PATH)?,
            self.file(api, SKILL_PATH)?
        ))
    }

    fn file(&self, api: &dyn Api, path: &str) -> Result<String> {
        let what = format!("{}/{path} at {}", self.repository, self.commit);
        let file = api
            .get(&format!(
                "/repos/{}/contents/{path}?ref={}",
                self.repository, self.commit
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
            commit: COMMIT.into(),
        }
    }

    fn requests() -> IssueRef {
        IssueRef {
            repository: "example/control".into(),
            issue: 5,
        }
    }

    #[test]
    fn the_supervisor_task_composes_the_pinned_profile_and_the_operator_request() {
        let text = profile().read(&home()).unwrap();
        let event = json!({"comment": {"user": {"login": "owner"}, "body": "Look at #9."},
            "issue": {"number": 5, "locked": true}});
        let request = request(&event, "", "owner", &requests()).unwrap();
        let task = supervisor_task(&text, "PLAYBOOK", &request).unwrap();
        assert_eq!(task, fixture("supervisor-task.md"));
        // Another commit or repository is not read.
        let moved = Profile {
            commit: "f".repeat(40),
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
