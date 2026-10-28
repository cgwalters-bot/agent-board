use super::*;
use crate::pass::tests::config;
use crate::pass::usage::{self, tests::now};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const REPO: &str = "cgwalters-forge/tracker";
const OPERATOR: &str = "cgwalters";
const BOT: &str = "cgwalters-bot";

/// Called with the forge as it is asked for the next lease commit.
type Hook = Box<dyn FnOnce(&Forge)>;

/// A JSON pointer into the broker's answer, and the value put there.
type Edit = (&'static str, Value);

/// The dispatch repository's lease refs and commits (message and parent),
/// its runs (absent is deleted) and their jobs, the token's login, and the
/// bot's usage comment.
pub(in crate::pass) struct Forge {
    refs: RefCell<BTreeMap<String, String>>,
    commits: RefCell<BTreeMap<String, (String, Option<String>)>>,
    runs: BTreeMap<u64, Value>,
    caller: Vec<u64>,
    jobs: BTreeMap<u64, Value>,
    login: &'static str,
    private: Value,
    usage: Value,
    /// What happens when the next lease commit is written: another admit.
    hook: RefCell<Option<Hook>>,
    /// Whether the answer to the next ref write is lost after it is made.
    lose: Cell<bool>,
}

fn ok(body: Value) -> RestResponse {
    RestResponse {
        status: 200,
        etag: None,
        last_modified: None,
        link: None,
        body,
    }
}

fn missing() -> RestResponse {
    RestResponse {
        status: 404,
        ..ok(json!({"message": "Not Found"}))
    }
}

impl Rest for Forge {
    fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
        let path = url.strip_prefix(API_ROOT).unwrap();
        let git = format!("repos/{REPO}/git/");
        let runs = format!("repos/{REPO}/actions/runs/");
        if path == "user" {
            return Ok(ok(json!({"login": self.login})));
        }
        if path == format!("repos/{REPO}") {
            return Ok(ok(json!({"private": self.private})));
        }
        if path == "repos/cgwalters-forge/bot-ops/issues/1/comments?per_page=100" {
            let body = format!("{USAGE_MARKER}\n\n```json\n{:#}\n```\n", self.usage);
            return Ok(ok(json!([{"user": {"login": BOT}, "body": body}])));
        }
        if let Some(prefix) = path.strip_prefix(&format!("{git}matching-refs/")) {
            let refs = self.refs.borrow();
            let one = |(name, sha)| json!({"ref": format!("refs/{name}"), "object": {"sha": sha}});
            let list = (refs.iter())
                .filter(|(name, _)| name.starts_with(prefix))
                .map(one)
                .collect();
            return Ok(ok(Value::Array(list)));
        }
        if let Some(sha) = path.strip_prefix(&format!("{git}commits/")) {
            let (message, _) = self.commits.borrow().get(sha).cloned().unwrap();
            return Ok(ok(json!({"sha": sha, "message": message})));
        }
        if let Some(rest) = path.strip_prefix(&runs) {
            let (id, jobs) = match rest.split_once("/jobs?") {
                Some((id, _)) => (id.parse().unwrap(), true),
                None => (rest.parse().unwrap(), false),
            };
            let found = match jobs {
                true => self.jobs.get(&id),
                false => self.runs.get(&id),
            };
            return Ok(found.cloned().map_or_else(missing, ok));
        }
        if path.starts_with(&format!("repos/{REPO}/actions/workflows/")) {
            let status = path
                .split("status=")
                .nth(1)
                .unwrap()
                .split('&')
                .next()
                .unwrap();
            let runs: Vec<_> = self
                .caller
                .iter()
                .filter_map(|id| self.runs.get(id))
                .filter(|r| r["status"] == status)
                .cloned()
                .collect();
            return Ok(ok(
                json!({"workflow_runs": runs, "total_count": runs.len()}),
            ));
        }
        panic!("unexpected GET {path}")
    }

    fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        let path = url.strip_prefix(API_ROOT).unwrap();
        let git = format!("repos/{REPO}/git/");
        let answer = match (method, path.strip_prefix(&git).unwrap()) {
            // GitHub refuses an empty tree with a 422.
            ("POST", "trees") => {
                let entries = body["tree"].as_array().unwrap();
                if entries.is_empty() {
                    return Ok((422, json!({"message": "Invalid tree info"})));
                }
                let file = json!({"path": TREE_FILE, "mode": "100644", "type": "blob",
                    "content": TREE_TEXT});
                assert_eq!(body, &json!({"tree": [file]}));
                return Ok((201, json!({"sha": "tree"})));
            }
            ("POST", "commits") => {
                assert_eq!(body["tree"], "tree");
                let parents = body["parents"].as_array().unwrap();
                assert!(parents.len() <= 1, "{parents:?}");
                let parent = parents.first().map(|p| p.as_str().unwrap().to_owned());
                // Taken out first: the hook writes through this forge too.
                let hook = self.hook.borrow_mut().take();
                if let Some(hook) = hook {
                    hook(self);
                }
                let mut commits = self.commits.borrow_mut();
                let sha = format!("c{}", commits.len());
                let message = text(body, "/message").to_owned();
                commits.insert(sha.clone(), (message, parent));
                return Ok((201, json!({"sha": sha})));
            }
            ("POST", "refs") => {
                let name = text(body, "/ref").strip_prefix("refs/").unwrap().to_owned();
                let mut refs = self.refs.borrow_mut();
                if refs.contains_key(&name) {
                    return Ok((422, json!({"message": TAKEN})));
                }
                refs.insert(name, text(body, "/sha").to_owned());
                (201, json!({}))
            }
            // Only a fast forward, to a child of where it is.
            ("PATCH", name) => {
                assert_eq!(body["force"], false);
                let name = name.strip_prefix("refs/").unwrap();
                let sha = text(body, "/sha").to_owned();
                let parent = self.commits.borrow()[&sha].1.clone();
                let mut refs = self.refs.borrow_mut();
                match refs.get_mut(name) {
                    None => return Ok((422, json!({"message": MOVED[1]}))),
                    Some(at) if Some(&*at) != parent.as_ref() => {
                        return Ok((422, json!({"message": MOVED[0]})));
                    }
                    Some(at) => *at = sha,
                }
                (200, json!({}))
            }
            (method, path) => panic!("unexpected {method} {path}"),
        };
        match self.lose.replace(false) {
            true => bail!("connection reset"),
            false => Ok(answer),
        }
    }
}

/// `at` as the forge stamps it.
fn ago(seconds: i64) -> String {
    stamp(now() - time::Duration::seconds(seconds)).unwrap()
}

fn run(id: u64, status: &str, started: i64) -> (u64, Value) {
    let run = json!({"id": id, "status": status, "conclusion": null,
        "run_started_at": ago(started)});
    (id, run)
}

/// A forge whose usage snapshot is the recorded broker answer with `edits`
/// (JSON pointer, value) and whose token is `login`'s.
pub(in crate::pass) fn forge(login: &'static str, edits: &[Edit]) -> Forge {
    let mut broker = usage::tests::broker();
    for (pointer, value) in edits {
        *broker.pointer_mut(pointer).unwrap() = value.clone();
    }
    Forge {
        refs: RefCell::default(),
        commits: RefCell::default(),
        runs: BTreeMap::new(),
        caller: Vec::new(),
        jobs: BTreeMap::new(),
        login,
        private: json!(true),
        usage: usage::snapshot(&broker, &usage::tests::pools(), now()).unwrap(),
        hook: RefCell::new(None),
        lose: Cell::new(false),
    }
}

impl Forge {
    /// A lease of run `run` on `pool` in `slot`, expiring in `expires`
    /// seconds.
    fn lease(&self, slot: u64, pool: &str, run: u64, expires: i64) {
        let lease = json!({"run": run, "item": "o/r#1", "pool": pool,
            "taken_at": ago(LEASE_SECONDS - expires), "expires_at": ago(-expires)});
        self.raw(&format!("{LEASE_REFS}/{slot}"), &lease.to_string());
    }

    pub(in crate::pass) fn raw(&self, name: &str, message: &str) {
        let sha = format!("old-{name}");
        self.commits
            .borrow_mut()
            .insert(sha.clone(), (message.into(), None));
        self.refs.borrow_mut().insert(name.into(), sha);
    }

    fn slots(&self) -> Vec<String> {
        self.refs.borrow().keys().cloned().collect()
    }

    /// The message of the commit at `slot`.
    fn message(&self, slot: u64) -> String {
        let sha = self.refs.borrow()[&format!("{LEASE_REFS}/{slot}")].clone();
        self.commits.borrow()[&sha].0.clone()
    }
}

struct Starter {
    calls: Cell<usize>,
    answer: Option<u64>,
}

impl Rest for Starter {
    fn get(&self, _: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
        panic!("dispatch must not read")
    }
    fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
        self.calls.set(self.calls.get() + 1);
        match self.answer {
            Some(run) => Ok((200, json!({"workflow_run_id": run}))),
            None => bail!("connection reset"),
        }
    }
}

#[test]
fn pass_reservations_race_with_admission_and_bind_only_the_named_run() {
    for raced in [false, true] {
        let config = Rc::new(config(json!({"leases": true, "wip": 1})));
        let forge = forge(BOT, &[]);
        let won = raced.then(|| racing(&forge, &config, 12));
        let starter = Starter {
            calls: Cell::new(0),
            answer: Some(11),
        };
        let item = item();
        let dispatch = Dispatch {
            client: &forge,
            dispatcher: &starter,
            config: &config,
            item: &item,
            pool: "claude",
            now: now(),
            sent: Cell::new(false),
            reserved: RefCell::new(None),
        };
        let answer = dispatch.send("POST", "dispatch", &json!({}));
        if let Some(won) = won {
            assert_eq!(won.borrow().as_ref().unwrap().lease.run, 12);
            // A refusal sent nothing, so the plan's records are released.
            let error = answer.unwrap_err();
            assert!(error.is::<plan::Refused>(), "{error:#}");
            assert!(error.to_string().contains("all 1 slot(s) are leased"));
            assert_eq!(starter.calls.get(), 0);
        } else {
            answer.unwrap();
            assert_eq!(starter.calls.get(), 1);
            assert_eq!(take(&forge, &config, 11, false).0.unwrap().unwrap().slot, 0);
            assert!(take(&forge, &config, 12, false).0.unwrap().is_err());
        }
    }
}

#[test]
fn ambiguous_dispatch_keeps_its_reservation() {
    let config = config(json!({"leases": true, "wip": 2}));
    let forge = forge(BOT, &[]);
    let starter = Starter {
        calls: Cell::new(0),
        answer: None,
    };
    let item = item();
    let dispatch = Dispatch {
        client: &forge,
        dispatcher: &starter,
        config: &config,
        item: &item,
        pool: "claude",
        now: now(),
        sent: Cell::new(false),
        reserved: RefCell::new(None),
    };
    assert!(dispatch.send("POST", "dispatch", &json!({})).is_err());
    assert_eq!(
        take(&forge, &config, 11, false).0.unwrap(),
        Err("dispatch reservation pending; retry admission".into())
    );
    assert_eq!(
        read(&forge, REPO, now()).unwrap()[0]
            .held
            .as_ref()
            .unwrap()
            .lease
            .run,
        0
    );
}

#[test]
fn cold_probe_admission_is_shared_and_bounded() {
    let config = Rc::new(config(json!({"wip": 3,
        "usage": {"repository": "cgwalters-forge/bot-ops", "issue": 1,
            "url": "https://broker.invalid/usage", "probe": true}})));
    let mut forge = forge(BOT, &[]);
    // This is the exact eligibility the pass publishes after a restart.
    let mut broker = usage::tests::broker();
    for provider in ["anthropic", "codex"] {
        broker[provider] = json!({"counts": {"requests": 0, "unmetered": 0}});
    }
    forge.usage = usage::snapshot(&broker, &config.pools, now()).unwrap();
    forge.usage["probes"] = serde_json::to_value(usage::unasked(&broker, &forge.usage)).unwrap();
    assert!(forge.usage["probes"]["claude"].is_string());
    let won = racing(&forge, &config, 12);
    assert!(take(&forge, &config, 11, false).0.unwrap().is_err());
    assert_eq!(won.borrow().as_ref().unwrap().slot, 0);
    assert!(take(&forge, &config, 13, false).0.unwrap().is_err());
    let item = item();
    let other = Request {
        pool: "openai",
        item: &item,
        run: 14,
        overridden: false,
    };
    assert!(
        admit(&forge, &config, &other, now(), &mut Vec::new())
            .unwrap()
            .is_err()
    );
    // A retry of the actual probe still succeeds, even with no reading.
    assert_eq!(take(&forge, &config, 12, false).0.unwrap().unwrap().slot, 0);
    release(&forge, REPO, ("claude", 12), now()).unwrap();
    // Missing eligibility (old snapshots, unknown request counts) fails closed.
    forge.usage["probes"] = Value::Null;
    assert!(take(&forge, &config, 15, false).0.unwrap().is_err());
}

fn item() -> ItemRef {
    ItemRef {
        host: crate::github::DEFAULT_HOST.into(),
        repository: "cgwalters-forge/agent-board".into(),
        number: 382,
    }
}

/// `admit` for run `id` on claude, with `overridden`, and its lines.
fn take(
    forge: &Forge,
    config: &Config,
    id: u64,
    overridden: bool,
) -> (Result<std::result::Result<Held, String>>, Vec<String>) {
    let item = item();
    let request = Request {
        pool: "claude",
        item: &item,
        run: id,
        overridden,
    };
    let mut lines = Vec::new();
    let admitted = admit(forge, config, &request, now(), &mut lines);
    (admitted, lines)
}

/// Makes the next lease commit wait for run `id`'s admit, whose answer is
/// kept in the cell returned.
fn racing(forge: &Forge, config: &Rc<Config>, id: u64) -> Rc<RefCell<Option<Held>>> {
    let won = Rc::new(RefCell::new(None));
    let (saw, config) = (won.clone(), config.clone());
    *forge.hook.borrow_mut() = Some(Box::new(move |forge: &Forge| {
        *saw.borrow_mut() = take(forge, &config, id, false).0.unwrap().ok();
    }));
    won
}

/// Two admits race for the last slot of two: the second reads the leases
/// after the first did, and creates its ref before the first. Exactly one
/// wins; the other is refused with the reason.
#[test]
fn racing_admits_for_the_last_slot() {
    let config = Rc::new(config(json!({"wip": 2})));
    let mut forge = forge(BOT, &[]);
    forge.runs.extend([run(10, "in_progress", 60)]);
    forge.lease(0, "claude", 10, 3600);
    let won = racing(&forge, &config, 12);
    let (mine, lines) = take(&forge, &config, 11, false);
    assert_eq!(lines, Vec::<String>::new());
    let theirs = won.borrow_mut().take().unwrap();
    assert_eq!((theirs.slot, theirs.lease.run), (1, 12));
    let refused = "all 2 slot(s) are leased";
    assert_eq!(mine.unwrap(), Err(refused.into()));
    assert_eq!(
        forge.slots(),
        ["agent-board/leases/0", "agent-board/leases/1"]
    );
    // The winner's commit is its lease.
    let message: Value = serde_json::from_str(&forge.message(1)).unwrap();
    assert_eq!(
        message,
        json!({"run": 12, "item": "cgwalters-forge/agent-board#382", "pool": "claude",
            "taken_at": "2026-10-23T15:15:10.452609311Z",
            "expires_at": "2026-10-24T15:15:10.452609311Z"})
    );
    // Leases are counted, not their slots, and every pool's: two past `wip`
    // 2, as after it was lowered, one of them openai's, leave claude none.
    let mut forge = self::forge(BOT, &[]);
    forge.runs.extend([run(10, "in_progress", 60)]);
    forge.lease(5, "openai", 10, 3600);
    forge.lease(6, "claude", 10, 3600);
    let (admitted, _) = take(&forge, &config, 11, false);
    assert_eq!(admitted.unwrap(), Err(refused.into()));
}

#[test]
fn unleased_caller_work_leaves_only_shared_remaining_capacity() {
    let config = Rc::new(config(json!({"wip": 2})));
    let mut forge = forge(BOT, &[]);
    forge.runs.extend([run(10, "in_progress", 60)]);
    forge.caller.push(10);
    let won = racing(&forge, &config, 12);
    assert!(take(&forge, &config, 11, false).0.unwrap().is_err());
    assert_eq!(won.borrow().as_ref().unwrap().slot, 0);
    assert!(take(&forge, &config, 13, false).0.unwrap().is_err());
    // The already-counted unleased run can join the protocol without being
    // charged twice. A further admission still cannot pass total WIP.
    assert_eq!(take(&forge, &config, 10, false).0.unwrap().unwrap().slot, 1);
    assert!(take(&forge, &config, 14, false).0.unwrap().is_err());
}

#[test]
fn concurrent_retries_never_take_a_second_slot() {
    let config = Rc::new(config(json!({"wip": 2})));
    let forge = forge(BOT, &[]);
    let won = racing(&forge, &config, 11);
    assert!(take(&forge, &config, 11, false).0.unwrap().is_err());
    assert_eq!(won.borrow().as_ref().unwrap().slot, 0);
    assert_eq!(take(&forge, &config, 11, false).0.unwrap().unwrap().slot, 0);
    assert_eq!(forge.slots().len(), 1);
    // Retries that take different slots, as one turns over between their
    // reads, leave the run its lowest: the later writer frees the other.
    let forge = self::forge(BOT, &[]);
    forge.lease(0, "claude", 10, 3600);
    let raced = config.clone();
    let won = Rc::new(RefCell::new(None));
    let saw = won.clone();
    *forge.hook.borrow_mut() = Some(Box::new(move |forge: &Forge| {
        release(forge, REPO, ("claude", 10), now()).unwrap();
        *saw.borrow_mut() = take(forge, &raced, 11, false).0.unwrap().ok();
    }));
    let mine = take(&forge, &config, 11, false).0.unwrap().unwrap();
    assert_eq!((mine.slot, won.borrow().as_ref().unwrap().slot), (0, 0));
    let live: Vec<_> = (read(&forge, REPO, now()).unwrap().into_iter())
        .filter_map(|f| f.held.map(|h| (h.slot, h.lease.run)))
        .collect();
    assert_eq!(live, [(0, 11)]);
    assert_eq!(forge.message(1), "a second lease of run 11");
}

#[test]
fn lease_objects_are_never_written_to_an_unsafe_destination() {
    let config = config(json!({}));
    for private in [json!(false), Value::Null, json!("true")] {
        let mut forge = forge(BOT, &[]);
        forge.private = private;
        let error = take(&forge, &config, 11, false).0.unwrap_err();
        assert!(error.to_string().contains("not known to be private"));
        assert!(forge.refs.borrow().is_empty());
        assert!(forge.commits.borrow().is_empty());
    }
}

/// Two admits race to reap the same expired lease for the one slot: the
/// one that moves the ref first takes it, and the other's move, from the
/// commit both read, is no fast forward, so it is refused rather than
/// freeing the winner's slot. A pass's reap from that stale read likewise
/// leaves the winner's lease in place.
#[test]
fn a_stale_reap_frees_no_new_lease() {
    let config = Rc::new(config(json!({"wip": 1})));
    let forge = forge(BOT, &[]);
    forge.lease(0, "claude", 1, -1);
    let stale = read(&forge, REPO, now()).unwrap();
    let won = racing(&forge, &config, 12);
    let (mine, lines) = take(&forge, &config, 11, false);
    assert_eq!(mine.unwrap(), Err("all 1 slot(s) are leased".into()));
    assert_eq!(lines, Vec::<String>::new());
    assert_eq!(won.borrow_mut().take().unwrap().lease.run, 12);
    assert!(!free(&forge, REPO, &stale[0], "reaped: expired").unwrap());
    let held = read(&forge, REPO, now()).unwrap();
    assert_eq!(held[0].held.as_ref().unwrap().lease.run, 12);
    assert_eq!(forge.slots(), ["agent-board/leases/0"]);
}

/// Pacing that holds claude, its five hours used up or its week past its
/// pace, leaves it no slot however free `wip` is, as does `wip` 0 or a
/// spent budget; the operator's override takes one all the same.
#[test]
fn pacing_holds_admit() {
    let five_hours = [("/anthropic/unified_5h/utilization", json!(1.0))];
    let week = [("/anthropic/unified_7d/utilization", json!(0.96))];
    let cases: &[(&[Edit], Value, &str)] = &[
        (
            &five_hours,
            json!({}),
            "pacing holds claude: claude is held until 2026-10-23T17:00:00Z",
        ),
        (
            &week,
            json!({}),
            "pacing holds claude: claude is held until 2026-10-26T23:00:00Z",
        ),
        (&[], json!({"wip": 0, "triage": false}), "wip is 0"),
        (
            &[],
            json!({"budget": {"weeklyUsd": 100}, "usage": {"repository": "cgwalters-forge/bot-ops",
                "issue": 1, "url": "https://broker.invalid/usage"}}),
            "budget: the usage snapshot has no spend reading",
        ),
    ];
    for (edits, settings, why) in cases {
        let config = config(settings.clone());
        let forge = forge(BOT, edits);
        let (admitted, _) = take(&forge, &config, 11, false);
        assert_eq!(admitted.unwrap(), Err(why.to_string()), "{why}");
        assert!(forge.slots().is_empty(), "{why}");
        let forge = Forge {
            login: OPERATOR,
            ..forge
        };
        let (admitted, lines) = take(&forge, &config, 11, true);
        let held = admitted.unwrap().unwrap();
        assert!(held.lease.overridden, "{why}");
        assert_eq!(lines, ["admit: override by cgwalters, past wip and pacing"]);
    }
    // Unpaced, claude has room.
    let (admitted, _) = take(&forge(BOT, &[]), &config(json!({})), 11, false);
    assert_eq!(admitted.unwrap().unwrap().slot, 0);
}

/// The bot's token, or one that cannot say whose it is, cannot override.
#[test]
fn override_is_the_operators_alone() {
    let config = config(json!({}));
    for login in [BOT, ""] {
        let forge = forge(login, &[("/anthropic/unified_5h/utilization", json!(1.0))]);
        let (admitted, lines) = take(&forge, &config, 11, true);
        let error = admitted.unwrap_err().to_string();
        assert_eq!(
            error,
            format!("--override is cgwalters's alone; the token is {login:?}'s")
        );
        assert!(lines.is_empty() && forge.slots().is_empty(), "{login}");
    }
}

/// An admit repeated, as after its answer was lost though its write was
/// made, gets the run's lease back and takes no second slot.
#[test]
fn a_retried_admit_takes_one_slot() {
    let config = config(json!({"wip": 2}));
    let forge = forge(BOT, &[]);
    forge.lose.set(true);
    let (lost, _) = take(&forge, &config, 11, false);
    assert_eq!(lost.unwrap_err().to_string(), "connection reset");
    for _ in 0..2 {
        let (again, lines) = take(&forge, &config, 11, false);
        assert_eq!(again.unwrap().unwrap().slot, 0);
        assert_eq!(lines, ["admit: run 11 holds it already"]);
    }
    assert_eq!(forge.slots(), ["agent-board/leases/0"]);
}

/// A lease that expired, whose run completed or has had every job finish
/// (agent-board#376) may be reaped; a run in flight, or one not found,
/// keeps its lease; a commit that is no lease leaves its slot free; and a
/// ref that is no slot is nothing. Admit takes the lowest slot no live
/// lease holds, reaping its lease. Release frees only its own run's leases
/// of its pool.
#[test]
fn ended_leases_are_reaped() {
    let config = config(json!({"wip": 8}));
    let mut forge = forge(BOT, &[]);
    let done = |id| {
        let mut run = run(id, "completed", 7200);
        run.1["conclusion"] = json!("success");
        run
    };
    forge.runs.extend([
        run(1, "in_progress", 60),
        done(2),
        run(3, "in_progress", 7200),
        run(4, "in_progress", 7200),
        run(6, "in_progress", 60),
    ]);
    let job = |status: &str| json!({"status": status, "completed_at": ago(1800)});
    let running = [job("completed"), job("in_progress")];
    forge.jobs.extend([
        (3, json!({"total_count": 1, "jobs": [job("completed")]})),
        (4, json!({"total_count": 2, "jobs": running})),
    ]);
    forge.lease(0, "claude", 1, -1);
    forge.lease(1, "claude", 2, 3600);
    forge.lease(2, "claude", 3, 3600);
    forge.lease(3, "claude", 4, 3600);
    forge.lease(4, "claude", 5, 3600);
    forge.lease(5, "claude", 6, 3600);
    forge.raw("agent-board/leases/6", "not JSON");
    forge.lease(7, "claude", 6, 3600);
    forge.lease(8, "openai", 6, 3600);
    forge.lease(100, "claude", 6, 3600);
    forge.raw("agent-board/leases/claude/0", "{}");
    let read = |forge: &Forge| {
        let all = read(forge, REPO, now()).unwrap();
        (all.into_iter())
            .map(|f| {
                let name = f.name.trim_start_matches("agent-board/leases/").to_owned();
                (name, f.held.map(|h| h.lease.run), f.ended)
            })
            .collect::<Vec<_>>()
    };
    let ended = |why: &str| Some(why.to_owned());
    assert_eq!(
        read(&forge),
        [
            (
                "0".into(),
                Some(1),
                ended("expired 2026-10-23T15:15:09.452609311Z")
            ),
            ("1".into(), Some(2), ended("its run 2 ended success")),
            ("100".into(), None, None),
            (
                "2".into(),
                Some(3),
                ended("its run 3's jobs have all finished")
            ),
            ("3".into(), Some(4), None),
            ("4".into(), Some(5), None),
            ("5".into(), Some(6), None),
            ("6".into(), None, None),
            ("7".into(), Some(6), None),
            ("8".into(), Some(6), None),
            ("claude/0".into(), None, None),
        ]
    );
    let (admitted, lines) = take(&forge, &config, 11, false);
    let reaped = "reap agent-board/leases/0: expired 2026-10-23T15:15:09.452609311Z";
    assert_eq!(lines, [reaped]);
    assert_eq!(admitted.unwrap().unwrap().slot, 0);
    let released = release(&forge, REPO, ("claude", 6), now()).unwrap();
    assert_eq!(
        released,
        [
            "release agent-board/leases/5",
            "release agent-board/leases/7"
        ]
    );
    assert_eq!(forge.message(7), "released by run 6");
    let none = release(&forge, REPO, ("claude", 6), now()).unwrap();
    assert_eq!(none, ["release: run 6 holds no lease of claude"]);
    let runs: Vec<_> = read(&forge).into_iter().map(|(_, run, _)| run).collect();
    let left = [
        Some(11),
        Some(2),
        None,
        Some(3),
        Some(4),
        Some(5),
        None,
        None,
        None,
        Some(6),
        None,
    ];
    assert_eq!(runs, left);
}
