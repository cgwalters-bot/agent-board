//! Preview-only Forgejo pass. Identity precedes broker reads; stage 4 reads
//! the board, pull requests and runs in flight and says what it would start;
//! no publication, GitHub fallback, adapter activation or dispatch is
//! reachable here.
use super::*;
use crate::forgejo::{
    Config as ForgejoConfig, Transport as ForgejoTransport, listing, pages, pages_to_end,
};

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
        ("epics", "epic driver #514"),
        ("holds", "scheduler and hold publication #513"),
        ("usage.repository", "usage publication in scheduler #513"),
        ("usage.issue", "usage publication in scheduler #513"),
        ("usage.probe", "run inventory in scheduler #513"),
        ("sweep", "a follow-up to scheduler #513 for sweep ownership"),
        (
            "revisit",
            "a follow-up to scheduler #513 for revisit ownership",
        ),
        ("leases", "lease wiring in scheduler #513"),
        ("reruns", "run wiring in scheduler #513"),
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
    ensure!(
        matches!(mode, Mode::Preview),
        "unsupported_capability: --apply: Forgejo scheduling needs #513"
    );
    config.check()?;
    let board = config
        .forgejo_board()
        .context("Forgejo pass needs a Forgejo board")?;
    let client = Budget {
        inner: client,
        bounds,
    };
    let caps = Caps::new(limits);
    let mut report = Report {
        summary: caps.summary(),
        ..Report::default()
    };
    report.tally.0.limit = config.wip;
    report.tally.1.mode = "preview";
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
    for (stage, name) in [(2, "follow pull requests"), (3, "epics")] {
        report
            .lines
            .push(format!("stage {stage} {name}: not built on Forgejo yet"));
    }
    bounds.enter(Stage::Schedule);
    let mut schedule = Schedule {
        client: &client,
        board,
        config,
        ids,
        now,
        caps: &caps,
        report: &mut report,
        pulls: BTreeMap::new(),
    };
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
    config: &'a Config,
    ids: Ids,
    now: OffsetDateTime,
    caps: &'a Caps,
    report: &'a mut Report,
    /// Each work repository's open pull requests, read once, and whether
    /// they were all read; or why they were not.
    pulls: BTreeMap<String, std::result::Result<(Vec<Value>, bool), String>>,
}

/// A board issue read, and why its Priority could not be, where it could not.
type Read = (Facts, Option<String>);

impl<C: ForgejoTransport> Schedule<'_, C> {
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
        if !config.work() {
            self.line("schedule: off (wip 0 or no workRepositories)".into());
            return Ok(());
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
        let facts = Facts {
            issue: ItemRef {
                host: host.into(),
                repository: repository.into(),
                number,
            },
            open: true,
            created,
            updated: None,
            author: self.trusted(&issue["user"]),
            labels,
            labels_complete: true,
            blocked_by: 0,
            pulls: Vec::new(),
            pulls_complete: true,
            // Epic children are #514's: every issue is unparented here.
            parent: None,
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
                // No parent is read on Forgejo yet.
                Screen::Child(_) => {
                    let why = "its parent is not read on Forgejo yet (#514)";
                    self.held(facts, Hold::new(HoldKind::Unreadable, why));
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
        for (option, follow_up) in [
            ("epics.repository", "#514"),
            ("holds.issue", "#513"),
            ("usage.repository", "#513"),
            ("usage.issue", "#513"),
            ("usage.probe", "#513"),
            ("sweep.max", "sweep ownership"),
            ("revisit.perPass", "revisit ownership"),
            ("leases", "#513"),
            ("reruns", "#513"),
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

    fn preview(fake: &Fake, broker: &BrokerFake, config: &Config) -> Report {
        run(
            fake,
            broker,
            config,
            (Mode::Preview, Limits::default()),
            usage::tests::now(),
        )
        .unwrap()
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
                "stage 3 epics: not built on Forgejo yet",
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
    fn shared_bounds_and_apply_refusal() {
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
        let fake = Fake::default();
        assert!(
            run(
                &fake,
                &BrokerFake::default(),
                &config,
                (Mode::Local, Limits::default()),
                usage::tests::now()
            )
            .is_err()
        );
        assert!(fake.calls.borrow().is_empty());
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
            "trigger_user": {"id": BOT}, "event": "workflow_dispatch",
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
}
