//! Starting an agent job: the one request `dispatch ISSUE --apply`, the poll
//! and `supervise execute` share ([docs/dispatch.md](../../../docs/dispatch.md)).
//! [`GitHub`] and the opt-in native [`Forgejo`] use the caller's workflow.
//! Each caller keeps its own transport and passes it as a
//! closure, so what its fakes record is unchanged.
use crate::poll::{validate_repository, validate_workflow};
use anyhow::{Context, Result, anyhow, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

mod forgejo;
pub use forgejo::{Forgejo, Lease as ForgejoLease, Slot as ForgejoSlot};

/// The job to start: the plan's or the caller's content, never its route.
#[derive(Clone, Copy, Debug)]
pub struct Start<'a> {
    /// Target repository, OWNER/NAME.
    pub repository: &'a str,
    /// Issue or pull request number in `repository`.
    pub item: u64,
    pub kind: &'a str,
    /// The task, sent as the `task` input.
    pub brief: &'a str,
    /// `None` leaves the input out; an empty one is sent empty.
    pub pool: Option<&'a str>,
    /// Not sent: the caller workflow has one runner and no such input.
    pub runner: Option<&'a str>,
    /// A stacked plan's base branch; only then is a `base` input sent.
    pub base: Option<&'a str>,
}

/// Where jobs are dispatched: the caller's workflow and the ref it runs
/// from. These come from the caller's own configuration, never from a plan.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Target {
    /// Caller repository, OWNER/NAME.
    pub caller: String,
    /// Its workflow file that takes a plan's inputs, such as dispatch.yml.
    pub workflow: String,
    pub git_ref: String,
}

/// One opt-in namespace route from trusted deployment configuration. The exact
/// repository list is an admission bound, not a wildcard namespace grant.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Routing {
    pub namespace: String,
    pub repositories: Vec<String>,
    pub target: Target,
    pub runner_config: String,
    #[serde(default = "small_cores")]
    pub cores: u8,
}

pub const ROUTED_CORES: u8 = 4;
pub const ROUTED_REPOSITORIES: usize = 10;
/// The largest listing of active runs in one status that admission reads.
const ROUTE_RUN_PAGE_SIZE: u64 = 100;

/// Every run status but `completed`, in lifecycle order.
const ROUTE_ACTIVE_STATUSES: [&str; 5] =
    ["requested", "pending", "waiting", "queued", "in_progress"];

/// Passes over `ROUTE_ACTIVE_STATUSES`: one more than the backward status
/// changes a run may make during the reads and still be seen.
const ROUTE_PASSES: usize = 2;

fn small_cores() -> u8 {
    ROUTED_CORES
}

impl Routing {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            crate::is_github_login(&self.namespace),
            "invalid route namespace"
        );
        self.target.validate()?;
        ensure!(
            !self.runner_config.is_empty()
                && self.runner_config.len() <= 64
                && self
                    .runner_config
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-'),
            "invalid route runnerConfig"
        );
        ensure!(
            self.cores == ROUTED_CORES,
            "route supports only {ROUTED_CORES} cores"
        );
        ensure!(
            (1..=ROUTED_REPOSITORIES).contains(&self.repositories.len()),
            "route requires 1..={ROUTED_REPOSITORIES} exact repositories"
        );
        let mut seen = std::collections::BTreeSet::new();
        for repo in &self.repositories {
            validate_repository(repo)?;
            ensure!(
                repo.split('/')
                    .next()
                    .is_some_and(|owner| owner.eq_ignore_ascii_case(&self.namespace)),
                "route repository {repo} is outside namespace {}",
                self.namespace
            );
            ensure!(
                seen.insert(repo.to_ascii_lowercase()),
                "duplicate route repository {repo}"
            );
        }
        Ok(())
    }

    /// Select by the actual job target, never the tracking issue. A matched
    /// namespace that cannot be admitted is an error, never a default-route job.
    pub fn select(&self, start: &Start) -> Result<bool> {
        self.validate()?;
        validate_repository(start.repository)?;
        if !start
            .repository
            .split('/')
            .next()
            .is_some_and(|owner| owner.eq_ignore_ascii_case(&self.namespace))
        {
            return Ok(false);
        }
        ensure!(
            self.repositories
                .iter()
                .any(|repo| repo.eq_ignore_ascii_case(start.repository)),
            "route does not admit target {}",
            start.repository
        );
        ensure!(
            ["research", "triage", "implement"].contains(&start.kind),
            "route does not support kind {:?}; dependent caller fix/review support is required",
            start.kind
        );
        ensure!(
            start.base.is_none(),
            "route does not support stacked plans/base input"
        );
        ensure!(
            start.pool.is_none_or(
                |pool| pool.is_empty() || ["openai", "claude", "mistral"].contains(&pool)
            ),
            "route does not support pool {:?}",
            start.pool
        );
        Ok(true)
    }

    /// List each workflow's active runs by status, whatever its history's
    /// length, preserving old tracker visibility. Neither an unfiltered
    /// history nor a filtered `total_count` (capped at 2500) bounds a busy
    /// caller, so each active status is listed in `ROUTE_ACTIVE_STATUSES`
    /// order, twice: the first pass sees every run that only moves forward
    /// in that order between its reads, the second also one that moves back
    /// once (a gate taking it from queued or in_progress to waiting). Refuse
    /// unavailable, malformed or oversized
    /// listings rather than spend elsewhere. The operator must serialize
    /// dispatches; this is admission, not a remote lock.
    pub fn check_idle(
        &self,
        client: &impl crate::poll::Rest,
        legacy: &Target,
        repository: &str,
    ) -> Result<()> {
        self.validate()?;
        legacy.validate()?;
        validate_repository(repository)?;
        for target in [legacy, &self.target] {
            for status in std::iter::repeat_n(ROUTE_ACTIVE_STATUSES, ROUTE_PASSES).flatten() {
                let (total, runs) = route_runs(
                    client,
                    target,
                    &format!("status={status}&per_page={ROUTE_RUN_PAGE_SIZE}"),
                )?;
                ensure!(
                    total <= ROUTE_RUN_PAGE_SIZE && runs.len() as u64 == total,
                    "route has {total} {status} runs on {}/{}, listed {}; dispatch held",
                    target.caller,
                    target.workflow,
                    runs.len()
                );
                for run in runs {
                    // A run listed under a status it has since left is
                    // judged by its current one.
                    if run["status"] == "completed" {
                        continue;
                    }
                    ensure!(
                        run["status"]
                            .as_str()
                            .is_some_and(|s| ROUTE_ACTIVE_STATUSES.contains(&s)),
                        "route run has unknown status; dispatch held"
                    );
                    let title = run["display_title"]
                        .as_str()
                        .context("route run missing display_title")?;
                    let named = title.split_whitespace().nth(1).unwrap_or_default();
                    let (repo, item) = named
                        .rsplit_once('#')
                        .context("route active run title has no target identity; dispatch held")?;
                    validate_repository(repo)
                        .context("route active run title has invalid repository")?;
                    ensure!(
                        item.parse::<u64>().is_ok_and(|item| item > 0),
                        "route active run title has invalid item; dispatch held"
                    );
                    if repo.eq_ignore_ascii_case(repository) {
                        anyhow::bail!(
                            "route held: {repository} has an active run on {}/{}: {}",
                            target.caller,
                            target.workflow,
                            run["html_url"]
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

/// One listing of the workflow's runs: its `total_count` and the runs on it.
fn route_runs(
    client: &impl crate::poll::Rest,
    target: &Target,
    query: &str,
) -> Result<(u64, Vec<Value>)> {
    let path = format!(
        "{}repos/{}/actions/workflows/{}/runs?{query}",
        crate::github::API_ROOT,
        target.caller,
        target.workflow
    );
    let response = client
        .get(&path, None)
        .with_context(|| format!("route unavailable: {}/{}", target.caller, target.workflow))?;
    ensure!(
        response.status == 200,
        "route unavailable: {}/{}: HTTP {}",
        target.caller,
        target.workflow,
        response.status
    );
    let total = response.body["total_count"]
        .as_u64()
        .context("route run listing missing total_count")?;
    let runs = response.body["workflow_runs"]
        .as_array()
        .context("route run listing missing workflow_runs")?;
    Ok((total, runs.clone()))
}

impl Target {
    /// Each part goes into an API path that carries the dispatch token.
    pub fn validate(&self) -> Result<()> {
        validate_repository(&self.caller).context("invalid caller repository")?;
        validate_workflow(&self.workflow)?;
        let git_ref = &self.git_ref;
        ensure!(
            !git_ref.is_empty()
                && !git_ref.starts_with('-')
                && !git_ref.contains("..")
                && git_ref
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)),
            "invalid caller ref"
        );
        Ok(())
    }
}

/// A started run, as the backend names it.
#[derive(Clone, Debug, PartialEq)]
pub struct Handle {
    /// Opaque to callers: GitHub's `workflow_run_id`, in decimal.
    pub id: String,
    /// The run's page, for people.
    pub url: String,
}

/// What a start did.
#[derive(Debug)]
pub enum StartOutcome {
    Started(Handle),
    /// Started, but the backend did not name the run.
    Unnamed,
    /// Refused before anything could run.
    NotStarted(anyhow::Error),
    /// The outcome is unknown: the caller records it and never retries it.
    Ambiguous(anyhow::Error),
}

/// The `workflow_dispatch` backend. It does not validate `target`; callers
/// keep their own checks, [`Target::validate`] among them.
pub struct GitHub<'a> {
    pub target: &'a Target,
    /// Ask for `return_run_details`, which names the run, and on a 422 ask
    /// again without it, as GitHub instances without the field refuse it.
    pub run_details: bool,
}

impl GitHub<'_> {
    /// The request's path below the API root.
    pub fn path(&self) -> String {
        let Target {
            caller, workflow, ..
        } = self.target;
        format!("repos/{caller}/actions/workflows/{workflow}/dispatches")
    }

    /// The request body, without `return_run_details`. The inputs are the
    /// caller workflow's, all strings.
    pub fn body(&self, start: &Start) -> Value {
        let mut body = json!({
            "ref": self.target.git_ref,
            "inputs": {
                "task": start.brief,
                "repo": start.repository,
                "item": start.item.to_string(),
                "kind": start.kind,
            },
        });
        if let Some(pool) = start.pool {
            body["inputs"]["pool"] = pool.into();
        }
        if let Some(base) = start.base {
            body["inputs"]["base"] = base.into();
        }
        body
    }

    pub fn routed_body(&self, start: &Start, route: Option<&Routing>) -> Result<Value> {
        let mut body = self.body(start);
        if let Some(route) = route {
            ensure!(route.select(start)?, "target does not match route");
            ensure!(
                self.target.caller == route.target.caller
                    && self.target.workflow == route.target.workflow
                    && self.target.git_ref == route.target.git_ref,
                "selected route target mismatch"
            );
            body["inputs"]["cores"] = route.cores.to_string().into();
            body["inputs"]["config"] = route.runner_config.clone().into();
            if start.pool.is_none_or(str::is_empty) {
                body["inputs"]["pool"] = "openai".into();
            }
        }
        Ok(body)
    }

    /// Send the request with `send`, which takes the path and body and
    /// answers the status and body; its `Err` is a lost answer. `admit` is
    /// asked before each request, and its `Err` starts nothing.
    pub fn start(
        &self,
        start: &Start,
        admit: impl FnMut() -> Result<()>,
        send: impl FnMut(&str, &Value) -> Result<(u16, Value)>,
    ) -> StartOutcome {
        self.start_routed(start, None, admit, send)
    }

    pub fn start_routed(
        &self,
        start: &Start,
        routing: Option<&Routing>,
        mut admit: impl FnMut() -> Result<()>,
        mut send: impl FnMut(&str, &Value) -> Result<(u16, Value)>,
    ) -> StartOutcome {
        let path = self.path();
        let mut body = match self.routed_body(start, routing) {
            Ok(body) => body,
            Err(error) => return StartOutcome::NotStarted(error),
        };
        if self.run_details {
            body["return_run_details"] = true.into();
        }
        loop {
            if let Err(error) = admit() {
                return StartOutcome::NotStarted(error);
            }
            let (status, answer) = match send(&path, &body) {
                Ok(sent) => sent,
                Err(error) => return StartOutcome::Ambiguous(error),
            };
            if (200..300).contains(&status) {
                return match answer["workflow_run_id"].as_u64() {
                    Some(id) => StartOutcome::Started(Handle {
                        id: id.to_string(),
                        url: format!(
                            "https://github.com/{}/actions/runs/{id}",
                            self.target.caller
                        ),
                    }),
                    None => StartOutcome::Unnamed,
                };
            }
            if status == 422
                && body
                    .as_object_mut()
                    .and_then(|b| b.remove("return_run_details"))
                    .is_some()
            {
                continue;
            }
            let error = anyhow!(
                "POST {path}: GitHub HTTP status {status}: {}",
                answer["message"].as_str().unwrap_or("message unavailable")
            );
            // A refusal started nothing; a server error may have.
            return match (400..500).contains(&status) {
                true => StartOutcome::NotStarted(error),
                false => StartOutcome::Ambiguous(error),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    fn route() -> Routing {
        serde_json::from_value(json!({
            "namespace": "bootc-dev",
            "repositories": ["bootc-dev/bootc", "bootc-dev/cgwalters-devspace-sandbox"],
            "target": {"caller": "bootc-dev/cgwalters-devspace-sandbox", "workflow": "agent.yml", "gitRef": "main"},
            "runnerConfig": "rhel10"
        })).unwrap()
    }

    #[test]
    fn trusted_routing_table() {
        let route = route();
        for (repository, kind, pool, base, expected) in [
            ("bootc-dev/bootc", "implement", "openai", None, Ok(true)),
            ("BOOTC-DEV/BOOTC", "research", "", None, Ok(true)),
            (
                "bootc-dev/cgwalters-devspace-sandbox",
                "triage",
                "claude",
                None,
                Ok(true),
            ),
            ("cgwalters-forge/bootc", "fix", "tier-1", None, Ok(false)),
            ("other/bootc", "review", "", None, Ok(false)),
            (
                "bootc-dev/other",
                "implement",
                "openai",
                None,
                Err("does not admit target"),
            ),
            (
                "bootc-dev/bootc",
                "fix",
                "openai",
                None,
                Err("does not support kind"),
            ),
            (
                "bootc-dev/bootc",
                "review",
                "openai",
                None,
                Err("does not support kind"),
            ),
            (
                "bootc-dev/bootc",
                "research",
                "tier-1",
                None,
                Err("does not support pool"),
            ),
            (
                "bootc-dev/bootc",
                "implement",
                "openai",
                Some("dispatch/lower"),
                Err("stacked"),
            ),
            (
                "bootc-dev/bootc?x=1",
                "research",
                "",
                None,
                Err("invalid repository"),
            ),
        ] {
            let start = Start {
                repository,
                item: 7,
                kind,
                brief: "Look.",
                pool: Some(pool),
                runner: None,
                base,
            };
            match (route.select(&start), expected) {
                (Ok(got), Ok(want)) => {
                    assert_eq!(got, want, "{repository} {kind}");
                    if got {
                        let github = GitHub {
                            target: &route.target,
                            run_details: true,
                        };
                        let body = github.routed_body(&start, Some(&route)).unwrap();
                        assert_eq!(body["inputs"]["cores"], "4");
                        assert_eq!(body["inputs"]["config"], "rhel10");
                        assert_eq!(
                            body["inputs"]["pool"],
                            if pool.is_empty() { "openai" } else { pool }
                        );
                    }
                }
                (Err(error), Err(want)) => {
                    assert!(format!("{error:#}").contains(want), "{error:#}")
                }
                (got, want) => panic!("{repository} {kind}: {got:?} != {want:?}"),
            }
        }
        for cores in [0, 16, 64, 255] {
            let mut route = self::route();
            route.cores = cores;
            assert!(
                route
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("only 4 cores")
            );
        }
        for repositories in [
            vec![],
            vec!["other/bootc".into()],
            vec!["bootc-dev/bootc".into(); 11],
            vec!["bootc-dev/bootc".into(), "BOOTC-DEV/BOOTC".into()],
        ] {
            let mut route = self::route();
            route.repositories = repositories;
            assert!(route.validate().is_err());
        }
    }

    /// A fake caller's run: caller repository, id, status and display title.
    type Run = (&'static str, u64, &'static str, &'static str);

    const TRACKER: &str = "cgwalters-forge/tracker";

    const SANDBOX: &str = "bootc-dev/cgwalters-devspace-sandbox";

    /// Both callers' runs. Before the numbered read, each of `moves` sets a
    /// run's status. `index` lists one run under a stale status, as a lagging
    /// run index would.
    struct Runs {
        runs: std::cell::RefCell<Vec<Run>>,
        http: u16,
        moves: Vec<(usize, u64, &'static str)>,
        index: Option<(u64, &'static str)>,
        reads: std::cell::Cell<usize>,
    }

    impl crate::poll::Rest for Runs {
        fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<crate::poll::RestResponse> {
            let read = self.reads.get();
            self.reads.set(read + 1);
            let mut runs = self.runs.borrow_mut();
            for &(_, id, status) in self.moves.iter().filter(|m| m.0 == read) {
                runs.iter_mut().find(|run| run.1 == id).unwrap().2 = status;
            }
            let caller = [TRACKER, SANDBOX]
                .into_iter()
                .find(|caller| url.contains(&format!("/repos/{caller}/actions/")))
                .unwrap();
            let (filter, per_page) = url
                .split_once("?status=")
                .and_then(|(_, query)| query.split_once("&per_page="))
                .unwrap();
            let per_page: usize = per_page.parse().unwrap();
            let listed: Vec<Value> = runs
                .iter()
                .filter(|run| run.0 == caller)
                .filter(|run| self.index.filter(|i| i.0 == run.1).map_or(run.2, |i| i.1) == filter)
                .map(|run| json!({"id": run.1, "status": run.2, "display_title": run.3}))
                .collect();
            Ok(crate::poll::RestResponse {
                status: self.http,
                etag: None,
                last_modified: None,
                link: None,
                body: json!({
                    "total_count": listed.len(),
                    "workflow_runs": listed[..listed.len().min(per_page)],
                }),
            })
        }

        fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
            panic!("routing check must not mutate")
        }
    }

    /// Reads per workflow: each active status, twice.
    const INVENTORY_READS: usize = ROUTE_PASSES * ROUTE_ACTIVE_STATUSES.len();

    #[test]
    fn routing_inventory_table() {
        let legacy = Target {
            caller: TRACKER.into(),
            workflow: "dispatch.yml".into(),
            git_ref: "main".into(),
        };
        let other = "fix cgwalters-forge/agent-board#381 (claude)";
        let bootc = "implement bootc-dev/bootc#2531 (openai, 4c)";
        let crowded: Vec<Run> = (1..=101)
            .map(|id| (TRACKER, id, "in_progress", other))
            .collect();
        // The tracker's reads are requested, pending, waiting (2), queued (3)
        // and in_progress (4), then again from 5: waiting is 7, queued 8 and
        // in_progress 9. The sandbox's follow from 10.
        for (name, runs, http, moves, index, expected) in [
            ("idle", vec![], 200, vec![], None, Ok(())),
            (
                "other active work, completed work",
                vec![
                    (TRACKER, 1, "in_progress", other),
                    (SANDBOX, 2, "completed", bootc),
                ],
                200,
                vec![],
                None,
                Ok(()),
            ),
            (
                "old tracker run",
                vec![(TRACKER, 1, "in_progress", bootc)],
                200,
                vec![],
                None,
                Err("route held"),
            ),
            (
                "sandbox queued",
                vec![(SANDBOX, 1, "queued", bootc)],
                200,
                vec![],
                None,
                Err("route held"),
            ),
            (
                "sandbox waiting",
                vec![(SANDBOX, 1, "waiting", bootc)],
                200,
                vec![],
                None,
                Err("route held"),
            ),
            (
                "case-insensitive target",
                vec![(
                    SANDBOX,
                    1,
                    "requested",
                    "research BOOTC-DEV/bootc#7 (openai, 4c)",
                )],
                200,
                vec![],
                None,
                Err("route held"),
            ),
            (
                "unavailable",
                vec![],
                404,
                vec![],
                None,
                Err("route unavailable"),
            ),
            (
                "crowded status",
                crowded,
                200,
                vec![],
                None,
                Err("101 in_progress runs"),
            ),
            (
                "started between reads",
                vec![(TRACKER, 1, "queued", bootc)],
                200,
                vec![(4, 1, "in_progress")],
                None,
                Err("route held"),
            ),
            (
                "moved forward before each read",
                vec![(SANDBOX, 1, "requested", bootc)],
                200,
                vec![
                    (10, 1, "pending"),
                    (11, 1, "waiting"),
                    (12, 1, "queued"),
                    (13, 1, "in_progress"),
                ],
                None,
                Err("route held"),
            ),
            (
                "gated between reads",
                vec![(TRACKER, 1, "in_progress", bootc)],
                200,
                vec![(3, 1, "waiting")],
                None,
                Err("route held"),
            ),
            // The documented limit: two backward changes during the reads.
            (
                "gated twice between reads",
                vec![(TRACKER, 1, "in_progress", bootc)],
                200,
                vec![(3, 1, "waiting"), (7, 1, "in_progress"), (9, 1, "waiting")],
                None,
                Ok(()),
            ),
            (
                "completed before its status is read",
                vec![(TRACKER, 1, "in_progress", bootc)],
                200,
                vec![(4, 1, "completed")],
                None,
                Ok(()),
            ),
            (
                "lagging index, completed",
                vec![(TRACKER, 1, "completed", bootc)],
                200,
                vec![],
                Some((1, "in_progress")),
                Ok(()),
            ),
            (
                "lagging index, started",
                vec![(TRACKER, 1, "in_progress", bootc)],
                200,
                vec![],
                Some((1, "queued")),
                Err("route held"),
            ),
            (
                "unknown status",
                vec![(TRACKER, 1, "action_required", other)],
                200,
                vec![],
                Some((1, "queued")),
                Err("unknown status"),
            ),
            (
                "empty title",
                vec![(TRACKER, 1, "queued", "")],
                200,
                vec![],
                None,
                Err("no target identity"),
            ),
            (
                "bare title",
                vec![(TRACKER, 1, "queued", "agent")],
                200,
                vec![],
                None,
                Err("no target identity"),
            ),
            (
                "bad item",
                vec![(TRACKER, 1, "queued", "implement bootc-dev/bootc#oops")],
                200,
                vec![],
                None,
                Err("invalid item"),
            ),
            (
                "bad repository",
                vec![(TRACKER, 1, "queued", "implement not-a-repo#1")],
                200,
                vec![],
                None,
                Err("invalid repository"),
            ),
        ] {
            let runs = Runs {
                runs: runs.into(),
                http,
                moves,
                index,
                reads: Default::default(),
            };
            match (
                route().check_idle(&runs, &legacy, "bootc-dev/bootc"),
                expected,
            ) {
                (Ok(()), Ok(())) => assert_eq!(runs.reads.get(), 2 * INVENTORY_READS, "{name}"),
                (Err(error), Err(want)) => {
                    assert!(format!("{error:#}").contains(want), "{name}: {error:#}")
                }
                (got, want) => panic!("{name}: {got:?} != {want:?}"),
            }
        }
    }

    #[test]
    fn github_start_table() {
        let target = Target {
            caller: "example/tracker".into(),
            workflow: "dispatch.yml".into(),
            git_ref: "main".into(),
        };
        let start = Start {
            repository: "example/app",
            item: 7,
            kind: "fix",
            brief: "Fix it.",
            pool: None,
            runner: Some("hosted"),
            base: None,
        };
        let path = "repos/example/tracker/actions/workflows/dispatch.yml/dispatches";
        let plain = json!({"ref": "main", "inputs": {"task": "Fix it.", "repo": "example/app", "item": "7", "kind": "fix"}});
        let mut detailed = plain.clone();
        detailed["return_run_details"] = true.into();
        // Answers in turn, `None` a lost one.
        type Answers = Vec<Option<(u16, Value)>>;
        // Whether run details are asked for; the answers; the bodies sent;
        // what came of it.
        let cases: &[(&str, bool, Answers, Vec<&Value>, &str)] = &[
            (
                "handle",
                true,
                vec![Some((200, json!({"workflow_run_id": 42})))],
                vec![&detailed],
                "started 42 https://github.com/example/tracker/actions/runs/42",
            ),
            (
                "422, then without run details",
                true,
                vec![Some((422, json!({}))), Some((204, Value::Null))],
                vec![&detailed, &plain],
                "unnamed",
            ),
            (
                "422 twice",
                true,
                vec![
                    Some((422, json!({"message": "no"}))),
                    Some((422, json!({}))),
                ],
                vec![&detailed, &plain],
                &format!("not started: POST {path}: GitHub HTTP status 422: message unavailable"),
            ),
            (
                "422 without asking for run details",
                false,
                vec![Some((422, json!({"message": "no"})))],
                vec![&plain],
                &format!("not started: POST {path}: GitHub HTTP status 422: no"),
            ),
            (
                "refused",
                true,
                vec![Some((404, json!({"message": "Not Found"})))],
                vec![&detailed],
                &format!("not started: POST {path}: GitHub HTTP status 404: Not Found"),
            ),
            (
                "server error",
                true,
                vec![Some((500, json!({"message": "boom"})))],
                vec![&detailed],
                &format!("ambiguous: POST {path}: GitHub HTTP status 500: boom"),
            ),
            (
                "lost answer",
                true,
                vec![None],
                vec![&detailed],
                "ambiguous: connection reset",
            ),
            (
                "no handle without run details",
                false,
                vec![Some((204, Value::Null))],
                vec![&plain],
                "unnamed",
            ),
        ];
        for (name, run_details, answers, sent, expected) in cases {
            let github = GitHub {
                target: &target,
                run_details: *run_details,
            };
            let mut answers = answers.iter();
            let mut bodies = vec![];
            let outcome = github.start(
                &start,
                || Ok(()),
                |p, body| {
                    assert_eq!(p, path, "{name}");
                    bodies.push(body.clone());
                    match answers.next().unwrap() {
                        Some(answer) => Ok(answer.clone()),
                        None => bail!("connection reset"),
                    }
                },
            );
            let got = match outcome {
                StartOutcome::Started(h) => format!("started {} {}", h.id, h.url),
                StartOutcome::Unnamed => "unnamed".into(),
                StartOutcome::NotStarted(e) => format!("not started: {e:#}"),
                StartOutcome::Ambiguous(e) => format!("ambiguous: {e:#}"),
            };
            assert_eq!(got, *expected, "{name}");
            assert_eq!(bodies.iter().collect::<Vec<_>>(), *sent, "{name}");
        }
    }

    #[test]
    fn github_start_admits_each_request() {
        let target = Target {
            caller: "example/tracker".into(),
            workflow: "dispatch.yml".into(),
            git_ref: "main".into(),
        };
        let start = Start {
            repository: "example/app",
            item: 7,
            kind: "triage",
            brief: "Look.",
            pool: Some("small"),
            runner: None,
            base: Some("stack/lower"),
        };
        let github = GitHub {
            target: &target,
            run_details: true,
        };
        assert_eq!(
            github.body(&start)["inputs"],
            json!({"task": "Look.", "repo": "example/app", "item": "7", "kind": "triage", "pool": "small", "base": "stack/lower"})
        );
        // One request is admitted: the retry after a 422 is not, and nothing
        // more is sent.
        let mut budget = 1;
        let mut sent = 0;
        let outcome = github.start(
            &start,
            || {
                ensure!(budget > 0, "budget spent");
                budget -= 1;
                Ok(())
            },
            |_, _| {
                sent += 1;
                Ok((422, json!({})))
            },
        );
        assert!(
            matches!(&outcome, StartOutcome::NotStarted(e) if e.to_string() == "budget spent"),
            "{outcome:?}"
        );
        assert_eq!(sent, 1);
    }
}
