//! Leases (agent-board#382): a slot of the runs' capacity that a run takes
//! before any inference, whoever starts it, so that no path past the pass
//! skips its WIP limit and pacing. A lease is the ref
//! `refs/agent-board/leases/SLOT` in `dispatchRepo`, at a commit on a tree
//! of one fixed file whose message is the lease's JSON. Every pool shares the slots, so
//! `wip` bounds them all together, and there are at most [`LEASE_SLOTS`].
//! A ref is never deleted: taking a slot creates its ref, which GitHub
//! refuses while it exists, or moves it to a child of the commit read,
//! which GitHub refuses unless it is a fast forward; giving one back moves
//! it likewise to a commit that is no lease. So of two writers that read the
//! same commit exactly one moves it, and a stale reap cannot free a slot
//! taken since. `agent-board admit` takes one, `agent-board release` gives
//! it back, and the pass reaps a lease whose run has ended or that has
//! expired.
use super::*;
use serde::Serialize;

/// The leases' refs, under `refs/`.
pub const LEASE_REFS: &str = "agent-board/leases";
/// How long a lease holds its slot unless released, or its run ends, first.
pub const LEASE_SECONDS: i64 = 24 * 60 * 60;
/// The slots there are, the most `wip` and the operator's `--override` may
/// take; a ref past them is no slot. They are read at once: more refs is an
/// error, as capacity read from some of them would be guessed.
const LEASE_SLOTS: u64 = WIP_LIMIT;
/// The one file of every lease commit's tree, and its text.
const TREE_FILE: &str = "README";
const TREE_TEXT: &str = "An agent-board lease: the commit message is the lease.\n";
/// What GitHub's 422 says when a ref to be created exists.
const TAKEN: &str = "Reference already exists";
/// What GitHub's 422 says when a ref to be moved is not where it was read,
/// or is gone.
const MOVED: [&str; 2] = ["Update is not a fast forward", "Reference does not exist"];

/// A lease commit's message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lease {
    /// The run that holds it, in `dispatchRepo`; zero reserves a dispatch
    /// whose run is not yet known. Such a reservation holds until bound
    /// to the dispatch response's run or expired, including ambiguous starts.
    pub run: u64,
    /// `OWNER/REPO#N`.
    pub item: String,
    pub pool: String,
    pub taken_at: String,
    pub expires_at: String,
    /// Taken with the operator's `--override`, past `wip` and pacing.
    #[serde(default, rename = "override", skip_serializing_if = "is_false")]
    pub overridden: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// A lease with its slot, as the hold snapshot lists it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Held {
    pub slot: u64,
    #[serde(flatten)]
    pub lease: Lease,
}

/// Merge only validated issue identities, normalized like workflow titles.
/// Invalid lease data must not silently guess which candidate is occupied.
pub(super) fn busy(held: &[Held], busy: &mut BTreeMap<String, String>) -> Result<()> {
    for h in held {
        let item =
            crate::github::parse_github_issue(&h.lease.item, None, crate::github::DEFAULT_HOST)
                .context("lease has an invalid item")?;
        busy.entry(key(&item))
            .or_insert_with(|| format!("leased by run {}", h.lease.run));
    }
    Ok(())
}

/// A lease ref as read: its name under `refs/`, its commit, its slot where
/// the name is one, its lease where it holds one, and why it may be reaped
/// where it may. A slot with no lease is free.
pub struct Found {
    pub name: String,
    sha: String,
    slot: Option<u64>,
    pub held: Option<Held>,
    pub ended: Option<String>,
}

impl Found {
    /// Whether it holds a lease that has not ended.
    fn live(&self) -> bool {
        self.held.is_some() && self.ended.is_none()
    }
}

/// What `admit` asks for.
pub struct Request<'a> {
    pub pool: &'a str,
    pub item: &'a ItemRef,
    pub run: u64,
    /// `--override`: the operator's alone.
    pub overridden: bool,
}

fn stamp(at: OffsetDateTime) -> Result<String> {
    Ok(at.format(&Rfc3339)?)
}

/// The refs under [`LEASE_REFS`] in `repo`: each name under `refs/` and its
/// commit.
fn refs(client: &impl Rest, repo: &str) -> Result<Vec<(String, String)>> {
    let url = format!("{API_ROOT}repos/{repo}/git/matching-refs/{LEASE_REFS}/");
    let response = client.get(&url, None)?;
    ensure!(
        response.status == 200,
        "listing the leases of {repo}: GitHub HTTP status {}",
        response.status
    );
    let list = response.body.as_array().context("refs are not a list")?;
    ensure!(
        list.len() as u64 <= LEASE_SLOTS && response.link.as_deref().and_then(next_link).is_none(),
        "{repo} has more than {LEASE_SLOTS} lease refs; capacity is unknown"
    );
    list.iter()
        .map(|r| {
            let name = text(r, "/ref")
                .strip_prefix("refs/")
                .context("a ref has no name")?;
            let sha = text(r, "/object/sha");
            ensure!(!sha.is_empty(), "ref {name} has no commit");
            Ok((name.to_owned(), sha.to_owned()))
        })
        .collect()
}

/// Why run `id` of `repo` has ended at `now`, or `None` while it runs or
/// cannot be read: completed, or with every job finished ([`jobs_ended`]).
/// A run not found, deleted or never `repo`'s, is no evidence: its lease
/// holds until it expires.
fn run_ended(
    client: &impl Rest,
    repo: &str,
    id: u64,
    now: OffsetDateTime,
) -> Result<Option<String>> {
    let response = client.get(&format!("{API_ROOT}repos/{repo}/actions/runs/{id}"), None)?;
    let run = &response.body;
    Ok(match response.status {
        200 if run["status"] == "completed" => Some(format!(
            "its run {id} ended {}",
            run["conclusion"].as_str().unwrap_or("with no conclusion")
        )),
        200 if jobs_ended(client, repo, run, now) => {
            Some(format!("its run {id}'s jobs have all finished"))
        }
        200 | 404 => None,
        status => bail!("reading run {id}: GitHub HTTP status {status}"),
    })
}

/// Every lease ref in `repo` at `now`. A commit that is no lease holds
/// nothing, and its slot, where its name is one, is free; a lease past its
/// `expires_at` or whose run has ended may be reaped. A run that cannot be
/// read leaves its lease in place.
pub fn read(client: &impl Rest, repo: &str, now: OffsetDateTime) -> Result<Vec<Found>> {
    let mut found = Vec::new();
    for (name, sha) in refs(client, repo)? {
        let slot = (name.strip_prefix(&format!("{LEASE_REFS}/")))
            .and_then(|s| s.parse().ok())
            .filter(|&s: &u64| s < LEASE_SLOTS && name == format!("{LEASE_REFS}/{s}"));
        let url = format!("{API_ROOT}repos/{repo}/git/commits/{sha}");
        let response = client.get(&url, None)?;
        ensure!(
            response.status == 200,
            "reading lease {name}: GitHub HTTP status {}",
            response.status
        );
        let lease = serde_json::from_str::<Lease>(text(&response.body, "/message"));
        let held = match (lease, slot) {
            (Ok(lease), Some(slot)) => Some(Held { slot, lease }),
            _ => None,
        };
        let ended = match &held {
            None => None,
            Some(held) => match at(&Value::from(held.lease.expires_at.as_str())) {
                None => Some("its expiry is not a time".to_owned()),
                Some(expires) if expires <= now => {
                    Some(format!("expired {}", held.lease.expires_at))
                }
                Some(_) if held.lease.run == 0 => None,
                Some(_) => run_ended(client, repo, held.lease.run, now).unwrap_or(None),
            },
        };
        found.push(Found {
            name,
            sha,
            slot,
            held,
            ended,
        });
    }
    Ok(found)
}

/// Points ref `name` of `repo` at a new commit with `message`: created where
/// `parent` is `None`, otherwise moved from `parent` by a fast forward.
/// Whether it was; not where another write created or moved it first.
fn write(
    client: &impl Rest,
    repo: &str,
    (name, parent): (&str, Option<&str>),
    message: &str,
) -> Result<bool> {
    // Lease messages name potentially private work items and remain in Git
    // history after release. Refuse before creating even an unreachable
    // object in a public or unreadable destination.
    let response = client.get(&format!("{API_ROOT}repos/{repo}"), None)?;
    ensure!(
        response.status == 200 && response.body["private"] == true,
        "{repo} is not known to be private; no lease objects are written there"
    );
    let git = format!("{API_ROOT}repos/{repo}/git");
    let trees = format!("{git}/trees");
    // GitHub refuses to create an empty tree.
    let entry = json!({"path": TREE_FILE, "mode": "100644", "type": "blob", "content": TREE_TEXT});
    let (_, tree) = plan::send(client, "POST", &trees, &json!({"tree": [entry]}), 0)?;
    let parents: Vec<&str> = parent.into_iter().collect();
    let body = json!({"message": message, "tree": text(&tree, "/sha"), "parents": parents});
    let (_, commit) = plan::send(client, "POST", &format!("{git}/commits"), &body, 0)?;
    let sha = text(&commit, "/sha");
    let (status, answer) = match parent {
        None => {
            let body = json!({"ref": format!("refs/{name}"), "sha": sha});
            client.send("POST", &format!("{git}/refs"), &body)?
        }
        Some(_) => {
            let body = json!({"sha": sha, "force": false});
            client.send("PATCH", &format!("{git}/refs/{name}"), &body)?
        }
    };
    let said = text(&answer, "/message");
    match status {
        200 | 201 => Ok(true),
        422 if said.contains(TAKEN) || MOVED.iter().any(|m| said.contains(m)) => Ok(false),
        status => bail!("writing {name}: GitHub HTTP status {status}: {said}"),
    }
}

/// Frees `found`'s slot, saying `why`, unless its ref moved since it was
/// read: whether it did.
pub fn free(client: &impl Rest, repo: &str, found: &Found, why: &str) -> Result<bool> {
    write(client, repo, (&found.name, Some(&found.sha)), why)
}

/// How many leases there may be at `now` for a run on `pool`: `wip` while
/// pacing, by the published usage `snapshot` as the pass reads it ([`room`],
/// its holds, the five-hour window's included) and the weekly budget, lets
/// it take a run; or why none.
fn slots(
    config: &Config,
    snapshot: Option<&Value>,
    (pool, settings): (&str, &PoolSettings),
    now: OffsetDateTime,
) -> std::result::Result<(u64, bool), String> {
    if config.wip == 0 {
        return Err("wip is 0".into());
    }
    let probe = match room(snapshot, pool, settings, now) {
        Ok(_) => false,
        Err(why) => {
            let allowed = config.usage.as_ref().is_some_and(|u| u.probe)
                && fresh(snapshot, now).is_ok_and(|s| {
                    s["pools"][pool]["reason"] == usage::NO_READING
                        && s["probes"][pool].as_str().is_some()
                });
            if !allowed {
                return Err(format!("pacing holds {pool}: {why}"));
            }
            true
        }
    };
    match budget_hold(config, snapshot, now, 1.0) {
        Some(why) => Err(format!("budget: {why}")),
        None => Ok((config.wip, probe)),
    }
}

/// Active caller runs also occupy WIP before caller-side admission is
/// wired, or while a caller is waiting to admit. Read the whole listing or
/// refuse: a truncated workflow list cannot establish free capacity.
fn unleased(
    client: &impl Rest,
    config: &Config,
    found: &[Found],
    own: u64,
    now: OffsetDateTime,
) -> Result<usize> {
    let leased: BTreeSet<u64> = found
        .iter()
        .filter(|f| f.live())
        .filter_map(|f| f.held.as_ref().map(|h| h.lease.run))
        .collect();
    let mut active = BTreeSet::new();
    for status in ACTIVE_RUNS {
        let url = format!(
            "{API_ROOT}repos/{}/actions/workflows/{}/runs?status={status}&per_page={RUNS_PER_PAGE}",
            config.dispatch_repo, config.dispatch_workflow
        );
        let response = client.get(&url, None)?;
        ensure!(
            response.status == 200,
            "listing caller runs for admission: GitHub HTTP status {}",
            response.status
        );
        let runs = response.body["workflow_runs"]
            .as_array()
            .context("caller run list has no workflow_runs")?;
        ensure!(
            response.link.as_deref().and_then(next_link).is_none()
                && runs.len() < RUNS_PER_PAGE as usize
                && response.body["total_count"]
                    .as_u64()
                    .is_some_and(|n| n <= RUNS_PER_PAGE),
            "caller run list may be incomplete; admission capacity is unknown"
        );
        for run in runs {
            let id = run["id"].as_u64().context("caller run has no id")?;
            if id != own
                && !leased.contains(&id)
                && !jobs_ended(client, &config.dispatch_repo, run, now)
            {
                active.insert(id);
            }
        }
    }
    Ok(active.len())
}

/// Takes a lease for `request` in `dispatchRepo` at `now`: its slot, or why
/// there is none. A run that holds a lease of the pool already gets it back,
/// so a retry after a lost answer takes no second. `lines` gets that, a
/// lease it reaped to take its slot, and an override. `--override` is
/// refused unless the token is the operator's.
pub fn admit(
    client: &impl Rest,
    config: &Config,
    request: &Request,
    now: OffsetDateTime,
    lines: &mut Vec<String>,
) -> Result<std::result::Result<Held, String>> {
    let pool = request.pool;
    let settings = (config.pools.0.iter())
        .find(|(name, _)| name == pool)
        .map(|(_, settings)| settings)
        .with_context(|| format!("{pool} is not in pools"))?;
    if request.overridden {
        let account = client.get(&format!("{API_ROOT}user"), None)?;
        let login = text(&account.body, "/login");
        ensure!(
            account.status == 200 && login.eq_ignore_ascii_case(&config.operator),
            "--override is {}'s alone; the token is {login:?}'s",
            config.operator
        );
        lines.push(format!("admit: override by {login}, past wip and pacing"));
    }
    let repo = &config.dispatch_repo;
    let found = read(client, repo, now)?;
    let own = (found.iter().filter(|f| f.live()))
        .filter_map(|f| f.held.as_ref())
        .find(|h| request.run != 0 && h.lease.run == request.run && h.lease.pool == pool);
    if let Some(held) = own {
        lines.push(format!("admit: run {} holds it already", request.run));
        return Ok(Ok(held.clone()));
    }
    // An item's identity alone cannot prove that this is the dispatched
    // run: a manual dispatch could race it. Only the dispatch response may
    // bind a reservation. The caller retries while that response is pending.
    if found
        .iter()
        .filter(|f| f.live())
        .filter_map(|f| f.held.as_ref())
        .any(|h| h.lease.run == 0 && h.lease.item.eq_ignore_ascii_case(&request.item.display()))
    {
        return Ok(Err("dispatch reservation pending; retry admission".into()));
    }
    let (mut count, probe) = match request.overridden {
        true => (LEASE_SLOTS, false),
        false => {
            let snapshot = published_usage(client, config)?;
            match slots(config, snapshot.as_ref(), (pool, settings), now) {
                Ok(capacity) => capacity,
                Err(why) => return Ok(Err(why)),
            }
        }
    };
    if !request.overridden {
        let unleased = unleased(client, config, &found, request.run, now)? as u64;
        count = if probe && unleased > 0 {
            0
        } else {
            count.saturating_sub(unleased)
        };
    }
    // Leases are counted, not their slots: one past `count`, the operator's
    // or from a higher `wip`, takes a slot too.
    let live: BTreeSet<u64> = (found.iter().filter(|f| f.live()))
        .filter_map(|f| f.slot)
        .collect();
    let left = if probe && !live.is_empty() {
        0
    } else {
        count.saturating_sub(live.len() as u64) as usize
    };
    let lease = Lease {
        run: request.run,
        item: request.item.display(),
        pool: pool.to_owned(),
        taken_at: stamp(now)?,
        expires_at: stamp(now + time::Duration::seconds(LEASE_SECONDS))?,
        overridden: request.overridden,
    };
    let message = serde_json::to_string(&lease)?;
    // All cold probes contend for slot zero, across pools/providers and
    // passes. This deliberately conservative bound needs no second lock:
    // a stale published zero-request reading cannot admit parallel probes.
    if let Some(slot) = (0..LEASE_SLOTS)
        .filter(|s| !live.contains(s) && (!probe || *s == 0))
        .take(left)
        .next()
    {
        let name = format!("{LEASE_REFS}/{slot}");
        let was = found.iter().find(|f| f.slot == Some(slot));
        let parent = was.map(|f| f.sha.as_str());
        // Not taken where another run took it first.
        if write(client, repo, (&name, parent), &message)? {
            if let Some(why) = was.and_then(|f| f.ended.as_ref()) {
                lines.push(format!("reap {name}: {why}"));
            }
            let held = Held { slot, lease };
            if request.run == 0 {
                return Ok(Ok(held));
            }
            return Ok(Ok(deduplicate(client, repo, request, now, held)?));
        }
        // A concurrent retry may have won this slot for the same run.
        // Re-read ownership on the next invocation, never take a second.
    }
    Ok(Err(format!("all {count} slot(s) are leased")))
}

/// The run's lease of the pool once `admit` has written one: its lowest
/// slot, the others freed. Retries of one run racing while another slot
/// turns over may each take a different slot, and no write arbitrates
/// between slots; the later writer reads both here, so the run keeps one.
fn deduplicate(
    client: &impl Rest,
    repo: &str,
    request: &Request,
    now: OffsetDateTime,
    written: Held,
) -> Result<Held> {
    let found = read(client, repo, now)?;
    let mut own = (found.iter().filter(|f| f.live()))
        .filter(|f| {
            f.held
                .as_ref()
                .is_some_and(|h| h.lease.run == request.run && h.lease.pool == request.pool)
        })
        .collect::<Vec<_>>();
    own.sort_by_key(|f| f.slot);
    // Gone already, as where its run ended, it is still the one written.
    let Some((keep, rest)) = own.split_first() else {
        return Ok(written);
    };
    for extra in rest {
        // Moved since it was read, another write has freed it already.
        free(
            client,
            repo,
            extra,
            &format!("a second lease of run {}", request.run),
        )?;
    }
    Ok(keep.held.clone().unwrap_or(written))
}

/// Dispatch with the same atomic slot claim as `admit`. Kept at the actual
/// send boundary so a peer admission after the pass's recheck still races
/// for the slot, not for a cached capacity count. On an ambiguous answer the
/// reservation stays. Only a named dispatch response can bind it to a run,
/// or an accepted rerun of `rerun`, the run it restarts.
pub(super) struct Dispatch<'a, C, D> {
    pub client: &'a C,
    pub dispatcher: &'a D,
    pub config: &'a Config,
    pub item: &'a ItemRef,
    pub pool: &'a str,
    pub now: OffsetDateTime,
    pub sent: Cell<bool>,
    pub reserved: RefCell<Option<Held>>,
    pub rerun: Option<u64>,
}

impl<C: Rest, D: Rest> Rest for Dispatch<'_, C, D> {
    fn get(&self, url: &str, condition: Option<(&str, &str)>) -> Result<RestResponse> {
        self.dispatcher.get(url, condition)
    }

    fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        if !self.config.leases {
            return self.dispatcher.send(method, url, body);
        }
        if self.reserved.borrow().is_none() {
            let request = Request {
                pool: self.pool,
                item: self.item,
                run: 0,
                overridden: false,
            };
            let held = admit(
                self.client,
                self.config,
                &request,
                self.now,
                &mut Vec::new(),
            )?
            // Nothing was sent, so the plan's records are released.
            .map_err(|why| plan::Refused(format!("lease admission: {why}")))?;
            *self.reserved.borrow_mut() = Some(held);
        }
        self.sent.set(true);
        let answer = self.dispatcher.send(method, url, body)?;
        if let Some(run) = (answer.1["workflow_run_id"].as_u64())
            .or(self.rerun)
            .filter(|_| answer.0 < 300)
        {
            let reserved = self.reserved.borrow();
            let held = reserved.as_ref().context("dispatch has no reservation")?;
            for found in read(self.client, &self.config.dispatch_repo, self.now)? {
                if found.held.as_ref() == Some(held) {
                    let mut bound = held.lease.clone();
                    bound.run = run;
                    // A caller may have adopted it already. Never overwrite
                    // a new generation, even if the answer arrives late.
                    write(
                        self.client,
                        &self.config.dispatch_repo,
                        (&found.name, Some(&found.sha)),
                        &serde_json::to_string(&bound)?,
                    )?;
                }
            }
        }
        Ok(answer)
    }
}

/// Frees run `run`'s leases of `pool` in `repo`: a line for each, or one
/// saying it held none.
pub fn release(
    client: &impl Rest,
    repo: &str,
    (pool, run): (&str, u64),
    now: OffsetDateTime,
) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for found in read(client, repo, now)? {
        let own = (found.held.as_ref()).is_some_and(|h| h.lease.run == run && h.lease.pool == pool);
        if own && free(client, repo, &found, &format!("released by run {run}"))? {
            lines.push(format!("release {}", found.name));
        }
    }
    if lines.is_empty() {
        lines.push(format!("release: run {run} holds no lease of {pool}"));
    }
    Ok(lines)
}

#[cfg(test)]
pub(super) mod tests;
