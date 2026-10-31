//! Forgejo pass: identity, broker pacing, stage 3's epics ([`epics`]) and
//! stage 4 preview/apply. No GitHub fallback, publication, PR follow-up or
//! Status writes.
use super::*;
use crate::dispatch::{Forgejo as Actions, ForgejoLease, ForgejoSlot, Start, StartOutcome};
use crate::forgejo::{
    Config as ForgejoConfig, Transport as ForgejoTransport, listing, pages, pages_to_end,
};
use crate::{BoardOperation, BoardWrite, Forge};

mod epics;
mod record;
pub use record::{Recorder, Replay};

const CONTRACT: &str = include_str!("../../../../fixtures/forgejo/pass-contract.json");
/// Pages of each listing stage 4 reads, 50 items each: a work repository's
/// open issues, its open pull requests and an issue's dependencies. More
/// open issues is an incomplete board, which starts nothing; more pull
/// requests or dependencies hold the issues they could belong to.
const LIST_PAGES: usize = 4;
/// A label board's Priority options in rank order, as `priority/` labels,
/// indexed by [`board_core::Priority`].
const PRIORITIES: [&str; 4] = ["P0", "P1", "P2", "P3"];
/// The caller's inputs as far as listing its runs needs them: none is sent.
const LISTING_INPUTS: [&str; 4] = ["task", "repo", "item", "kind"];
/// The ref the runs are listed under where `dispatchRef` is unset. Listing
/// does not filter by ref, so it only has to be valid.
const LISTING_REF: &str = "main";
/// Preprovisioned private contents branch; never created by a pass.
const LEASE_BRANCH: &str = "agent-board-leases";

/// Reject explicit settings, even false, empty or malformed ones, before typed
/// decoding can discard their presence or hide the capability diagnostic.
pub(super) fn check_options(value: &Value) -> Result<()> {
    let contract: Value = serde_json::from_str(CONTRACT).context("Forgejo pass contract")?;
    for (option, entry) in contract["options"]
        .as_object()
        .context("contract options")?
    {
        let pointer = format!("/{}", option.replace('.', "/"));
        if entry["verdict"] == "refused" && value.pointer(&pointer).is_some() {
            bail!(
                "unsupported_capability: {option}: {}",
                entry["reason"].as_str().unwrap_or_default()
            );
        }
    }
    for (option, follow_up) in [
        ("holds", "scheduler and hold publication #513"),
        ("usage.repository", "usage publication in scheduler #513"),
        ("usage.issue", "usage publication in scheduler #513"),
        ("usage.probe", "run inventory in scheduler #513"),
        ("sweep", "a follow-up to scheduler #513 for sweep ownership"),
        (
            "revisit",
            "a follow-up to scheduler #513 for revisit ownership",
        ),
    ] {
        let pointer = format!("/{}", option.replace('.', "/"));
        if value.pointer(&pointer).is_some() {
            bail!(
                "unsupported_capability: {option}: not built on Forgejo yet; needs {follow_up}; leave this option out for preview"
            );
        }
    }
    Ok(())
}

impl<C: ForgejoTransport> ForgejoTransport for Budget<'_, C> {
    fn get(&self, url: &url::Url) -> Result<crate::forgejo::Response> {
        self.spend(false)?;
        self.observe(self.inner.get(url))
    }

    fn send(&self, method: &str, url: &url::Url, body: &Value) -> Result<(u16, Value)> {
        self.spend(true)?;
        self.observe(self.inner.send(method, url, body))
    }

    fn write(&self, method: &str, url: &url::Url, body: &Value) -> Result<Value> {
        self.spend(true)?;
        self.observe(self.inner.write(method, url, body))
    }
}

/// Every attempted mutation counts, including cancellation and lost replies.
struct Capped<'a, C> {
    inner: &'a C,
    caps: &'a Caps,
}

impl<C: ForgejoTransport> Capped<'_, C> {
    fn charge(&self, url: &url::Url) -> Result<()> {
        let kind = if url.path().contains("/actions/")
            && (url.path().ends_with("/dispatches") || url.path().ends_with("/rerun"))
        {
            Kind::Dispatch
        } else {
            Kind::Write
        };
        ensure!(self.caps.check(kind).is_none(), "held by {}", kind.flag());
        self.caps.count(kind);
        Ok(())
    }
}

impl<C: ForgejoTransport> ForgejoTransport for Capped<'_, C> {
    fn get(&self, url: &url::Url) -> Result<crate::forgejo::Response> {
        self.inner.get(url)
    }

    fn send(&self, method: &str, url: &url::Url, body: &Value) -> Result<(u16, Value)> {
        self.charge(url)?;
        self.inner.send(method, url, body)
    }

    fn write(&self, method: &str, url: &url::Url, body: &Value) -> Result<Value> {
        self.charge(url)?;
        self.inner.write(method, url, body)
    }
}

/// The bot's and the operator's user ids, which every trust decision compares.
#[derive(Clone, Copy)]
struct Ids {
    bot: u64,
    operator: u64,
}

fn identity(
    client: &impl ForgejoTransport,
    board: &ForgejoConfig,
    config: &Config,
) -> Result<(String, Ids)> {
    let api = board.api_root()?;
    let bot = client.get(&api.join("user")?)?.body;
    ensure!(
        text(&bot, "/login").eq_ignore_ascii_case(&config.bot),
        "the Forgejo token's login is not bot {:?}",
        config.bot
    );
    let bot_id = bot["id"].as_u64().context("bot has no user id")?;
    ensure!(
        board.recorders.contains(&bot_id),
        "bot id {bot_id} is not in board.recorders"
    );
    let mut operator_url = api.join("users/")?;
    operator_url
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid API root"))?
        .pop_if_empty()
        .push(&config.operator);
    let operator = client.get(&operator_url)?.body;
    ensure!(
        text(&operator, "/login").eq_ignore_ascii_case(&config.operator),
        "Forgejo operator lookup returned a different login"
    );
    let operator_id = operator["id"].as_u64().context("operator has no user id")?;
    ensure!(
        board.recorders.contains(&operator_id),
        "operator id {operator_id} is not in board.recorders; verify the operator's numeric id, not a reused login"
    );
    let line = format!(
        "identity: bot {} (id {bot_id}), operator {} (id {operator_id}); both in recorders",
        config.bot, config.operator
    );
    let ids = Ids {
        bot: bot_id,
        operator: operator_id,
    };
    Ok((line, ids))
}

/// Run only identity and stage 1 under the shared request and time bounds.
pub fn run(
    client: &impl ForgejoTransport,
    broker: &impl Broker,
    config: &Config,
    (mode, limits): (Mode<'_>, Limits),
    now: OffsetDateTime,
) -> Result<Report> {
    let bounds = Bounds::new(
        PASS_REQUEST_LIMIT,
        Instant::now() + Duration::from_secs(PASS_SECONDS_LIMIT),
    );
    run_within(client, broker, config, (mode, limits), now, &bounds)
}

fn run_within(
    client: &impl ForgejoTransport,
    broker: &impl Broker,
    config: &Config,
    (mode, limits): (Mode<'_>, Limits),
    now: OffsetDateTime,
    bounds: &Bounds,
) -> Result<Report> {
    config.check()?;
    let board = config
        .forgejo_board()
        .context("Forgejo pass needs a Forgejo board")?;
    let caps = Caps::new(limits);
    let capped = Capped {
        inner: client,
        caps: &caps,
    };
    let client = Budget {
        inner: &capped,
        bounds,
    };
    let mut report = Report {
        summary: caps.summary(),
        ..Report::default()
    };
    report.tally.0.limit = config.wip;
    report.tally.1.mode = match mode {
        Mode::Preview => "preview",
        Mode::Local => "local",
        Mode::Actions(_) => "actions",
    };
    let ids = match identity(&client, board, config) {
        Ok((line, ids)) => {
            report.lines.push(line);
            ids
        }
        Err(error) => {
            report.failures += 1;
            report.lines.push(format!("identity error: {error:#}"));
            report.spent = bounds.spent();
            return Ok(report);
        }
    };
    // Broker calls share the same bound and clock, but not the forge transport
    // or its token. Nothing can fall back to a forge snapshot here.
    let broker = Budget {
        inner: broker,
        bounds,
    };
    bounds.enter(Stage::Usage);
    let snapshot = refresh(&broker, config, now, &mut report)?;
    report
        .lines
        .push("stage 2 follow pull requests: not built on Forgejo yet".into());
    bounds.enter(Stage::Epics);
    // Starts may name the board's repositories and, for an epic's planning
    // run, `epics.repository`, both from this trusted file.
    let mut starts = board.clone();
    if let Some(epics) = &config.epics
        && !(starts.repositories.iter()).any(|r| r.eq_ignore_ascii_case(&epics.repository))
    {
        starts.repositories.push(epics.repository.clone());
    }
    let mut schedule = Schedule {
        client: &client,
        board,
        starts: &starts,
        config,
        ids,
        now,
        caps: &caps,
        report: &mut report,
        pulls: BTreeMap::new(),
        mode,
        parents: BTreeMap::new(),
        planning: BTreeSet::new(),
        epic_started: 0,
    };
    schedule.epics(snapshot.as_ref());
    bounds.enter(Stage::Schedule);
    if let Err(error) = schedule.run(snapshot.as_ref()) {
        report.failures += 1;
        report.lines.push(format!("schedule error: {error:#}"));
    }
    for (stage, name) in [(5, "status"), (6, "cost")] {
        report
            .lines
            .push(format!("stage {stage} {name}: not built on Forgejo yet"));
    }
    report.summary = caps.summary();
    report.spent = bounds.spent();
    Ok(report)
}

/// The instance an issue of `board` names as its host: its URL without the
/// trailing slash, so that its [`key`] is the issue's URL less `/issues/`
/// and two instances' `OWNER/REPO#N` never share one.
fn instance(board: &ForgejoConfig) -> String {
    board.base().as_str().trim_end_matches('/').to_owned()
}

/// `repos/{repository}/{path}` under the API root of `board`, with `query`.
fn api(
    board: &ForgejoConfig,
    repository: &str,
    path: &str,
    query: &[(&str, &str)],
) -> Result<url::Url> {
    let mut url = board
        .api_root()?
        .join(&format!("repos/{repository}/{path}"))?;
    url.query_pairs_mut().extend_pairs(query);
    Ok(url)
}

/// Stage 4 on Forgejo, as a preview: the GitHub pass's candidates, ranks,
/// holds, WIP and pool pacing ([`screen`], [`rank`], [`hold`], [`choose`]),
/// over the open issues of the work repositories, their dependencies and
/// matching pull requests, and the caller's runs in flight. It reads no plan
/// and so says which issue `would start`, not which run.
struct Schedule<'a, C> {
    client: &'a Budget<'a, C>,
    board: &'a ForgejoConfig,
    /// The instance as the dispatch adapter takes it: the board's
    /// repositories and `epics.repository`, whose epics' planning runs start.
    starts: &'a ForgejoConfig,
    config: &'a Config,
    ids: Ids,
    now: OffsetDateTime,
    caps: &'a Caps,
    report: &'a mut Report,
    /// Each work repository's open pull requests, read once, and whether
    /// they were all read; or why they were not.
    pulls: BTreeMap<String, std::result::Result<(Vec<Value>, bool), String>>,
    mode: Mode<'a>,
    /// Stage 3's: each live epic's open children, where both relationship
    /// comments agree, by [`key`].
    parents: BTreeMap<String, Parented>,
    /// The live epics of `epics.repository` by [`key`], whose planning
    /// runs stage 3 starts through stage 4's claim and start path.
    planning: BTreeSet<String>,
    /// The planning runs stage 3 started or would start, whose slots stage
    /// 4 does not offer again.
    epic_started: usize,
}

/// A live epic's child, and the epic's Priority, which ranks it.
#[derive(Clone)]
struct Parented {
    parent: Parent,
    priority: Option<String>,
}

/// A board issue read, and why its Priority could not be, where it could not.
type Read = (Facts, Option<String>);

struct Accepted {
    id: u64,
    plan: plan::JobPlan,
}

impl Accepted {
    fn start(&self) -> Start<'_> {
        Start {
            repository: &self.plan.repo,
            item: self.plan.item,
            kind: &self.plan.kind,
            brief: &self.plan.brief,
            pool: Some(&self.plan.pool),
            runner: Some(&self.plan.runner),
            base: None,
        }
    }
}

impl<C: ForgejoTransport> Schedule<'_, C> {
    fn forge(&self) -> crate::forgejo::Forgejo<&Budget<'_, C>> {
        crate::forgejo::Forgejo {
            transport: self.client,
            config: self.board.clone(),
        }
    }

    /// The writer of stage 3, whose epics and children need not be board
    /// members: the work repositories and `epics.repository`, open issues
    /// only. Its plans bound it further ([`crate::epic::Bounds`]).
    fn epic_forge(&self) -> crate::forgejo::Forgejo<&Budget<'_, C>> {
        let mut config = self.board.clone();
        config.membership = crate::forgejo::Membership::Open;
        config.repositories = Vec::new();
        let epics = self.config.epics.iter().map(|e| &e.repository);
        for repository in self.config.work_repositories.iter().chain(epics) {
            if !(config.repositories.iter()).any(|r| r.eq_ignore_ascii_case(repository)) {
                config.repositories.push(repository.clone());
            }
        }
        crate::forgejo::Forgejo {
            transport: self.client,
            config,
        }
    }

    /// The writer of `issue`: an epic's planning runs are stage 3's.
    fn forge_for(&self, issue: &ItemRef) -> crate::forgejo::Forgejo<&Budget<'_, C>> {
        match self.planning.contains(&key(issue)) {
            true => self.epic_forge(),
            false => self.forge(),
        }
    }

    fn target(&self) -> Target {
        self.config
            .target(self.config.dispatch_ref.as_deref().unwrap_or(LISTING_REF))
    }

    fn actions<'a>(&'a self, target: &'a Target) -> Result<Actions<'a, Budget<'a, C>>> {
        Actions::new(
            self.client,
            self.starts,
            target,
            ["task", "repo", "item", "kind", "pool"]
                .map(String::from)
                .into(),
            self.ids.bot,
        )
    }

    /// The concrete race is two pass workflows both observing a free WIP slot.
    /// Use this instance's bounded active inventory, never GitHub's API.
    fn serialized(&self) -> Result<()> {
        let Mode::Actions(own) = self.mode else {
            return Ok(());
        };
        crate::forgejo::validate_repository(&own.repository)?;
        ensure!(own.id > 0, "invalid pass run id");
        let run = self
            .client
            .get(&api(
                self.board,
                &own.repository,
                &format!("actions/runs/{}", own.id),
                &[],
            )?)?
            .body;
        ensure!(
            run["id"] == own.id
                && run["repository"]["full_name"]
                    .as_str()
                    .is_some_and(|r| r.eq_ignore_ascii_case(&own.repository)),
            "pass run identity mismatch"
        );
        let workflow = run["workflow_id"]
            .as_str()
            .context("pass run has no workflow")?;
        validate_workflow(workflow)?;
        let target = Target {
            caller: own.repository.clone(),
            workflow: workflow.into(),
            git_ref: LISTING_REF.into(),
        };
        let actions = self.actions(&target)?;
        ensure!(
            !actions
                .active_runs()?
                .iter()
                .any(|r| r["id"] != own.id && r["status"] == "running"),
            "another pass is in progress; new work waits"
        );
        Ok(())
    }

    fn slots(&self, actions: &Actions<'_, Budget<'_, C>>) -> Result<Vec<(u64, ForgejoSlot)>> {
        (0..self.config.wip)
            .map(|n| Ok((n, actions.slot(LEASE_BRANCH, n)?)))
            .collect()
    }

    fn reap(&mut self) -> Result<()> {
        let target = self.target();
        let actions = self.actions(&target)?;
        for (_, slot) in self.slots(&actions)? {
            let Some(lease) = slot.lease() else { continue };
            let expired = OffsetDateTime::parse(&lease.expires_at, &Rfc3339)? <= self.now;
            let ended = lease.run > 0
                && actions.status(lease.run).is_ok_and(|r| {
                    matches!(
                        r["status"].as_str(),
                        Some("success" | "failure" | "cancelled" | "skipped")
                    )
                });
            if expired || ended {
                ensure!(
                    actions.release(&slot, &lease.claim, lease.run, self.now)?,
                    "lease release lost race; held"
                );
            }
        }
        Ok(())
    }

    fn accepted(&self, issue: &ItemRef) -> Result<Accepted> {
        let comments = self
            .forge_for(issue)
            .plan_thread(&self.write_identity(issue))?;
        let mut newest = None;
        for comment in comments {
            if !comment["user"]["id"]
                .as_u64()
                .is_some_and(|id| self.board.recorders.contains(&id))
            {
                continue;
            }
            let body = text(&comment, "/body");
            let parsed = plan::fenced_json(body, PLAN_FENCE);
            if matches!(parsed, Ok(None)) {
                continue;
            }
            let id = comment["id"].as_u64().context("plan has no id")?;
            if newest.as_ref().is_none_or(|(n, _)| id > *n) {
                newest = Some((id, parsed));
            }
        }
        let (id, parsed) = newest.context("no accepted recorder plan; a person must post one")?;
        let value = parsed?.context("plan missing")?;
        let plan = plan::JobPlan::from_json_for(&value, crate::forgejo::Naming::Forgejo)?;
        ensure!(
            plan.repo.eq_ignore_ascii_case(&issue.repository) && plan.item == issue.number,
            "plan targets another issue; held"
        );
        ensure!(
            plan.stacks_on.is_none(),
            "stacked plans are not wired on Forgejo; held"
        );
        Ok(Accepted { id, plan })
    }

    // BoardWrite's normalized snapshot identities use the hostname, whereas
    // scheduling keys retain the whole instance (port and subpath included).
    fn write_identity(&self, issue: &ItemRef) -> ItemRef {
        ItemRef {
            host: self.board.url.host_str().unwrap_or_default().into(),
            ..issue.clone()
        }
    }

    fn write(&self, facts: &Facts, operation: BoardOperation) -> Result<()> {
        let forge = self.forge_for(&facts.issue);
        self.write_on(&forge, &facts.issue, &facts.labels, operation)
    }

    /// One bounded write by `forge` to the open `issue`, observed with
    /// `labels`, which it reads back ([`Forge::board_write`]).
    fn write_on(
        &self,
        forge: &crate::forgejo::Forgejo<&Budget<'_, C>>,
        issue: &ItemRef,
        labels: &[String],
        operation: BoardOperation,
    ) -> Result<()> {
        let observed = board_core::Item {
            identity: self.write_identity(issue),
            project_item: None,
            assignment: None,
            title: String::new(),
            state: "open".into(),
            state_reason: None,
            status: None,
            priority: None,
            turn: None,
            fields_complete: false,
            field_errors: vec![],
            evidence: vec![],
            content_kind: "issue".into(),
            labels: labels.to_vec(),
            assignees: vec![],
            author: None,
            timestamps: BTreeMap::new(),
            fields: BTreeMap::new(),
        };
        forge.board_write(
            &BoardWrite {
                observed,
                policy: board_core::AgentPolicy::parse(board_core::DEFAULT_AGENT_POLICY)?,
                operation,
            },
            true,
            &mut 1,
        )
    }

    fn receipt(&self, facts: &Facts, plan: u64, body: Value) -> Result<()> {
        self.write(
            facts,
            BoardOperation::Receipt {
                marker: plan::DISPATCH_MARKER.into(),
                plan,
                body: body.to_string(),
            },
        )
    }

    fn claim(&self, facts: &Facts, plan: u64) -> Result<Option<crate::PlanClaim>> {
        self.forge_for(&facts.issue).plan_claim(
            &self.write_identity(&facts.issue),
            plan::DISPATCH_MARKER,
            plan,
        )
    }

    fn claim_time(&self, facts: &Facts, claim: u64) -> Result<OffsetDateTime> {
        let comments = self
            .forge_for(&facts.issue)
            .plan_thread(&self.write_identity(&facts.issue))?;
        let comment = comments
            .iter()
            .find(|c| c["id"] == claim)
            .context("claim disappeared")?;
        Ok(OffsetDateTime::parse(
            text(comment, "/created_at"),
            &Rfc3339,
        )?)
    }

    fn final_check(
        &self,
        facts: &Facts,
        accepted: &Accepted,
        snapshot: Option<&Value>,
        owner: Option<(&str, u64)>,
    ) -> Result<()> {
        self.serialized()?;
        ensure!(!self.caps.full(Kind::Dispatch), "held by --max-dispatch");
        ensure!(
            choose(
                snapshot,
                &self.config.pools,
                std::slice::from_ref(&accepted.plan.pool),
                self.now
            )
            .is_ok(),
            "pool holds"
        );
        ensure!(
            budget_hold(self.config, snapshot, self.now, 1.0).is_none(),
            "budget holds"
        );
        let latest = self.accepted(&facts.issue)?;
        ensure!(
            latest.id == accepted.id
                && (latest.plan == accepted.plan || {
                    let mut selected = latest.plan.clone();
                    selected.pool = accepted.plan.pool.clone();
                    latest.plan.pool.is_empty() && selected == accepted.plan
                }),
            "plan changed after admission; held"
        );
        let issue = self
            .client
            .get(&api(
                self.board,
                &facts.issue.repository,
                &format!("issues/{}", facts.issue.number),
                &[],
            )?)?
            .body;
        if self.planning.contains(&key(&facts.issue)) {
            // An epic's planning run: still an open, live epic.
            ensure!(
                issue["number"] == facts.issue.number
                    && issue["pull_request"].is_null()
                    && issue["state"] == "open"
                    && self.live_epic(&facts.issue)?,
                "epic is no longer live; held"
            );
        } else {
            self.still_candidate(facts, &issue)?;
        }
        let target = self.target();
        let actions = self.actions(&target)?;
        let runs = actions.active_runs()?;
        ensure!(
            !runs.iter().any(
                |r| run_title(text(r, "/title")).is_some_and(|(_, repo, n)| repo
                    .eq_ignore_ascii_case(&facts.issue.repository)
                    && n == facts.issue.number)
            ),
            "issue already in flight"
        );
        let active: BTreeSet<_> = runs.iter().filter_map(|r| r["id"].as_u64()).collect();
        let mut occupied = active.len();
        let mut owners = BTreeSet::new();
        let mut leased_runs = BTreeSet::new();
        let mut found_owner = false;
        for (_, slot) in self.slots(&actions)? {
            if let Some(lease) = slot.lease() {
                ensure!(
                    owners.insert(lease.claim.clone()),
                    "duplicate lease claim; held"
                );
                ensure!(
                    lease.run == 0 || leased_runs.insert(lease.run),
                    "duplicate run lease; held"
                );
                if owner.is_some_and(|(claim, _)| lease.claim == claim) {
                    ensure!(
                        Some(lease.run) == owner.map(|(_, run)| run)
                            && OffsetDateTime::parse(&lease.expires_at, &Rfc3339)? > self.now,
                        "reservation no longer live"
                    );
                    found_owner = true;
                    continue;
                }
                ensure!(
                    !lease.item.eq_ignore_ascii_case(&facts.issue.display()),
                    "issue already leased"
                );
                if lease.run == 0 || !active.contains(&lease.run) {
                    occupied += 1;
                }
            }
        }
        ensure!(owner.is_none() || found_owner, "reservation disappeared");
        ensure!(
            occupied < self.config.wip as usize,
            "WIP full including reservations; held"
        );
        Ok(())
    }

    /// Whether the board issue `facts`, read again as `issue`, still
    /// qualifies and holds nothing: a candidate, unparented or a live
    /// epic's child, a board member, with its holds read again.
    fn still_candidate(&self, facts: &Facts, issue: &Value) -> Result<()> {
        let (fresh, unread) = self.facts(&instance(self.board), &facts.issue.repository, issue)?;
        ensure!(fresh.issue == facts.issue, "issue identity changed");
        ensure!(
            matches!(
                screen(&fresh, self.config, &[], &self.people()),
                Screen::Unparented | Screen::Child(_)
            ),
            "issue no longer qualifies as a candidate"
        );
        if let crate::forgejo::Membership::Label(member) = &self.board.membership {
            ensure!(fresh.labels.contains(member), "issue left board membership");
        }
        let mut report = Report::default();
        let mut recheck = Schedule {
            client: self.client,
            board: self.board,
            starts: self.starts,
            config: self.config,
            ids: self.ids,
            now: self.now,
            caps: self.caps,
            report: &mut report,
            pulls: BTreeMap::new(),
            mode: self.mode,
            parents: self.parents.clone(),
            planning: BTreeSet::new(),
            epic_started: 0,
        };
        let fresh = recheck
            .complete(&fresh, unread.as_deref())
            .map_err(|h| anyhow::anyhow!("{}", h.reason))?;
        ensure!(
            fresh.open
                && issue["state"] == "open"
                && hold(&fresh, &BTreeMap::new(), &self.people()).is_none(),
            "issue now holds"
        );
        Ok(())
    }

    fn reserve(
        &self,
        facts: &Facts,
        accepted: &Accepted,
        snapshot: Option<&Value>,
    ) -> Result<(u64, ForgejoLease)> {
        let target = self.target();
        let actions = self.actions(&target)?;
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|e| anyhow::anyhow!("lease identity randomness: {e}"))?;
        let nonce: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let lease = ForgejoLease {
            run: 0,
            claim: format!("{}:{}/{}", facts.issue.display(), accepted.id, nonce),
            item: facts.issue.display(),
            pool: accepted.plan.pool.clone(),
            taken_at: self.now.format(&Rfc3339)?,
            expires_at: (self.now + time::Duration::seconds(lease::LEASE_SECONDS))
                .format(&Rfc3339)?,
        };
        let (n, slot) = self
            .slots(&actions)?
            .into_iter()
            .find(|(_, s)| s.lease().is_none())
            .context("no free lease slot")?;
        ensure!(
            actions.admit(&slot, &lease, self.now, || self
                .final_check(facts, accepted, snapshot, None))?,
            "lease slot race lost; held"
        );
        Ok((n, lease))
    }

    fn bind(&self, slot: u64, lease: &ForgejoLease, run: u64) -> Result<()> {
        let target = self.target();
        let actions = self.actions(&target)?;
        let slots = self.slots(&actions)?;
        ensure!(
            !slots.iter().any(|(_, s)| s
                .lease()
                .is_some_and(|l| l.run == run && l.claim != lease.claim)),
            "run already leased"
        );
        let observed = actions.slot(LEASE_BRANCH, slot)?;
        ensure!(
            actions.bind(&observed, &lease.claim, run, self.now)?,
            "lease bind lost race; held"
        );
        Ok(())
    }

    fn apply(&mut self, facts: &Facts, pool: &str, snapshot: Option<&Value>) -> Result<bool> {
        let mut accepted = self.accepted(&facts.issue)?;
        let existing = self.claim(facts, accepted.id)?;
        if let Some(claim) = existing {
            ensure!(
                claim.recorder == self.ids.bot,
                "plan claimed by another recorder; held"
            );
            let target = self.target();
            let actions = self.actions(&target)?;
            if claim.receipts.is_empty() {
                let prefix = format!("{}:{}/", facts.issue.display(), accepted.id);
                let slots = self.slots(&actions)?;
                let matches: Vec<_> = slots
                    .iter()
                    .filter(|(_, s)| s.lease().is_some_and(|l| l.claim.starts_with(&prefix)))
                    .collect();
                ensure!(
                    matches.len() == 1,
                    "claim has no unique reservation; hold for a person, never retry"
                );
                let (slot, observed) = matches[0];
                let lease = observed.lease().context("reservation disappeared")?;
                accepted.plan.pool = lease.pool.clone();
                let found =
                    actions.recover(&accepted.start(), self.claim_time(facts, claim.claim)?)?;
                let run: u64 = found.id.parse()?;
                if lease.run == 0 {
                    self.bind(*slot, lease, run)?;
                } else {
                    ensure!(lease.run == run, "recovered run differs from reservation");
                }
                self.receipt(
                    facts,
                    accepted.id,
                    json!({"event":"found", "run":run, "url":found.url, "pool":lease.pool}),
                )?;
                self.line(format!(
                    "recover {}: found {}",
                    facts.issue.display(),
                    found.url
                ));
                return Ok(false);
            }
            let receipts: Vec<Value> = claim
                .receipts
                .iter()
                .filter_map(|r| serde_json::from_str(r).ok())
                .collect();
            let started = receipts
                .iter()
                .find(|r| matches!(r["event"].as_str(), Some("started" | "found")));
            let Some(started) = started else {
                self.line(format!(
                    "hold {}: plan already receipted; never retry",
                    facts.issue.display()
                ));
                return Ok(false);
            };
            let run = started["run"].as_u64().context("receipt has no run")?;
            if !self.config.reruns
                || receipts
                    .iter()
                    .any(|r| r["event"] == "rerun" && r["run"] == run)
            {
                self.line(format!(
                    "hold {}: plan already dispatched",
                    facts.issue.display()
                ));
                return Ok(false);
            }
            if actions.status(run)?["status"] != "failure" {
                return Ok(false);
            }
            accepted.plan.pool = started["pool"]
                .as_str()
                .context("receipt has no pool")?
                .into();
            let at = self.claim_time(facts, claim.claim)?;
            let mut sent = false;
            let outcome = actions.rerun(run, &accepted.start(), at, || {
                let (slot, lease) = self.reserve(facts, &accepted, snapshot)?;
                self.bind(slot, &lease, run)?;
                self.receipt(
                    facts,
                    accepted.id,
                    json!({"event":"rerun", "run":run, "attempt":1, "pool":accepted.plan.pool}),
                )?;
                self.final_check(facts, &accepted, snapshot, Some((&lease.claim, run)))?;
                let current = self
                    .claim(facts, accepted.id)?
                    .context("claim disappeared")?;
                ensure!(
                    current.claim == claim.claim && current.recorder == self.ids.bot,
                    "claim changed before rerun"
                );
                sent = true;
                Ok(())
            });
            let line = outcome_line(&outcome);
            self.line(format!("run rerun {}: {line}", facts.issue.display()));
            return Ok(sent);
        }
        if accepted.plan.pool.is_empty() {
            accepted.plan.pool = pool.into();
        }
        if let Some(held) = self
            .caps
            .check(Kind::Dispatch)
            .or_else(|| self.caps.check_many(Kind::Write, 4))
        {
            self.line(format!("{held}: start {}", facts.issue.display()));
            return Ok(false);
        }
        let (slot, lease) = self.reserve(facts, &accepted, snapshot)?;
        self.write(
            facts,
            BoardOperation::Claim {
                marker: plan::DISPATCH_MARKER.into(),
                plan: accepted.id,
            },
        )?;
        let claim = self
            .claim(facts, accepted.id)?
            .context("claim disappeared")?;
        let at = self.claim_time(facts, claim.claim)?;
        let target = self.target();
        let actions = self.actions(&target)?;
        let outcome = actions.start(&accepted.start(), at, || {
            self.final_check(facts, &accepted, snapshot, Some((&lease.claim, 0)))?;
            let current = self
                .claim(facts, accepted.id)?
                .context("claim disappeared")?;
            ensure!(
                current.claim == claim.claim && current.receipts.is_empty(),
                "claim changed before dispatch"
            );
            let observed = actions.slot(LEASE_BRANCH, slot)?;
            ensure!(
                observed.lease() == Some(&lease),
                "reservation changed before dispatch"
            );
            Ok(())
        });
        let mut receipt =
            json!({"event":"held", "detail":outcome_line(&outcome), "pool":accepted.plan.pool});
        if let StartOutcome::Started(handle) = &outcome {
            let run: u64 = handle.id.parse()?;
            // Even a failed bind must not lose the confirmed start's receipt.
            let bound = self.bind(slot, &lease, run);
            receipt = json!({"event":"started", "run":run, "url":handle.url, "pool":accepted.plan.pool,
                "bind_error":bound.err().map(|e| format!("{e:#}"))});
        }
        self.receipt(facts, accepted.id, receipt)?;
        self.line(format!(
            "start {}: {}",
            facts.issue.display(),
            outcome_line(&outcome)
        ));
        // A refusal or ambiguity still owns its reservation until expiry.
        Ok(true)
    }

    fn line(&mut self, line: String) {
        self.report.lines.push(line);
    }

    fn record(&mut self, entry: Entry) {
        self.line(entry.line());
        self.report.holds.push(entry);
    }

    fn held(&mut self, facts: &Facts, hold: Hold) {
        self.record(Entry {
            scope: Scope::Item(facts.issue.clone()),
            hold,
            priority: facts.priority.clone(),
            why: None,
        });
    }

    fn pass_held(&mut self, kind: HoldKind, why: impl Into<String>) {
        self.record(Entry {
            scope: Scope::Pass,
            hold: Hold::new(kind, why),
            priority: None,
            why: None,
        });
    }

    fn people(&self) -> [&str; 2] {
        [&self.config.operator, &self.config.bot]
    }

    /// The login of `user` where its id is the bot's or the operator's;
    /// a login alone, which can be renamed and taken, is no one's.
    fn trusted(&self, user: &Value) -> Option<String> {
        let id = user["id"].as_u64()?;
        let Ids { bot, operator } = self.ids;
        (id == bot)
            .then(|| self.config.bot.clone())
            .or_else(|| (id == operator).then(|| self.config.operator.clone()))
    }

    fn run(&mut self, snapshot: Option<&Value>) -> Result<()> {
        let config = self.config;
        self.report.tally.1.started = 0;
        let mut recovered = BTreeSet::new();
        if !config.work() {
            self.line("schedule: off (wip 0 or no workRepositories)".into());
            return Ok(());
        }
        if !matches!(self.mode, Mode::Preview) {
            self.serialized()?;
            if !config.leases {
                self.pass_held(
                    HoldKind::Wip,
                    "applying Forgejo work requires leases; no reservation, no start",
                );
                return Ok(());
            }
            // Recovery is not admission: even a full WIP or a held pool must
            // not hide a claim left between POST and its receipt.
            let issues = self.issues()?;
            for (facts, _) in issues.values() {
                let Ok(accepted) = self.accepted(&facts.issue) else {
                    continue;
                };
                if self
                    .claim(facts, accepted.id)?
                    .is_some_and(|c| c.receipts.is_empty())
                {
                    recovered.insert(key(&facts.issue));
                    if let Err(error) = self.apply(facts, "", snapshot) {
                        self.held(facts, Hold::new(HoldKind::Unreadable, format!("{error:#}")));
                    }
                }
            }
            self.reap()?;
        }
        let (active, mut busy) = match self.runs() {
            Ok(runs) => runs,
            Err(error) => {
                self.report.failures += 1;
                let why = format!("the runs in flight could not all be listed: {error:#}");
                self.pass_held(HoldKind::Wip, format!("{why}; new runs wait"));
                return Ok(());
            }
        };
        // Stage 3's planning starts count even where not listed yet; where
        // they are, a slot waits a pass rather than the WIP limit giving.
        let active = active + self.epic_started;
        self.report.tally.0.in_flight = Some(active);
        let wip = config.wip as usize;
        if active >= wip {
            self.pass_held(HoldKind::Wip, format!("{active} of {wip} runs active"));
            return Ok(());
        }
        let all: Vec<String> = config.pools.0.iter().map(|(n, _)| n.clone()).collect();
        if let Err(why) = choose(snapshot, &config.pools, &all, self.now) {
            self.pass_held(HoldKind::Pools, format!("{why}; new runs wait"));
            return Ok(());
        }
        if let Some(why) = budget_hold(config, snapshot, self.now, 1.0) {
            self.caps.hold();
            self.pass_held(HoldKind::Budget, format!("{why}; new runs wait"));
            return Ok(());
        }
        let board = self.issues()?;
        let candidates = self.candidates(&board);
        let mut free = wip - active;
        for (n, (facts, unread)) in candidates.iter().enumerate() {
            if recovered.contains(&key(&facts.issue)) {
                continue;
            }
            let left = candidates.len() - n;
            if free == 0 {
                self.line(format!("wip: no slot left; {left} more candidate(s) wait"));
                break;
            }
            // What the listing already says holds it before any read: a
            // label, Blocked or a run in flight.
            if let Some(held) = hold(facts, &busy, &self.people()) {
                self.held(facts, held);
                continue;
            }
            let facts = match self.complete(facts, unread.as_deref()) {
                Ok(facts) => facts,
                Err(hold) => {
                    self.held(facts, hold);
                    continue;
                }
            };
            if let Some(held) = hold(&facts, &busy, &self.people()) {
                self.held(&facts, held);
                continue;
            }
            let pool = match choose(snapshot, &config.pools, &all, self.now) {
                Ok(pool) => pool,
                Err(why) => {
                    self.held(
                        &facts,
                        Hold::new(HoldKind::Pools, format!("no pool: {why}")),
                    );
                    continue;
                }
            };
            let line = format!("would start {} pool={pool}", facts.issue.display());
            if !matches!(self.mode, Mode::Preview) {
                match self.apply(&facts, &pool, snapshot) {
                    Ok(true) => {
                        free -= 1;
                        busy.insert(key(&facts.issue), "started in this pass".into());
                        self.report.tally.1.started += 1;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        self.report.failures += 1;
                        self.held(
                            &facts,
                            Hold::new(HoldKind::Unreadable, format!("{error:#}")),
                        );
                    }
                }
                continue;
            }
            free -= 1;
            if let Some(held) = self.caps.check(Kind::Dispatch) {
                self.line(format!("{held}: {line}"));
                continue;
            }
            self.caps.count(Kind::Dispatch);
            busy.insert(key(&facts.issue), "started in this pass".into());
            self.report.tally.1.started += 1;
            self.line(line);
        }
        let started = self.report.tally.1.started;
        self.report.tally.0.in_flight = Some(active + started);
        Ok(())
    }

    /// The caller's runs in flight, and the issues they name by `run-name`
    /// (`KIND OWNER/REPO#N (POOL)`), by [`key`], with each run's link. A
    /// listing cut short is an error.
    fn runs(&self) -> Result<(usize, BTreeMap<String, String>)> {
        let config = self.config;
        let target = config.target(config.dispatch_ref.as_deref().unwrap_or(LISTING_REF));
        let inputs = LISTING_INPUTS.iter().map(|s| s.to_string()).collect();
        let actions =
            crate::dispatch::Forgejo::new(self.client, self.board, &target, inputs, self.ids.bot)?;
        let runs = actions.active_runs()?;
        let host = instance(self.board);
        let mut busy = BTreeMap::new();
        for run in &runs {
            let Some((_, repository, number)) = run_title(text(run, "/title")) else {
                continue;
            };
            let issue = ItemRef {
                host: host.clone(),
                repository: repository.into(),
                number,
            };
            // As the dispatch adapter links a run: by its index in the
            // caller, under the trusted instance, never the run's own URL.
            let link = match run["index_in_repo"].as_u64() {
                Some(index) => format!("{host}/{}/actions/runs/{index}", target.caller),
                None => format!("run {}", run["id"]),
            };
            busy.entry(key(&issue)).or_insert(link);
        }
        Ok((runs.len(), busy))
    }

    /// The open issues of each work repository on the board, by [`key`]. A
    /// listing past [`LIST_PAGES`] or not read whole is an error:
    /// scheduling from part of the board would start the wrong work.
    fn issues(&self) -> Result<BTreeMap<String, Read>> {
        let board = self.board;
        let host = instance(board);
        let mut query = vec![("type", "issues"), ("state", "open")];
        if let crate::forgejo::Membership::Label(label) = &board.membership {
            query.push(("labels", label.as_str()));
        }
        let mut items = BTreeMap::new();
        for repository in &board.repositories {
            // Only a work repository's issues can be candidates.
            if !self.config.works_in(repository) {
                continue;
            }
            let url = api(board, repository, "issues", &query)?;
            let (issues, whole) = pages(url, LIST_PAGES, |url| listing(self.client, url))?;
            ensure!(
                whole,
                "the open issues of {repository} could not all be read in {LIST_PAGES} pages; the board is incomplete"
            );
            for issue in &issues {
                let read = self.facts(&host, repository, issue)?;
                let at = key(&read.0.issue);
                ensure!(
                    items.insert(at, read).is_none(),
                    "{repository}#{} listed twice; the board is incomplete",
                    issue["number"]
                );
            }
        }
        Ok(items)
    }

    /// An open issue as listed, without its dependencies and pull requests,
    /// which [`Self::complete`] reads for a candidate.
    fn facts(&self, host: &str, repository: &str, issue: &Value) -> Result<Read> {
        let number = issue["number"].as_u64().context("issue has no number")?;
        let name = format!("{repository}#{number}");
        // Issue versus pull request identity is a boundary: `type=issues` must hold.
        ensure!(issue["pull_request"].is_null(), "{name} is a pull request");
        let listed = text(issue, "/repository/full_name");
        ensure!(
            listed.eq_ignore_ascii_case(repository),
            "{name} reported from {listed:?}"
        );
        ensure!(issue["state"] == "open", "{name} is listed open but is not");
        let labels = issue["labels"]
            .as_array()
            .with_context(|| format!("{name} has no labels"))?
            .iter()
            .map(|l| l["name"].as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .with_context(|| format!("a label of {name} has no name"))?;
        let created = OffsetDateTime::parse(text(issue, "/created_at"), &Rfc3339)
            .with_context(|| format!("{name} has no creation time"))?;
        let (priority, unread) = match crate::forgejo::priority(&labels) {
            Ok(priority) => (priority.map(|p| PRIORITIES[p as usize].to_owned()), None),
            Err(why) => (None, Some(format!("its Priority could not be read: {why}"))),
        };
        // `status/blocked` is the board's Blocked where Status is labels.
        let blocked = self.board.status_labels
            && labels
                .iter()
                .any(|l| l.eq_ignore_ascii_case("status/blocked"));
        let at = ItemRef {
            host: host.into(),
            repository: repository.into(),
            number,
        };
        // A live epic's child where both relationship comments agree
        // (stage 3); a claim of one side alone is held by [`Self::complete`].
        let parent = self.parents.get(&key(&at)).map(|p| p.parent.clone());
        let facts = Facts {
            issue: at,
            open: true,
            created,
            updated: None,
            author: self.trusted(&issue["user"]),
            labels,
            labels_complete: true,
            blocked_by: 0,
            pulls: Vec::new(),
            pulls_complete: true,
            parent,
            priority,
            status: blocked.then(|| BLOCKED_STATUS.into()),
            why: None,
        };
        Ok((facts, unread))
    }

    /// The board issues that qualify, in order, by [`screen`] and [`rank`],
    /// after the `candidates:` line.
    fn candidates<'b>(&mut self, board: &'b BTreeMap<String, Read>) -> Vec<&'b Read> {
        let config = self.config;
        let people = [config.operator.as_str(), config.bot.as_str()];
        let options: Vec<String> = PRIORITIES.iter().map(|p| p.to_string()).collect();
        let mut ranked = Vec::new();
        let (mut visitors, mut unparented) = (0, 0);
        for read in board.values() {
            let facts = &read.0;
            match screen(facts, config, &[], &people) {
                Screen::Out => {}
                Screen::Held(hold) => self.held(facts, hold),
                Screen::Visitor => visitors += 1,
                Screen::Unparented => {
                    unparented += 1;
                    let rank = rank(&options, None, facts.priority.as_deref(), facts);
                    ranked.push((rank, read));
                }
                // Only a live epic's children have a parent here.
                Screen::Child(_) => {
                    let epic =
                        (self.parents.get(&key(&facts.issue))).and_then(|p| p.priority.as_deref());
                    let rank = rank(&options, epic, facts.priority.as_deref(), facts);
                    ranked.push((rank, read));
                }
            }
        }
        ranked.sort_by(|a, b| a.0.cmp(&b.0));
        self.report.tally.1.candidates = Some(ranked.len());
        let line = candidates_line(config, ranked.len(), (visitors, 0, unparented));
        self.line(line);
        ranked.into_iter().map(|(_, read)| read).collect()
    }

    /// `facts` with its open dependencies and its trusted closing pull
    /// requests, or the hold of what could not be read. A read that fails,
    /// rather than reads more than its pages, is a failure too.
    fn complete(
        &mut self,
        facts: &Facts,
        unread: Option<&str>,
    ) -> std::result::Result<Facts, Hold> {
        let unreadable = |why: String| Hold::new(HoldKind::Unreadable, why);
        if let Some(why) = unread {
            return Err(unreadable(why.to_owned()));
        }
        let mut facts = facts.clone();
        let issue = &facts.issue;
        if self.config.epics.is_some() {
            // Its own parent comment, where no live epic lists it back: one
            // side alone, or an epic that is not live, holds it.
            let thread = self.forge().plan_thread(&self.write_identity(issue));
            let named = thread.and_then(|t| crate::forgejo::epics::parent(&t, self.ids.bot));
            match (named, &facts.parent) {
                (Err(error), _) => {
                    self.report.failures += 1;
                    let why = format!("its parent comment could not be read: {error:#}");
                    return Err(unreadable(why));
                }
                (Ok(Some(epic)), None) => {
                    let why = format!(
                        "its parent {epic} is not a live epic (open, labelled epic by the operator or the bot) that lists it"
                    );
                    return Err(Hold::new(HoldKind::EpicNotLive, why));
                }
                _ => {}
            }
        }
        let path = format!("issues/{}/dependencies", issue.number);
        // Its pages carry no total ([`pages_to_end`]).
        let dependencies = api(self.board, &issue.repository, &path, &[])
            .and_then(|url| pages_to_end(url, LIST_PAGES, |url| listing(self.client, url)));
        facts.blocked_by = match dependencies {
            // A blocker the token may not read is listed `HIDDEN`, and open.
            Ok((dependencies, true)) => dependencies
                .iter()
                .filter(|d| d["state"] != "closed")
                .count() as u64,
            Ok((_, false)) => {
                return Err(unreadable("its dependencies could not all be read".into()));
            }
            // A repository without dependencies enabled answers 404 to every
            // issue, and has none (issue_dependency.go's GetIssueDependencies).
            Err(error) if error.downcast_ref::<crate::forgejo::NotFound>().is_some() => 0,
            Err(error) => {
                self.report.failures += 1;
                let why = format!("its dependencies could not all be read: {error:#}");
                return Err(unreadable(why));
            }
        };
        let repository = issue.repository.to_ascii_lowercase();
        if !self.pulls.contains_key(&repository) {
            let query = [("state", "open")];
            let read = api(self.board, &issue.repository, "pulls", &query)
                .and_then(|url| pages(url, LIST_PAGES, |url| listing(self.client, url)))
                .map_err(|error| format!("{error:#}"));
            self.report.failures += usize::from(read.is_err());
            self.pulls.insert(repository.clone(), read);
        }
        let (pulls, whole) = match &self.pulls[&repository] {
            Ok(read) => read,
            Err(error) => {
                return Err(unreadable(format!(
                    "its pull requests could not all be read: {error}"
                )));
            }
        };
        let web = format!("{}/", instance(self.board));
        let mut closing = Vec::new();
        for pull in pulls {
            let Some(author) = self.trusted(&pull["user"]) else {
                continue;
            };
            let body = pull["body"].as_str().unwrap_or_default();
            if !stack::closes_at(body, &web, &issue.repository, issue.number) {
                continue;
            }
            let Some(number) = pull["number"].as_u64() else {
                return Err(unreadable("a pull request has no number".into()));
            };
            closing.push(ClosingPull {
                number,
                repository: issue.repository.clone(),
                author: Some(author),
                updated: (pull["updated_at"].as_str())
                    .and_then(|at| OffsetDateTime::parse(at, &Rfc3339).ok()),
            });
        }
        facts.pulls = closing;
        facts.pulls_complete = *whole;
        Ok(facts)
    }
}

fn outcome_line(outcome: &StartOutcome) -> String {
    match outcome {
        StartOutcome::Started(handle) => handle.url.clone(),
        StartOutcome::NotStarted(error) => format!("refused: {error:#}"),
        StartOutcome::Ambiguous(error) => format!("ambiguous: {error:#}; never retry"),
        StartOutcome::Unnamed => "ambiguous: unnamed start; never retry".into(),
    }
}

/// Bound each broker read as well as forge reads. A slow read cannot admit a
/// second request after the deadline; HTTP transports also bound each call.
struct BrokerReads<'a, B>(&'a Budget<'a, B>);

impl<B: Broker> Broker for BrokerReads<'_, B> {
    fn usage(&self, url: &str) -> Result<Value> {
        self.0.spend(false)?;
        self.0.inner.usage(url)
    }

    fn grant(&self, url: &str) -> Result<Value> {
        self.0.spend(false)?;
        self.0.inner.grant(url)
    }
}

/// Stage 1: the usage snapshot stage 4 paces pools by, none without a broker.
fn refresh<B: Broker>(
    broker: &Budget<'_, B>,
    config: &Config,
    now: OffsetDateTime,
    report: &mut Report,
) -> Result<Option<Value>> {
    let Some(url) = config.usage.as_ref().and_then(|u| u.url.as_deref()) else {
        report
            .lines
            .push("usage: no usage.url; no broker reading, no forge fallback".into());
        return Ok(None);
    };
    let broker = BrokerReads(broker);
    let grant = config
        .grant()
        .and_then(|budget| match usage::read_grant(&broker, budget) {
            Ok(grant) => Some(grant),
            Err(error) => {
                report.failures += 1;
                report.lines.push(format!(
                    "usage error: reading the grant: {error:#}; every pool holds"
                ));
                None
            }
        });
    let read = broker
        .usage(url)
        .and_then(|answer| usage::reading(&answer, config, grant.as_ref(), now));
    let snapshot = match read {
        Ok((_, snapshot)) => snapshot,
        Err(error) => {
            report.failures += 1;
            report.lines.push(format!(
                "usage error: reading the broker: {error:#}; no forge fallback"
            ));
            let mut snapshot = usage::bare(&config.pools, now)?;
            if let Some(budget) = config.grant() {
                usage::grants(&mut snapshot, grant.as_ref(), budget, now)?;
            }
            snapshot
        }
    };
    report
        .lines
        .push(format!("usage: {}", usage::pools(&snapshot).join("; ")));
    Ok(Some(snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forgejo::Response;

    mod apply;
    mod epics;

    fn file() -> Value {
        let contract: Value = serde_json::from_str(CONTRACT).unwrap();
        json!({"operator": "operator", "bot": "bot", "dispatchRepo": "example/control",
            "dispatchWorkflow": "agent.yml", "board": contract["instance"]["board"],
            "pools": {"claude": {}, "openai": {}}, "usage": {"url": "http://broker.internal/usage"}})
    }

    fn config(value: &Value) -> Config {
        Config::parse(&value.to_string()).unwrap()
    }

    fn set(value: &mut Value, path: &str, leaf: Value) {
        let (head, rest) = path.split_once('.').unwrap_or((path, ""));
        if rest.is_empty() {
            value[head] = leaf;
        } else {
            if !value[head].is_object() {
                value[head] = json!({});
            }
            set(&mut value[head], rest, leaf);
        }
    }

    #[test]
    fn naming_follows_trusted_board() {
        for (login, forgejo, github) in [
            ("alice", true, true),
            ("alice-smith", true, true),
            ("alice.smith", true, false),
            ("alice_smith", true, true),
            ("a..b", false, false),
            ("a._b", false, false),
            (".alice", false, false),
            ("alice.", false, false),
            ("..", false, false),
            ("../alice", false, false),
            ("alice/bob", false, false),
            ("alice\\bob", false, false),
            ("alice?x", false, false),
            ("alice#x", false, false),
            ("alice\n", false, false),
            ("%2e%2e", false, false),
            ("//evil.example", false, false),
            ("https://evil.example", false, false),
            ("alice@evil.example", false, false),
        ] {
            for (is_forgejo, accepted) in [(true, forgejo), (false, github)] {
                for field in ["operator", "bot", "dispatchRepo", "workRepositories"] {
                    let mut value = file();
                    // Keep the GitHub baseline independent of broker settings.
                    value.as_object_mut().unwrap().remove("usage");
                    value.as_object_mut().unwrap().remove("pools");
                    if !is_forgejo {
                        value.as_object_mut().unwrap().remove("board");
                    }
                    value[field] = match field {
                        "dispatchRepo" => json!(format!("{login}/work.repo")),
                        "workRepositories" => json!([format!("{login}/work.repo")]),
                        _ => json!(login),
                    };
                    assert_eq!(
                        Config::parse(&value.to_string()).is_ok(),
                        accepted,
                        "forgejo={is_forgejo} {field}={login:?}"
                    );
                }
            }
        }
        for name in [
            "work.repo",
            ".config",
            "work_repo",
            "work-repo",
            ".",
            "..",
            "a/b",
            "a\\b",
            "a?b",
            "a#b",
            "a\n",
            "%2f",
            "//evil.example",
        ] {
            let accepted = matches!(name, "work.repo" | ".config" | "work_repo" | "work-repo");
            for field in ["dispatchRepo", "workRepositories"] {
                let mut value = file();
                let repo = format!("example/{name}");
                value[field] = if field == "workRepositories" {
                    json!([repo])
                } else {
                    json!(repo)
                };
                assert_eq!(
                    Config::parse(&value.to_string()).is_ok(),
                    accepted,
                    "{field}={name:?}"
                );
            }
        }
    }

    #[test]
    fn contract_drives_every_refused_option_before_decoding() {
        let contract: Value = serde_json::from_str(CONTRACT).unwrap();
        for (option, entry) in contract["options"].as_object().unwrap() {
            if entry["verdict"] != "refused" {
                continue;
            }
            for supplied in [json!(false), json!(0), json!([]), json!({}), Value::Null] {
                let mut value = file();
                set(&mut value, option, supplied);
                let error = Config::parse(&value.to_string()).unwrap_err().to_string();
                assert_eq!(
                    error,
                    format!(
                        "unsupported_capability: {option}: {}",
                        entry["reason"].as_str().unwrap()
                    )
                );
            }
        }
    }

    #[test]
    fn temporary_refusals_and_broker_only_usage() {
        assert!(config(&file()).forgejo_board().is_some());
        let mut epics = file();
        set(&mut epics, "epics.repository", json!("example/plans"));
        set(&mut epics, "epics.close", json!(true));
        assert!(Config::parse(&epics.to_string()).is_ok());
        for (option, follow_up) in [
            ("holds.issue", "#513"),
            ("usage.repository", "#513"),
            ("usage.issue", "#513"),
            ("usage.probe", "#513"),
            ("sweep.max", "sweep ownership"),
            ("revisit.perPass", "revisit ownership"),
        ] {
            let mut value = file();
            set(&mut value, option, json!(false));
            let error = Config::parse(&value.to_string()).unwrap_err().to_string();
            assert!(
                error.starts_with("unsupported_capability:"),
                "{option}: {error}"
            );
            assert!(error.contains(follow_up), "{option}: {error}");
        }
        for variable in ["GH_TOKEN", "GITHUB_TOKEN", "DISPATCH_TOKEN"] {
            let mut value = file();
            value["board"]["tokenEnv"] = variable.into();
            assert!(Config::parse(&value.to_string()).is_err());
        }
        let mut github = file();
        github["board"] = json!({"owner": "example", "project": 1});
        assert!(Config::parse(&github.to_string()).is_err());
        github["usage"]["repository"] = "example/private".into();
        github["usage"]["issue"] = 1.into();
        assert_eq!(
            config(&github).github_board().unwrap().priority_field,
            "Priority"
        );
    }

    const BOT: u64 = 1234;
    const OPERATOR: u64 = 5678;

    /// The board, pull requests, dependencies and runs a [`Fake`] lists,
    /// each with how many more its total says there are than it lists.
    #[derive(Default)]
    struct Listed {
        issues: Vec<Value>,
        pulls: (Vec<Value>, u64),
        dependencies: BTreeMap<u64, Vec<Value>>,
        runs: (Vec<Value>, u64),
        /// The last path segment of a listing that answers 404 (`404 NAME`)
        /// or fails (`NAME`).
        broken: &'static str,
    }

    struct Fake {
        calls: RefCell<Vec<String>>,
        bot: Value,
        operator: Value,
        listed: Listed,
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                calls: RefCell::default(),
                bot: json!({"login": "bot", "id": BOT}),
                operator: json!({"login": "operator", "id": OPERATOR}),
                listed: Listed::default(),
            }
        }
    }

    /// The `page` of `items` a listing `url` asks for, `limit` a page.
    fn page(url: &url::Url, items: &[Value]) -> Vec<Value> {
        let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        let limit: usize = query["limit"].parse().unwrap();
        let page: usize = query["page"].parse().unwrap();
        items
            .iter()
            .skip((page - 1) * limit)
            .take(limit)
            .cloned()
            .collect()
    }

    impl ForgejoTransport for Fake {
        fn get(&self, url: &url::Url) -> Result<Response> {
            self.calls.borrow_mut().push(url.to_string());
            let listed = &self.listed;
            let paged = |items: &[Value], more: u64| {
                let total = Some(items.len() as u64 + more);
                Ok(Response {
                    body: page(url, items).into(),
                    total,
                })
            };
            let path = url.path().strip_prefix("/subpath/api/v1/").unwrap();
            let last = path.rsplit('/').next().unwrap();
            if listed.broken == format!("404 {last}") {
                return Err(crate::forgejo::NotFound.into());
            }
            ensure!(listed.broken != last, "GET {path}: Forgejo HTTP status 500");
            let body = match path.split('/').collect::<Vec<_>>()[..] {
                ["user"] => self.bot.clone(),
                ["users", "operator"] => self.operator.clone(),
                ["repos", "example", "work", "issues"] => return paged(&listed.issues, 0),
                ["repos", "example", "work", "pulls"] => {
                    return paged(&listed.pulls.0, listed.pulls.1);
                }
                ["repos", "example", "work", "issues", n, "dependencies"] => {
                    let n = n.parse().unwrap();
                    let found = listed.dependencies.get(&n).cloned().unwrap_or_default();
                    // IssueListWithoutPagination: no X-Total-Count.
                    return Ok(Response {
                        body: page(url, &found).into(),
                        total: None,
                    });
                }
                ["repos", "example", "control", "actions", "runs"] => {
                    let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
                    assert_eq!(query["workflow_id"], "agent.yml");
                    let runs: Vec<Value> = (listed.runs.0.iter())
                        .filter(|r| r["status"] == query["status"].as_str())
                        .cloned()
                        .collect();
                    let total = runs.len() as u64 + listed.runs.1;
                    return Ok(Response {
                        body: json!({"total_count": total, "workflow_runs": page(url, &runs)}),
                        total: None,
                    });
                }
                _ => panic!("unexpected forge read {path}"),
            };
            ensure!(body.is_object(), "identity unreadable");
            Ok(Response { body, total: None })
        }

        fn send(&self, _: &str, _: &url::Url, _: &Value) -> Result<(u16, Value)> {
            panic!("preview wrote")
        }
    }

    struct BrokerFake {
        calls: RefCell<Vec<String>>,
        usage: Option<Value>,
        grant: Option<Value>,
    }

    impl Default for BrokerFake {
        fn default() -> Self {
            Self {
                calls: RefCell::default(),
                usage: Some(usage::tests::broker()),
                grant: Some(usage::tests::grant()),
            }
        }
    }

    impl Broker for BrokerFake {
        fn usage(&self, url: &str) -> Result<Value> {
            assert_eq!(url, "http://broker.internal/usage");
            self.calls.borrow_mut().push("usage".into());
            self.usage.clone().context("usage unavailable")
        }

        fn grant(&self, url: &str) -> Result<Value> {
            assert_eq!(url, "http://broker.internal/grant");
            self.calls.borrow_mut().push("grant".into());
            self.grant.clone().context("grant unavailable")
        }
    }

    fn preview_config() -> Config {
        let mut value = file();
        value["board"]["url"] = "https://forge.example:8443/subpath".into();
        config(&value)
    }

    /// A preview through [`Recorder`], which its [`Replay`] repeats, with no
    /// transport or broker, line for line: every preview test covers replay.
    fn preview(fake: &Fake, broker: &BrokerFake, config: &Config) -> Report {
        let (board, now) = (config.forgejo_board().unwrap(), usage::tests::now());
        let preview = (Mode::Preview, Limits::default());
        let recorder = Recorder::new(fake, broker, board);
        let report = run(&recorder, &recorder, config, preview, now).unwrap();
        let fixture = recorder.fixture(now).unwrap();
        let replay = Replay::from_json(&serde_json::from_str(&fixture).unwrap(), board).unwrap();
        let replayed = run(&replay, &replay, config, preview, replay.now).unwrap();
        replay.finish().unwrap();
        assert_eq!(replayed.lines, report.lines);
        assert_eq!(replayed.summary_line(), report.summary_line());
        assert_eq!(
            (replayed.failures, replayed.spent.requests),
            (report.failures, report.spent.requests)
        );
        report
    }

    #[test]
    fn fake_transport_preview_output() {
        let (fake, broker, config) = (Fake::default(), BrokerFake::default(), preview_config());
        let report = preview(&fake, &broker, &config);
        assert_eq!(report.failures, 0);
        assert_eq!(report.spent.requests, 3);
        assert_eq!(report.lines.len(), 7);
        assert!(report.lines[0].starts_with("identity: bot bot (id 1234)"));
        let snapshot = usage::reading(&usage::tests::broker(), &config, None, usage::tests::now())
            .unwrap()
            .1;
        assert_eq!(
            report.lines[1],
            format!("usage: {}", usage::pools(&snapshot).join("; "))
        );
        assert_eq!(
            report.lines[2..],
            [
                "stage 2 follow pull requests: not built on Forgejo yet",
                "stage 3 epics: off (no epics)",
                "schedule: off (wip 0 or no workRepositories)",
                "stage 5 status: not built on Forgejo yet",
                "stage 6 cost: not built on Forgejo yet",
            ]
        );
        assert_eq!(
            *fake.calls.borrow(),
            [
                "https://forge.example:8443/subpath/api/v1/user",
                "https://forge.example:8443/subpath/api/v1/users/operator"
            ]
        );
        for line in &report.lines {
            println!("{line}");
        }
        println!("{}", report.summary_line());
    }

    #[test]
    fn identity_failures_stop_immediately() {
        for (bot, operator, reads, diagnostic) in [
            (
                json!({"login": "visitor", "id": 1234}),
                json!({}),
                1,
                "login",
            ),
            (json!({"login": "bot", "id": 999}), json!({}), 1, "bot id"),
            (Value::Null, json!({}), 1, "unreadable"),
            (json!({"login": "bot"}), json!({}), 1, "user id"),
            (
                json!({"login": "bot", "id": 1234}),
                json!({"login": "operator", "id": 999}),
                2,
                "reused login",
            ),
            (
                json!({"login": "bot", "id": 1234}),
                Value::Null,
                2,
                "unreadable",
            ),
            (
                json!({"login": "bot", "id": 1234}),
                json!({"login": "other", "id": 5678}),
                2,
                "different login",
            ),
        ] {
            let fake = Fake {
                bot,
                operator,
                ..Fake::default()
            };
            let broker = BrokerFake::default();
            let report = preview(&fake, &broker, &preview_config());
            assert_eq!(report.failures, 1);
            assert_eq!(report.lines.len(), 1);
            assert!(report.lines[0].contains(diagnostic), "{:?}", report.lines);
            assert_eq!(fake.calls.borrow().len(), reads);
            assert!(broker.calls.borrow().is_empty());
        }
    }

    #[test]
    fn broker_failure_missing_readings_and_grants_never_read_forge() {
        for (has_usage, grant, fails) in [
            (true, false, false),
            (false, false, false),
            (true, true, false),
            (false, true, false),
            (true, true, true),
            (false, true, true),
        ] {
            let fake = Fake::default();
            let mut broker = BrokerFake::default();
            if !has_usage {
                broker.usage = None;
            }
            if fails {
                broker.grant = None;
            }
            let mut value = file();
            value["board"]["url"] = "https://forge.example/subpath".into();
            if grant {
                value["budget"] = json!({"source": "broker", "url": "http://broker.internal/grant",
                    "pools": {"claude": "anthropic", "openai": "codex"}});
            }
            let report = preview(&fake, &broker, &config(&value));
            assert_eq!(fake.calls.borrow().len(), 2);
            assert_eq!(
                report.failures,
                usize::from(!has_usage) + usize::from(fails)
            );
            let line = report
                .lines
                .iter()
                .find(|l| l.starts_with("usage: "))
                .unwrap();
            if grant && !fails {
                assert!(line.contains("granted"), "{line}");
            }
            if grant && fails {
                assert!(line.contains("no-grant"), "{line}");
            }
            if !has_usage && !grant {
                assert!(line.contains("no reading (held)"));
            }
        }
        let fake = Fake::default();
        let broker = BrokerFake {
            usage: Some(json!({"schema": "praxis-broker-usage/v1"})),
            ..BrokerFake::default()
        };
        assert!(preview(&fake, &broker, &preview_config()).lines[1].contains("no reading (held)"));
        let mut value = file();
        value.as_object_mut().unwrap().remove("usage");
        value.as_object_mut().unwrap().remove("pools");
        value["board"]["url"] = "https://forge.example/subpath".into();
        let broker = BrokerFake::default();
        let report = preview(&Fake::default(), &broker, &config(&value));
        assert!(report.lines[1].contains("no usage.url"));
        assert!(broker.calls.borrow().is_empty());
    }

    #[test]
    fn shared_bounds() {
        let config = preview_config();
        for (limit, deadline, expected) in [
            (
                PASS_REQUEST_LIMIT,
                Instant::now() - Duration::from_secs(1),
                0,
            ),
            (WRITE_RESERVE, Instant::now() + Duration::from_secs(60), 0),
            (
                WRITE_RESERVE + 1,
                Instant::now() + Duration::from_secs(60),
                1,
            ),
            (
                WRITE_RESERVE + 2,
                Instant::now() + Duration::from_secs(60),
                2,
            ),
        ] {
            let fake = Fake::default();
            let broker = BrokerFake::default();
            let bounds = Bounds::new(limit, deadline);
            let report = run_within(
                &fake,
                &broker,
                &config,
                (Mode::Preview, Limits::default()),
                usage::tests::now(),
                &bounds,
            )
            .unwrap();
            assert_eq!(fake.calls.borrow().len(), expected);
            assert!(broker.calls.borrow().is_empty());
            assert!(report.failures > 0);
            assert_eq!(report.spent.requests, expected);
        }
    }

    fn login(id: u64) -> &'static str {
        match id {
            BOT => "bot",
            OPERATOR => "operator",
            _ => "visitor",
        }
    }

    /// A listed open issue of example/work, source-shaped (Forgejo v17.0.1
    /// modules/structs/issue.go), with its poster's id and labels.
    fn issue(number: u64, poster: u64, labels: &[&str]) -> Value {
        let labels: Vec<Value> = (labels.iter())
            .map(|l| json!({"id": 1, "name": l, "exclusive": l.contains('/'), "color": "ee0701"}))
            .collect();
        json!({"id": 100 + number, "number": number, "title": format!("Issue {number}"),
            "body": "", "state": "open", "user": {"id": poster, "login": login(poster)},
            "labels": labels, "assignees": null, "comments": 0, "pull_request": null,
            "created_at": format!("2026-10-{:02}T10:00:00Z", 10 + number),
            "updated_at": "2026-10-22T10:00:00Z", "closed_at": null,
            "repository": {"id": 3, "name": "work", "owner": "example", "full_name": "example/work"}})
    }

    /// An open pull request of example/work (modules/structs/pull.go).
    fn pull(number: u64, poster: u64, body: &str) -> Value {
        json!({"id": 200 + number, "number": number, "title": "Change", "body": body,
            "state": "open", "user": {"id": poster, "login": login(poster)},
            "head": {"ref": "dispatch/change", "label": "dispatch/change"},
            "base": {"ref": "main", "label": "main"},
            "updated_at": "2026-10-23T09:00:00Z"})
    }

    /// A run of the caller (modules/structs/action.go), as the dispatch
    /// adapter's tests shape it.
    fn caller_run(id: u64, title: &str) -> Value {
        json!({"id": id, "index_in_repo": id - 40, "workflow_id": "agent.yml",
            "title": title, "status": "running", "prettyref": "main",
            "trigger_user": {"id": BOT}, "event": "",
            "repository": {"full_name": "example/control"}, "created": "2026-10-23T14:00:00Z",
            "html_url": "https://public.example/example/control/actions/runs/1"})
    }

    /// A Forgejo pass that schedules: `wip` slots, one pool, at `url`.
    fn scheduling(url: &str, wip: u64) -> Config {
        let mut value = file();
        value["board"]["url"] = url.into();
        value["wip"] = wip.into();
        value["workRepositories"] = json!(["example/work"]);
        value["pools"] = json!({"claude": {}});
        config(&value)
    }

    const URL: &str = "https://forge.example:8443/subpath";

    /// Stage 4's lines of a preview over `listed`.
    fn stage4(listed: Listed, config: &Config) -> (Vec<String>, Report) {
        let fake = Fake {
            listed,
            ..Fake::default()
        };
        let report = preview(&fake, &BrokerFake::default(), config);
        let lines = report
            .lines
            .iter()
            .skip_while(|l| !l.starts_with("stage 3 "));
        let lines = (lines.skip(1))
            .take_while(|l| !l.starts_with("stage 5 "))
            .cloned()
            .collect();
        (lines, report)
    }

    const CANDIDATE: &str = "candidates: 1 (0 opened by others, 0 outside a live epic)";

    #[test]
    fn schedule_lines() {
        let seven = |labels: &[&str]| issue(7, OPERATOR, labels);
        let held = |why: &str| format!("hold example/work#7: {why}");
        let start = "would start example/work#7 pool=claude".to_owned();
        let in_flight = caller_run(43, "implement example/work#7 (claude)");
        // One more than four pages of pull requests, and than ten of runs.
        let visitors: Vec<Value> = (1..=201).map(|n| pull(300 + n, 999, "Closes #7")).collect();
        let crowd: Vec<Value> = (100..601)
            .map(|id| caller_run(id, "triage example/work#8 (claude)"))
            .collect();
        let cases: Vec<(&str, Listed, u64, Vec<String>)> = vec![
            (
                "eligible",
                Listed {
                    issues: vec![seven(&["priority/P1"])],
                    ..Listed::default()
                },
                2,
                vec![CANDIDATE.into(), start.clone()],
            ),
            (
                "needs-human",
                Listed {
                    issues: vec![seven(&["needs-human"])],
                    ..Listed::default()
                },
                2,
                vec![CANDIDATE.into(), held("labelled needs-human")],
            ),
            (
                "needs-design",
                Listed {
                    issues: vec![seven(&["needs-design", "priority/P0"])],
                    ..Listed::default()
                },
                2,
                vec![CANDIDATE.into(), held("labelled needs-design")],
            ),
            (
                "an open dependency, and a closed one",
                Listed {
                    issues: vec![seven(&[])],
                    dependencies: [(7, vec![issue(3, 999, &[]), {
                        let mut closed = issue(4, 999, &[]);
                        closed["state"] = "closed".into();
                        closed
                    }])]
                    .into(),
                    ..Listed::default()
                },
                2,
                vec![CANDIDATE.into(), held("1 open dependencies")],
            ),
            (
                "the bot's closing pull request",
                Listed {
                    issues: vec![seven(&[])],
                    pulls: (vec![pull(12, BOT, "Fix things.\n\nCloses: #7")], 0),
                    ..Listed::default()
                },
                2,
                vec![
                    CANDIDATE.into(),
                    held("pull request example/work#12 is open"),
                ],
            ),
            (
                "the operator's pull request closing the issue by URL",
                Listed {
                    issues: vec![seven(&[])],
                    pulls: (
                        vec![pull(
                            13,
                            OPERATOR,
                            &format!("fixes {URL}/example/work/issues/7"),
                        )],
                        0,
                    ),
                    ..Listed::default()
                },
                2,
                vec![
                    CANDIDATE.into(),
                    held("pull request example/work#13 is open"),
                ],
            ),
            (
                "a visitor's, another issue's and a GitHub URL's pull requests",
                Listed {
                    issues: vec![seven(&[])],
                    pulls: (
                        vec![
                            pull(12, 999, "Closes #7"),
                            pull(14, BOT, "Part of #7"),
                            pull(15, BOT, "Closes #71"),
                            pull(16, BOT, "Closes https://github.com/example/work/issues/7"),
                        ],
                        0,
                    ),
                    ..Listed::default()
                },
                2,
                vec![CANDIDATE.into(), start.clone()],
            ),
            (
                "more pull requests than the page cap",
                Listed {
                    issues: vec![seven(&[])],
                    pulls: (visitors, 0),
                    ..Listed::default()
                },
                2,
                vec![
                    CANDIDATE.into(),
                    held("its pull requests could not all be read"),
                ],
            ),
            (
                "a run in flight",
                Listed {
                    issues: vec![seven(&[])],
                    runs: (vec![in_flight.clone()], 0),
                    ..Listed::default()
                },
                2,
                vec![
                    CANDIDATE.into(),
                    held(&format!(
                        "a run is in flight, {URL}/example/control/actions/runs/3"
                    )),
                ],
            ),
            (
                "a full WIP",
                Listed {
                    issues: vec![seven(&[])],
                    runs: (vec![caller_run(44, "triage example/work#8 (claude)")], 0),
                    ..Listed::default()
                },
                1,
                vec!["hold wip: 1 of 1 runs active".into()],
            ),
            (
                "more runs than the page cap (a failure)",
                Listed {
                    issues: vec![seven(&[])],
                    runs: (crowd, 0),
                    ..Listed::default()
                },
                2,
                vec![
                    "hold wip: the runs in flight could not all be listed: Actions listing total moved or exceeds cap; held; new runs wait"
                        .into(),
                ],
            ),
            (
                "opened by someone else",
                Listed {
                    issues: vec![issue(7, 999, &["priority/P0"])],
                    ..Listed::default()
                },
                2,
                vec!["candidates: 0 (1 opened by others, 0 outside a live epic)".into()],
            ),
            (
                "an epic, a question and an unreadable visitor's",
                Listed {
                    issues: vec![
                        issue(5, BOT, &["epic"]),
                        issue(6, BOT, &["question"]),
                        issue(8, 999, &["priority/P1", "priority/P2"]),
                    ],
                    ..Listed::default()
                },
                2,
                vec!["candidates: 0 (1 opened by others, 0 outside a live epic)".into()],
            ),
            (
                "two priority/ labels",
                Listed {
                    issues: vec![seven(&["priority/P1", "priority/P2"])],
                    ..Listed::default()
                },
                2,
                vec![
                    CANDIDATE.into(),
                    held("its Priority could not be read: 2 priority/ labels"),
                ],
            ),
            (
                "an unknown Priority",
                Listed {
                    issues: vec![seven(&["priority/P9"])],
                    ..Listed::default()
                },
                2,
                vec![
                    CANDIDATE.into(),
                    held("its Priority could not be read: unknown label \"priority/P9\""),
                ],
            ),
            (
                "ranked by Priority, then age, within WIP",
                Listed {
                    issues: vec![
                        issue(2, BOT, &["priority/P2"]),
                        issue(3, OPERATOR, &[]),
                        issue(9, OPERATOR, &["priority/P0"]),
                        issue(1, BOT, &["priority/P2"]),
                    ],
                    ..Listed::default()
                },
                2,
                vec![
                    "candidates: 4 (0 opened by others, 0 outside a live epic)".into(),
                    "would start example/work#9 pool=claude".into(),
                    "would start example/work#1 pool=claude".into(),
                    "wip: no slot left; 2 more candidate(s) wait".into(),
                ],
            ),
            (
                "dependencies off in the repository",
                Listed {
                    issues: vec![seven(&[])],
                    broken: "404 dependencies",
                    ..Listed::default()
                },
                2,
                vec![CANDIDATE.into(), start.clone()],
            ),
            (
                "more dependencies than the page cap",
                Listed {
                    issues: vec![seven(&[])],
                    dependencies: [(7, (1..=201).map(|n| issue(n, 999, &[])).collect())].into(),
                    ..Listed::default()
                },
                2,
                vec![
                    CANDIDATE.into(),
                    held("its dependencies could not all be read"),
                ],
            ),
            (
                "dependencies that fail (a failure)",
                Listed {
                    issues: vec![seven(&[])],
                    broken: "dependencies",
                    ..Listed::default()
                },
                2,
                vec![
                    CANDIDATE.into(),
                    held(
                        "its dependencies could not all be read: GET repos/example/work/issues/7/dependencies: Forgejo HTTP status 500",
                    ),
                ],
            ),
            (
                "pull requests that fail (a failure)",
                Listed {
                    issues: vec![seven(&[])],
                    broken: "pulls",
                    ..Listed::default()
                },
                2,
                vec![
                    CANDIDATE.into(),
                    held(
                        "its pull requests could not all be read: GET repos/example/work/pulls: Forgejo HTTP status 500",
                    ),
                ],
            ),
            (
                "runs that fail (a failure)",
                Listed {
                    issues: vec![seven(&[])],
                    broken: "runs",
                    ..Listed::default()
                },
                2,
                vec![
                    "hold wip: the runs in flight could not all be listed: GET repos/example/control/actions/runs: Forgejo HTTP status 500; new runs wait"
                        .into(),
                ],
            ),
        ];
        for (name, listed, wip, expected) in cases {
            let (lines, report) = stage4(listed, &scheduling(URL, wip));
            assert_eq!(lines, expected, "{name}");
            let failed = name.ends_with("(a failure)");
            assert_eq!(report.failures, usize::from(failed), "{name}");
        }
    }

    #[test]
    fn board_read_boundaries() {
        let edited = |edit: fn(&mut Value)| {
            let mut issue = issue(7, OPERATOR, &[]);
            edit(&mut issue);
            vec![issue]
        };
        let twice = vec![issue(7, OPERATOR, &[]), issue(7, BOT, &[])];
        let many: Vec<Value> = (1..=201).map(|n| issue(n, BOT, &[])).collect();
        for (issues, error) in [
            (
                edited(|i| i["pull_request"] = json!({"merged": false})),
                "example/work#7 is a pull request",
            ),
            (
                edited(|i| i["repository"]["full_name"] = "other/work".into()),
                "example/work#7 reported from \"other/work\"",
            ),
            (edited(|i| i["state"] = "closed".into()), "is listed open"),
            (twice, "example/work#7 listed twice"),
            (many, "could not all be read in 4 pages"),
        ] {
            let listed = Listed {
                issues,
                ..Listed::default()
            };
            let (lines, report) = stage4(listed, &scheduling(URL, 2));
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert!(lines[0].starts_with("schedule error: "), "{lines:?}");
            assert!(lines[0].contains(error), "{lines:?}");
            assert_eq!(report.failures, 1);
        }
        // The membership label filters the listing; `status/blocked` is
        // Blocked only with `statusLabels`, and holds before any read.
        for (status_labels, expected) in [
            (true, "hold example/work#7: board Status is Blocked"),
            (false, "would start example/work#7 pool=claude"),
        ] {
            let fake = Fake {
                listed: Listed {
                    issues: vec![issue(7, OPERATOR, &["status/blocked"])],
                    ..Listed::default()
                },
                ..Fake::default()
            };
            let mut value = file();
            value["board"]["url"] = URL.into();
            value["board"]["statusLabels"] = status_labels.into();
            value["wip"] = 2.into();
            value["workRepositories"] = json!(["example/work"]);
            value["pools"] = json!({"claude": {}});
            let report = preview(&fake, &BrokerFake::default(), &config(&value));
            assert!(
                report.lines.contains(&expected.into()),
                "{:?}",
                report.lines
            );
            let calls = fake.calls.borrow();
            let listing = format!(
                "{URL}/api/v1/repos/example/work/issues?type=issues&state=open&labels=agent-board&limit=50&page=1"
            );
            assert!(calls.contains(&listing), "{calls:?}");
            let read = |path: &str| calls.iter().any(|c| c.contains(path));
            assert_eq!(read("/dependencies"), !status_labels, "{calls:?}");
        }
    }

    #[test]
    fn pools_over_pace_and_max_dispatch_hold_starts() {
        let listed = || Listed {
            issues: vec![issue(7, OPERATOR, &[])],
            ..Listed::default()
        };
        let mut paced = file();
        paced["board"]["url"] = URL.into();
        paced["wip"] = 2.into();
        paced["workRepositories"] = json!(["example/work"]);
        paced["pools"] = json!({"claude": {"target": 0.01, "burst": 0}});
        let (lines, _) = stage4(listed(), &config(&paced));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("hold pools: claude is held")
                && lines[0].ends_with("; new runs wait"),
            "{lines:?}"
        );
        let fake = Fake {
            listed: listed(),
            ..Fake::default()
        };
        let limits = Limits {
            dispatch: Some(0),
            ..Limits::default()
        };
        let report = run(
            &fake,
            &BrokerFake::default(),
            &scheduling(URL, 2),
            (Mode::Preview, limits),
            usage::tests::now(),
        )
        .unwrap();
        assert!(
            report
                .lines
                .contains(&"held by --max-dispatch: would start example/work#7 pool=claude".into()),
            "{:?}",
            report.lines
        );
        assert!(
            report.summary.starts_with("summary: dispatched 0/0"),
            "{}",
            report.summary
        );
    }

    #[test]
    fn two_instances_share_no_keys_or_requests() {
        let mut keys = BTreeSet::new();
        for url in [
            "https://one.example/subpath",
            "https://two.example:8443/subpath",
        ] {
            let fake = Fake {
                listed: Listed {
                    issues: vec![issue(7, OPERATOR, &["needs-human"])],
                    runs: (vec![caller_run(43, "implement example/work#7 (claude)")], 0),
                    ..Listed::default()
                },
                ..Fake::default()
            };
            let report = preview(&fake, &BrokerFake::default(), &scheduling(url, 2));
            let calls = fake.calls.borrow();
            assert!(calls.len() > 2);
            assert!(
                calls
                    .iter()
                    .all(|c| c.starts_with(&format!("{url}/api/v1/"))),
                "{calls:?}"
            );
            let [entry] = &report.holds[..] else {
                panic!("{:?}", report.holds);
            };
            let Scope::Item(item) = &entry.scope else {
                panic!("{entry:?}");
            };
            assert_eq!(item.host, url);
            assert_eq!(key(item), format!("{url}/example/work#7"));
            assert!(keys.insert(key(item)));
        }
        assert_eq!(keys.len(), 2);
    }

    /// A recording of a stage 4 preview at `url`, as JSON.
    fn recording(url: &str) -> Value {
        let fake = Fake {
            listed: Listed {
                issues: vec![issue(7, OPERATOR, &[])],
                ..Listed::default()
            },
            ..Fake::default()
        };
        let broker = BrokerFake::default();
        let config = scheduling(url, 2);
        let recorder = Recorder::new(&fake, &broker, config.forgejo_board().unwrap());
        let preview = (Mode::Preview, Limits::default());
        run(&recorder, &recorder, &config, preview, usage::tests::now()).unwrap();
        // Neither writes: the fake panics on one.
        let target = url::Url::parse(&format!("{url}/api/v1/repos/example/work/labels")).unwrap();
        assert!(recorder.send("POST", &target, &json!({})).is_err());
        assert!(recorder.write("POST", &target, &json!({})).is_err());
        serde_json::from_str(&recorder.fixture(usage::tests::now()).unwrap()).unwrap()
    }

    #[test]
    fn replay_fails_closed() {
        const OTHER: &str = "https://two.example:8443/subpath";
        let fixture = recording(URL);
        let requests = fixture["requests"].as_array().unwrap().len();
        let config = scheduling(URL, 2);
        let board = config.forgejo_board().unwrap();
        // Another instance with the same item names, a GitHub recording, or
        // a Forgejo one under a GitHub replay, is refused before any read.
        let other = scheduling(OTHER, 2);
        for (fixture, board, error) in [
            (&fixture, other.forgejo_board().unwrap(), "recorded on"),
            (
                &json!({"schema": REPLAY_SCHEMA, "now": "2026-10-23T00:00:00Z", "requests": []}),
                board,
                "not a Forgejo pass replay",
            ),
        ] {
            let refused = Replay::from_json(fixture, board).err().unwrap();
            assert!(format!("{refused:#}").contains(error), "{refused:#}");
        }
        assert!(super::super::Replay::from_json(&fixture).is_err());
        let recorded = recording(OTHER);
        assert_ne!(recorded["requests"], fixture["requests"]);
        // A response missing, a request changed, or one left unread.
        let edited = |edit: &dyn Fn(&mut Vec<Value>)| {
            let mut fixture = fixture.clone();
            edit(fixture["requests"].as_array_mut().unwrap());
            Replay::from_json(&fixture, board).unwrap()
        };
        for (replay, reads) in [
            (edited(&|r| r.truncate(requests - 1)), "did not record"),
            (
                edited(&|r| r[2]["request"]["get"] = format!("{URL}/api/v1/user").into()),
                "recorded",
            ),
            (edited(&|r| r.push(r[0].clone())), ""),
        ] {
            let report = run(
                &replay,
                &replay,
                &config,
                (Mode::Preview, Limits::default()),
                replay.now,
            )
            .unwrap();
            let finished = replay.finish().unwrap_err().to_string();
            if reads.is_empty() {
                assert_eq!(report.failures, 0);
                assert_eq!(
                    finished,
                    format!(
                        "the pass read {requests} of the replay's {} requests",
                        requests + 1
                    )
                );
            } else {
                assert!(report.failures > 0, "{:?}", report.lines);
                assert!(
                    report.lines.iter().any(|l| l.contains(reads)),
                    "{:?}",
                    report.lines
                );
                assert!(
                    finished.contains("diverged") || finished.contains("read"),
                    "{finished}"
                );
            }
        }
        // Writes are refused, never answered.
        let replay = Replay::from_json(&fixture, board).unwrap();
        let target = url::Url::parse(&format!("{URL}/api/v1/repos/example/work/labels")).unwrap();
        assert!(replay.write("POST", &target, &json!({})).is_err());
        assert!(replay.send("POST", &target, &json!({})).is_err());
    }

    #[test]
    fn recording_keeps_order_status_and_totals() {
        let fixture = recording(URL);
        assert_eq!(fixture["forge"], "forgejo");
        assert_eq!(fixture["instance"], URL);
        let requests: Vec<String> = (fixture["requests"].as_array().unwrap().iter())
            .map(|e| e["request"].to_string())
            .collect();
        assert!(requests[0].contains("/api/v1/user"), "{requests:?}");
        assert!(
            requests[1].contains("/api/v1/users/operator"),
            "{requests:?}"
        );
        assert_eq!(requests[2], r#"{"broker":"/usage"}"#);
        let issues = (fixture["requests"].as_array().unwrap().iter())
            .find(|e| {
                e["request"]["get"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("/issues?")
            })
            .unwrap();
        assert_eq!(issues["answer"]["total"], 1);
        let text = fixture.to_string();
        assert!(!text.contains("uthorization"), "{text}");
    }
}
