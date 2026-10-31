//! The weekly sweep on Forgejo (docs/forges.md#the-weekly-sweep): the
//! GitHub sweep's week, list, pacing and verdicts ([`shared`]), over records
//! Forgejo can vouch for. Forgejo names no last editor of an issue body or a
//! comment, so the sweep issue's body is only for people: its list and due
//! time are the bot's earliest unedited list comment, written before its run
//! starts; its run is a bot triage plan claimed and started as stage 4
//! starts any plan; and its reply is applied under an appended claim and one
//! receipt, attaching issues by stage 3's relationship comments. A claim
//! without its receipt is recovered from both sides of each relationship as
//! they are now, and nothing it cannot prove is sent again.
use super::*;
use crate::epic::IssueRef;
use crate::forgejo::epics as relations;
use crate::forgejo::{NotFound, RECEIPT_MARKER, SWEEP_APPLY_MARKER};
use crate::pass::sweep::{self as shared, Found, Verdict};
use crate::plan::JOB_PLAN_SCHEMA;

/// The first line of the bot's list record on a sweep issue, naming it.
const LIST_MARKER: &str = "agent-board-sweep-list";
/// The fence of an apply's receipt.
const APPLIED_FENCE: &str = "agent-board-sweep-applied";
const APPLIED_SCHEMA: &str = "agent-board/sweep-applied/v1";
/// Requests one write may take with its reads ([`Forge::board_write`]).
const WRITE_REQUESTS: usize = 12;

/// An issue's label names.
fn names(issue: &Value) -> Result<Vec<String>> {
    (issue["labels"].as_array().context("issue has no labels")?)
        .iter()
        .map(|l| l["name"].as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .context("a label has no name")
}

fn first_line(body: &str) -> &str {
    body.lines().next().unwrap_or_default().trim_end()
}

/// The marker value of `body`'s first line, `<!-- MARKER: VALUE -->`.
fn marked<'b>(body: &'b str, marker: &str) -> Option<&'b str> {
    first_line(body)
        .strip_prefix(&format!("<!-- {marker}: "))?
        .strip_suffix(" -->")
}

/// The listed issues of the sweep issue `number`'s list record: the bot's
/// earliest comment whose first line names it, which comes before the run.
fn listed(thread: &[Value], bot: u64, number: u64) -> Result<Vec<String>> {
    let record = (thread.iter())
        .filter(|c| c["user"]["id"].as_u64() == Some(bot))
        .filter(|c| marked(text(c, "/body"), LIST_MARKER).is_some())
        .min_by_key(|c| c["id"].as_u64())
        .context("it has no list record")?;
    let body = text(record, "/body");
    ensure!(
        marked(body, LIST_MARKER) == Some(&number.to_string()),
        "its list record names another issue"
    );
    let list =
        plan::fenced_json(body, shared::LIST_FENCE)?.context("its list record has no list")?;
    ensure!(
        list["schema"] == shared::LIST_SCHEMA,
        "its list schema is not {}",
        shared::LIST_SCHEMA
    );
    Ok((list["issues"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .map(str::to_ascii_lowercase)
        .collect())
}

/// An `attach` as read from both sides now.
enum Side {
    /// Neither: the child's newest parent comment names no epic.
    None,
    /// The child names the epic, which does not list it back.
    Child,
    /// Both agree.
    Both,
    /// The child names another epic.
    Other(IssueRef),
}

/// Each epic an attach names, by lower-case `OWNER/REPO#N`: its labels and
/// the children its newest children comment lists, read once.
type Epics = BTreeMap<String, (Vec<String>, Vec<IssueRef>)>;

/// What an apply makes of the verdicts.
#[derive(Default)]
struct Applying {
    /// Both relationship comments to write: the issue and its epic.
    attach: Vec<(ItemRef, IssueRef)>,
    /// Only the epic's children comment, the child's being there.
    complete: Vec<(ItemRef, IssueRef)>,
    done: Vec<String>,
    left: Vec<String>,
}

impl<C: ForgejoTransport> Schedule<'_, C> {
    /// With [`Config::sweep`], the bot's open sweep issue in its repository,
    /// else the newest it filed since this week's due time: posted by the
    /// bot's id, its body starting with the marker. Each listing is read
    /// whole, or the sweep errs: one could hide past it, and another would
    /// be filed.
    pub(super) fn find_sweep(&self) -> Result<Option<Found>> {
        let config = self.config;
        let Some(sweep) = &config.sweep else {
            return Ok(None);
        };
        let repository = sweep.repository(config)?;
        let due = sweep.due(self.now)?;
        let since = shared::stamp(due);
        for state in ["open", "all"] {
            let mut query = vec![
                ("type", "issues"),
                ("state", state),
                ("created_by", config.bot.as_str()),
            ];
            // Updated since the due time holds every issue filed since.
            if state == "all" {
                query.push(("since", since.as_str()));
            }
            let url = api(self.board, repository, "issues", &query)?;
            let (issues, whole) = pages(url, LIST_PAGES, |url| listing(self.client, url))?;
            ensure!(
                whole,
                "the bot's {state} issues in {repository} are more than {LIST_PAGES} pages; no sweep is filed or started"
            );
            let mut newest: Option<Found> = None;
            for issue in &issues {
                if issue["user"]["id"].as_u64() != Some(self.ids.bot)
                    || !issue["pull_request"].is_null()
                    || !text(issue, "/body").starts_with(shared::SWEEP_MARKER)
                    || !text(issue, "/repository/full_name").eq_ignore_ascii_case(repository)
                {
                    continue;
                }
                let created = OffsetDateTime::parse(text(issue, "/created_at"), &Rfc3339)
                    .context("issue has no created_at")?;
                if state == "all" && created < due {
                    continue;
                }
                let number = issue["number"].as_u64().context("issue has no number")?;
                if newest
                    .as_ref()
                    .is_some_and(|n| (n.created, n.issue.number) > (created, number))
                {
                    continue;
                }
                let labelled = (issue["labels"].as_array().into_iter().flatten())
                    .any(|l| text(l, "/name").eq_ignore_ascii_case(NEEDS_HUMAN));
                let issue_ref = ItemRef {
                    host: instance(self.board),
                    repository: repository.to_owned(),
                    number,
                };
                newest = Some(Found {
                    url: self.web(&issue_ref),
                    issue: issue_ref,
                    open: issue["state"] == "open",
                    created,
                    labelled,
                });
            }
            if newest.is_some() {
                return Ok(newest);
            }
        }
        Ok(None)
    }

    /// The issue's address on this instance, never one a response gave.
    fn web(&self, issue: &ItemRef) -> String {
        format!(
            "{}/{}/issues/{}",
            instance(self.board),
            issue.repository,
            issue.number
        )
    }

    /// The labels of `issue` as read now, as a label write compares them.
    fn labels_of(&self, issue: &ItemRef) -> Result<Vec<String>> {
        let path = format!("issues/{}", issue.number);
        let url = api(self.board, &issue.repository, &path, &[])?;
        names(&self.client.get(&url)?.body)
    }

    /// The sweep issue as the claim and start path takes it.
    fn sweep_item(&self, found: &Found, labels: &[String]) -> Facts {
        Facts {
            issue: found.issue.clone(),
            open: true,
            created: found.created,
            updated: None,
            author: Some(self.config.bot.clone()),
            labels: labels.to_vec(),
            labels_complete: true,
            blocked_by: 0,
            pulls: Vec::new(),
            pulls_complete: true,
            parent: None,
            priority: None,
            status: None,
            why: None,
        }
    }

    /// Whether the sweep issue in `repository` may name an issue of
    /// `source`: its own, or one this instance shows to anyone, by `read`.
    fn shown(&self, repository: &str, source: &str, read: &mut BTreeMap<String, bool>) -> bool {
        source.eq_ignore_ascii_case(repository)
            || *(read.entry(source.to_ascii_lowercase()))
                .or_insert_with(|| self.public(source).unwrap_or(false))
    }

    /// Whether `repository` is public: not private, not internal (shown to
    /// signed-in users only) and of an owner whose visibility is public.
    /// A field that is missing is not public.
    fn public(&self, repository: &str) -> Result<bool> {
        let url = self
            .board
            .api_root()?
            .join(&format!("repos/{repository}"))?;
        let read = self.client.get(&url)?.body;
        Ok(read["full_name"]
            .as_str()
            .is_some_and(|n| n.eq_ignore_ascii_case(repository))
            && read["private"] == false
            && read["internal"] == false
            && read["owner"]["visibility"] == "public")
    }

    /// After stage 4's starts left `free` slots: files this week's sweep
    /// issue where it is due and `candidates` were read, starts its run
    /// where slots and pools are spare, or applies its reply. Where stage 4
    /// starts nothing, `candidates` is None and `free` none: an open sweep
    /// is still applied, settled or labelled, and nothing is filed. How many
    /// runs it took.
    pub(super) fn sweep(
        &mut self,
        candidates: Option<&[&Read]>,
        free: usize,
        busy: &BTreeMap<String, String>,
        snapshot: Option<&Value>,
    ) -> usize {
        let Some(sweep) = &self.config.sweep else {
            return 0;
        };
        // Not read: the schedule said why.
        let Some(found) = self.swept.clone() else {
            return 0;
        };
        let step = match (found, candidates) {
            (Some(found), _) if found.open => self.sweep_open(&found, free, busy, snapshot),
            (found, Some(candidates)) => self
                .sweep_file(sweep, found.as_ref(), candidates)
                .map(|()| 0),
            _ => Ok(0),
        };
        step.unwrap_or_else(|error| {
            self.report.failures += 1;
            self.line(format!("sweep error: {error:#}"));
            0
        })
    }

    /// Files this week's sweep issue where it is due and none was filed
    /// since, listing the candidates outside every live epic that it may
    /// show, then appends its list record: two writes, within
    /// `--max-writes` together. A create whose reply is lost is never sent
    /// again: the next pass finds the issue with no list, stuck.
    fn sweep_file(
        &mut self,
        sweep: &shared::Sweep,
        found: Option<&Found>,
        candidates: &[&Read],
    ) -> Result<()> {
        let due = sweep.due(self.now)?;
        let next = shared::stamp(due + time::Duration::weeks(1));
        if let Some(found) = found.filter(|f| f.created >= due) {
            self.line(format!(
                "sweep: this week's, {}, is closed; the next is due from {next}",
                found.url
            ));
            return Ok(());
        }
        let repository = sweep.repository(self.config)?.to_owned();
        let mut read = BTreeMap::new();
        let facts = candidates.iter().map(|(facts, _)| facts);
        let (mut outside, hidden): (Vec<_>, Vec<_>) = (shared::outside(facts).into_iter())
            .partition(|f| self.shown(&repository, &f.issue.repository, &mut read));
        outside.truncate(sweep.max as usize);
        if !hidden.is_empty() {
            self.line(format!(
                "sweep: {} issue(s) outside the epics are in repositories not read as public, and not listed in {repository}",
                hidden.len()
            ));
        }
        if outside.is_empty() {
            self.line(format!(
                "sweep: no issue is outside the epics; the next is due from {next}"
            ));
            return Ok(());
        }
        let n = outside.len();
        let title = format!("Weekly sweep: {n} issues outside the epics");
        let line = format!("sweep: file {title} in {repository}");
        // The issue and its list record.
        if let Some(held) = self.caps.check_many(Kind::Write, 2) {
            self.line(format!("{held}: {line}"));
            return Ok(());
        }
        if matches!(self.mode, Mode::Preview) {
            self.caps.count_many(Kind::Write, 2);
            self.line(format!("{line}; a preview files nothing"));
            return Ok(());
        }
        let issues: Vec<String> = outside.iter().map(|f| f.issue.display()).collect();
        let links: String = (outside.iter())
            .map(|f| format!("- {}\n", self.web(&f.issue)))
            .collect();
        let body = format!(
            "{}\nThese open issues are outside every live epic, so no triage run looks at them on its own. \
             One triage run sweeps them when capacity is spare: the board pass attaches those it says belong to a live epic, \
             and lists the rest here for {}, who closes this issue once they are settled. \
             The board pass's first comment here is its record of this list.\n\n{links}\n{}",
            shared::SWEEP_MARKER,
            self.config.operator,
            crate::Footer::default()
        );
        let forge = self.epic_forge();
        let number = forge
            .create_issue(&repository, &title, &body, &[])
            .context("filing the sweep issue; never filed again: the next pass finds it")?;
        let issue = ItemRef {
            host: instance(self.board),
            repository,
            number,
        };
        let list =
            json!({"schema": shared::LIST_SCHEMA, "due": shared::stamp(due), "issues": issues});
        let record = format!(
            "<!-- {LIST_MARKER}: {number} -->\nThe board pass's record of what this sweep lists.\n\n```{}\n{list:#}\n```\n",
            shared::LIST_FENCE
        );
        self.write_on(
            &forge,
            &issue,
            &[],
            BoardOperation::Comment { body: record },
        )
        .with_context(|| {
            format!(
                "recording the list of {}; it waits for a person",
                self.web(&issue)
            )
        })?;
        self.line(format!(
            "sweep: filed {}: {n} issue(s) outside the epics",
            self.web(&issue)
        ));
        Ok(())
    }

    /// The open sweep issue `found`: its run started, recovered or waited
    /// for, or its reply applied once, or its apply settled. Where it is
    /// stuck it is labelled `needs-human`: no new sweep is filed while it is
    /// open. How many runs it took.
    fn sweep_open(
        &mut self,
        found: &Found,
        free: usize,
        busy: &BTreeMap<String, String>,
        snapshot: Option<&Value>,
    ) -> Result<usize> {
        let url = &found.url;
        let bot = self.ids.bot;
        let at = self.write_identity(&found.issue);
        // Every recorder comment unedited, or none is read.
        let (value, thread) = match self.epic_forge().issue_thread(&at) {
            Ok(read) => read,
            Err(error) => {
                let why = format!("its records cannot be read: {error:#}; a person closes it");
                self.sweep_stuck(found, &[], &why);
                return Ok(0);
            }
        };
        let labels = names(&value)?;
        let listed = match listed(&thread, bot, found.issue.number) {
            Ok(listed) => listed,
            Err(error) => {
                let why = format!("{error:#}; a person closes it");
                self.sweep_stuck(found, &labels, &why);
                return Ok(0);
            }
        };
        // An apply claimed: settled, or recovered.
        let claimed = (thread.iter())
            .filter(|c| {
                (c["user"]["id"].as_u64()).is_some_and(|id| self.board.recorders.contains(&id))
            })
            .filter_map(|c| {
                Some((
                    c["id"].as_u64()?,
                    marked(text(c, "/body"), SWEEP_APPLY_MARKER)?,
                ))
            })
            .min_by_key(|(id, _)| *id);
        if let Some((_, plan)) = claimed {
            self.sweep_claimed(found, &labels, &thread, &listed, plan)?;
            return Ok(0);
        }
        let item = self.sweep_item(found, &labels);
        let accepted = match self.accepted(&found.issue) {
            Ok(accepted) => accepted,
            Err(_)
                if !(thread.iter()).any(|c| {
                    c["user"]["id"]
                        .as_u64()
                        .is_some_and(|id| self.board.recorders.contains(&id))
                        && plan::has_fence(text(c, "/body"), PLAN_FENCE)
                }) =>
            {
                return self.sweep_start(found, &labels, &listed, free, snapshot);
            }
            Err(error) => {
                let why = format!("its plan cannot be used: {error:#}; a person replaces it");
                self.sweep_stuck(found, &labels, &why);
                return Ok(0);
            }
        };
        let claim = match self.claim(&item, accepted.id) {
            Ok(Some(claim)) => claim,
            Ok(None) => {
                let why = format!(
                    "its plan {} has no dispatch claim; a person dispatches it",
                    accepted.id
                );
                self.sweep_stuck(found, &labels, &why);
                return Ok(0);
            }
            Err(error) => {
                let why = format!("its plan cannot be used: {error:#}; a person replaces it");
                self.sweep_stuck(found, &labels, &why);
                return Ok(0);
            }
        };
        if claim.receipts.is_empty() {
            // A start cut short: found and receipted, or held; never again.
            if matches!(self.mode, Mode::Preview) {
                self.line(format!(
                    "sweep {url}: its start was cut short; an applying pass recovers it"
                ));
                return Ok(0);
            }
            if let Err(error) = self.apply(&item, "", snapshot) {
                self.report.failures += 1;
                self.line(format!("sweep {url}: recovering its start: {error:#}"));
            }
            return Ok(0);
        }
        if let Some(run) = busy.get(&key(&found.issue)) {
            self.line(format!("sweep {url}: its run is in flight, {run}"));
            return Ok(0);
        }
        let reply = (thread.iter().rev())
            .filter(|c| c["id"].as_u64() > Some(claim.claim))
            .find(|c| {
                let body = text(c, "/body");
                c["user"]["id"].as_u64() == Some(bot)
                    && marked(body, SWEEP_APPLY_MARKER).is_none()
                    && marked(body, RECEIPT_MARKER).is_none()
                    && plan::has_fence(body, shared::VERDICTS_FENCE)
            });
        let Some(reply) = reply else {
            let why = "its run gave no verdicts; a person runs it again";
            self.sweep_stuck(found, &labels, why);
            return Ok(0);
        };
        let reply_id = reply["id"].as_u64().context("the reply has no id")?;
        let verdicts = match shared::verdicts(text(reply, "/body"), &listed, &self.sweep_epics()) {
            Ok(verdicts) => verdicts,
            Err(error) => {
                let why = format!(
                    "its verdicts {url}#issuecomment-{reply_id} cannot be read: {error:#}; a person runs it again"
                );
                self.sweep_stuck(found, &labels, &why);
                return Ok(0);
            }
        };
        self.sweep_apply(found, &labels, reply_id, &verdicts, false)?;
        Ok(0)
    }

    /// The live epics stage 3 read that the sweep issue may name.
    fn sweep_epics(&self) -> BTreeSet<String> {
        let Some(sweep) = &self.config.sweep else {
            return BTreeSet::new();
        };
        let Ok(repository) = sweep.repository(self.config) else {
            return BTreeSet::new();
        };
        let mut read = BTreeMap::new();
        (self.live.iter())
            .filter(|at| {
                at.split_once('#')
                    .is_some_and(|(source, _)| self.shown(repository, source, &mut read))
            })
            .cloned()
            .collect()
    }

    /// The sweep's run, below all other work: its triage plan posted and
    /// started through stage 4's claim and start path, with a slot left
    /// after stage 4's starts and every triage pool under its pace.
    fn sweep_start(
        &mut self,
        found: &Found,
        labels: &[String],
        listed: &[String],
        free: usize,
        snapshot: Option<&Value>,
    ) -> Result<usize> {
        let config = self.config;
        let url = &found.url;
        let mut read = BTreeMap::new();
        let source = (listed.iter()).find_map(|at| {
            let (source, _) = at.split_once('#')?;
            // Listed while it was a work repository, and no longer one.
            if !config.works_in(source) {
                Some((source, "not a work repository"))
            } else if !self.shown(&found.issue.repository, source, &mut read) {
                Some((source, "not read as public"))
            } else {
                None
            }
        });
        if let Some((source, not)) = source {
            let why = format!("it lists an issue of {source}, {not}; no run starts");
            self.sweep_stuck(found, labels, &why);
            return Ok(0);
        }
        if free == 0 {
            self.line(format!("sweep {url}: no slot free; it waits"));
            return Ok(0);
        }
        if Instant::now() > self.client.bounds.admit_by {
            let limit = PASS_SECONDS_LIMIT;
            self.line(format!("sweep {url}: the pass ran past {limit} seconds"));
            return Ok(0);
        }
        let mut over = Vec::new();
        for name in config.triage_pools() {
            let Some((_, settings)) = config.pools.0.iter().find(|(n, _)| n == name) else {
                continue;
            };
            match room(snapshot, name, settings, self.now) {
                Ok(room) if room > 0.0 => {}
                Ok(_) => over.push(format!("{name} is not under its pace")),
                Err(why) => over.push(why),
            }
        }
        if !over.is_empty() {
            self.line(format!(
                "sweep {url}: waits for every pool to be under its pace: {}",
                over.join("; ")
            ));
            return Ok(0);
        }
        if let Some(why) = budget_hold(config, snapshot, self.now, 1.0) {
            self.line(format!("sweep {url}: budget: {why}"));
            return Ok(0);
        }
        let pool = match choose(snapshot, &config.pools, config.triage_pools(), self.now) {
            Ok(pool) => pool,
            Err(why) => {
                self.line(format!("sweep {url}: no pool: {why}"));
                return Ok(0);
            }
        };
        let line = format!("start sweep {} pool={pool}", found.issue.display());
        if matches!(self.mode, Mode::Preview) {
            if let Some(held) = self.caps.check(Kind::Dispatch) {
                self.line(format!("{held}: {line}"));
                return Ok(0);
            }
            self.caps.count(Kind::Dispatch);
            self.line(line);
            return Ok(1);
        }
        if !config.leases {
            let why = "applying Forgejo work requires leases; no reservation, no start";
            self.line(format!("sweep {url}: {why}"));
            return Ok(0);
        }
        // The plan's comment, the lease, claim, bind and receipt.
        if let Some(held) =
            (self.caps.check(Kind::Dispatch)).or_else(|| self.caps.check_many(Kind::Write, 5))
        {
            self.line(format!("{held}: {line}"));
            return Ok(0);
        }
        self.line(line);
        let brief = shared::brief(url, &config.operator, &self.sweep_epics());
        let issue = &found.issue;
        let job = json!({"schema": JOB_PLAN_SCHEMA, "repo": issue.repository, "item": issue.number,
            "kind": "triage", "runner": TRIAGE_RUNNER, "brief": brief, "pool": pool});
        let body = format!(
            "{} At {}.\n\n```{PLAN_FENCE}\n{job:#}\n```\n",
            shared::SWEEP_INTRO,
            self.now.format(&Rfc3339)?
        );
        let item = self.sweep_item(found, labels);
        let forge = self.epic_forge();
        let started = self
            .write_on(&forge, issue, labels, BoardOperation::Comment { body })
            .and_then(|()| self.apply(&item, &pool, snapshot));
        match started {
            Ok(true) => Ok(1),
            Ok(false) => Ok(0),
            Err(error) => {
                self.report.failures += 1;
                self.line(format!("sweep {url}: its run could not start: {error:#}"));
                Ok(0)
            }
        }
    }

    /// An apply claimed on the sweep issue, naming the reply comment `plan`
    /// (`ID@CREATED`): settled where its receipt is there, else recovered.
    fn sweep_claimed(
        &mut self,
        found: &Found,
        labels: &[String],
        thread: &[Value],
        listed: &[String],
        plan: &str,
    ) -> Result<()> {
        let reply_id: u64 = (plan.split_once('@'))
            .and_then(|(n, _)| n.parse().ok())
            .context("the apply claim names no comment")?;
        let at = self.write_identity(&found.issue);
        let claim = self
            .epic_forge()
            .plan_claim(&at, SWEEP_APPLY_MARKER, reply_id);
        let claim = match claim {
            Ok(Some(claim)) if claim.recorder == self.ids.bot => claim,
            Ok(_) => {
                let why = "its apply claim is not the bot's; a person closes it";
                self.sweep_stuck(found, labels, why);
                return Ok(());
            }
            Err(error) => {
                let why = format!("its apply claim cannot be read: {error:#}; a person closes it");
                self.sweep_stuck(found, labels, &why);
                return Ok(());
            }
        };
        let applied = (claim.receipts.iter())
            .filter_map(|r| plan::fenced_json(r, APPLIED_FENCE).ok().flatten())
            .find(|r| r["schema"] == APPLIED_SCHEMA && r["reply"] == reply_id);
        if let Some(applied) = applied {
            let left = applied["left"]
                .as_u64()
                .context("its receipt counts nothing left")?;
            return self.sweep_settle(found, labels, left as usize);
        }
        // Cut short between its claim and its receipt: the reply read again,
        // and only what both issues prove now is counted.
        let read = (thread.iter())
            .find(|c| c["id"].as_u64() == Some(reply_id))
            .filter(|c| c["user"]["id"].as_u64() == Some(self.ids.bot))
            .context("the claimed reply is not the bot's comment")
            .and_then(|reply| shared::verdicts(text(reply, "/body"), listed, &self.sweep_epics()));
        match read {
            Ok(verdicts) => self.sweep_apply(found, labels, reply_id, &verdicts, true),
            Err(error) => {
                let why =
                    format!("its claimed verdicts cannot be read: {error:#}; a person closes it");
                self.sweep_stuck(found, labels, &why);
                Ok(())
            }
        }
    }

    /// Reads each listed issue and its parent comment, and each named
    /// epic's children comment, into what applying the verdicts does. In
    /// `recovering`, an attach with neither side is left, never sent again.
    fn sweep_plan(
        &self,
        found: &Found,
        verdicts: &[(ItemRef, Verdict)],
        recovering: bool,
    ) -> Result<(Applying, Epics)> {
        let forge = self.epic_forge();
        let bot = self.ids.bot;
        let sweep = &found.issue.repository;
        let mut out = Applying::default();
        let mut epics = Epics::new();
        let mut additions = BTreeMap::<String, usize>::new();
        let mut read = BTreeMap::new();
        for (issue, verdict) in verdicts {
            let name = issue.display();
            let said = || match verdict {
                Verdict::Attach(epic) => format!("belongs to {}", epic.display()),
                Verdict::Close(verdict, reason) => format!("{verdict}, to close: {reason}"),
                Verdict::Question(question) => question.clone(),
            };
            if !self.config.works_in(&issue.repository) {
                out.left
                    .push(format!("- {name}, not in a work repository: {}", said()));
                continue;
            }
            // Read as public before its run, and no longer: its verdict may
            // hold what the run read there since, so it is not copied here.
            if !self.shown(sweep, &issue.repository, &mut read) {
                out.left.push(format!(
                    "- {name}, not read as public: its verdict is left out"
                ));
                continue;
            }
            let child = IssueRef {
                repo: issue.repository.clone(),
                number: issue.number,
            };
            let item = self.write_identity(&self.item(&child));
            let (value, thread) = match forge.issue_thread(&item) {
                Ok(read) => read,
                Err(error) if error.downcast_ref::<NotFound>().is_some() => {
                    out.left.push(format!("- {name}, not found: {}", said()));
                    continue;
                }
                Err(error) => return Err(error.context(format!("reading {name}"))),
            };
            // Settled since: the operator closed it.
            if value["state"] != "open" {
                continue;
            }
            let Verdict::Attach(epic) = verdict else {
                out.left.push(format!("- {name}: {}", said()));
                continue;
            };
            let epic = IssueRef {
                repo: epic.repository.clone(),
                number: epic.number,
            };
            let to = epic.to_string().to_ascii_lowercase();
            if !epics.contains_key(&to) {
                let at = self.write_identity(&self.item(&epic));
                let (value, thread) = forge
                    .issue_thread(&at)
                    .with_context(|| format!("reading the epic {epic}"))?;
                ensure!(value["state"] == "open", "the epic {epic} is not open");
                let labels = names(&value)?;
                let children = relations::children(&thread, bot, epic.number)?;
                epics.insert(to.clone(), (labels, children));
            }
            let named = child.to_string();
            let lists = (epics[&to].1.iter()).any(|c| c.to_string().eq_ignore_ascii_case(&named));
            let side = match relations::parent(&thread, bot)? {
                None => Side::None,
                Some(p) if p.to_string().eq_ignore_ascii_case(&to) && lists => Side::Both,
                Some(p) if p.to_string().eq_ignore_ascii_case(&to) => Side::Child,
                Some(p) => Side::Other(p),
            };
            // Reserve space for every new child planned in this apply,
            // without changing the lists that sweep_write will append to.
            let added = additions.entry(to.clone()).or_default();
            let full = !lists && epics[&to].1.len() + *added >= relations::CHILD_LIMIT;
            match side {
                Side::Both => out
                    .done
                    .push(format!("- {name} is a sub-issue of {epic} already")),
                Side::Other(parent) => {
                    let parent = match self.shown(sweep, &parent.repo, &mut read) {
                        true => parent.to_string(),
                        false => "an issue not read as public".to_owned(),
                    };
                    out.left.push(format!(
                        "- {name}: belongs to {epic}, but is a sub-issue of {parent} already"
                    ));
                }
                _ if full => out.left.push(format!(
                    "- {name}: belongs to {epic}, which lists {} children already",
                    relations::CHILD_LIMIT
                )),
                Side::Child => {
                    *added += 1;
                    out.complete.push((issue.clone(), epic));
                }
                Side::None if recovering => out.left.push(format!(
                    "- {name}: belongs to {epic}; the apply was cut short before attaching it, which is never sent again"
                )),
                Side::None => {
                    *added += usize::from(!lists);
                    out.attach.push((issue.clone(), epic));
                }
            }
        }
        Ok((out, epics))
    }

    /// Applies a sweep's verdicts from the reply `reply_id`: its claim
    /// (unless `recovering` it), each attach's two relationship comments
    /// (the child's, then its epic's), each half-made one's epic side, one
    /// receipt naming the reply with what is left, then the sweep issue's
    /// `needs-human` label where something is, else its close. Every write
    /// within `--max-writes` together; nothing in a preview.
    fn sweep_apply(
        &mut self,
        found: &Found,
        labels: &[String],
        reply_id: u64,
        verdicts: &[(ItemRef, Verdict)],
        recovering: bool,
    ) -> Result<()> {
        let url = &found.url;
        let reply = format!("{url}#issuecomment-{reply_id}");
        let (mut plan, mut epics) = self.sweep_plan(found, verdicts, recovering)?;
        let attaches = plan.attach.len() + plan.complete.len();
        let summary = format!(
            "sweep {url}: verdicts {reply}: attach {attaches}, {} left for {}",
            plan.left.len(),
            self.config.operator
        );
        let writes = 2 * plan.attach.len()
            + plan.complete.len()
            + usize::from(!recovering)
            + 1
            + usize::from(plan.left.is_empty() || !found.labelled);
        if let Some(held) = self.caps.check_many(Kind::Write, writes) {
            self.line(format!("{held}: {summary}"));
            return Ok(());
        }
        if matches!(self.mode, Mode::Preview) {
            self.caps.count_many(Kind::Write, writes);
            for (issue, epic) in plan.attach.iter().chain(&plan.complete) {
                self.line(format!(
                    "sweep: attach {} to {epic}; a preview writes nothing",
                    issue.display()
                ));
            }
            self.line(summary);
            return Ok(());
        }
        let bounds = self.client.bounds;
        let needed = WRITE_REQUESTS * writes;
        if bounds.left.get() < needed + WRITE_RESERVE || Instant::now() > bounds.admit_by {
            self.report.failures += 1;
            self.line(format!(
                "sweep {url}: the pass has {} requests left, fewer than its apply may take ({needed}), or ran out of time; it waits",
                bounds.left.get()
            ));
            return Ok(());
        }
        // Its reads were in time, so its writes may finish past the bound.
        bounds.finishing.set(true);
        let result = self.sweep_write(found, labels, reply_id, &mut plan, &mut epics, recovering);
        bounds.finishing.set(false);
        self.report.failures += result?;
        let done = plan.done.len();
        let left = plan.left.len();
        let what = match recovering {
            true => "recovered",
            false => "applied",
        };
        self.line(format!(
            "sweep {url}: {what} {reply}: attached {done}, {left} left for {}",
            self.config.operator
        ));
        Ok(())
    }

    /// [`Self::sweep_apply`]'s writes, in order: how many attaches failed,
    /// each left for the operator; an error where the claim, the receipt
    /// or the last write failed.
    fn sweep_write(
        &self,
        found: &Found,
        labels: &[String],
        reply_id: u64,
        plan: &mut Applying,
        epics: &mut Epics,
        recovering: bool,
    ) -> Result<usize> {
        let forge = self.epic_forge();
        let issue = &found.issue;
        let mut failures = 0;
        if !recovering {
            let claim = BoardOperation::Claim {
                marker: SWEEP_APPLY_MARKER.into(),
                plan: reply_id,
            };
            self.write_on(&forge, issue, labels, claim)?;
        }
        // The epic's side: its children comment with `child` added.
        let mut epic_side = |child: &ItemRef, epic: &IssueRef| -> Result<()> {
            let to = epic.to_string().to_ascii_lowercase();
            let (labels, children) = epics.get_mut(&to).context("epic not read")?;
            let child = IssueRef {
                repo: child.repository.clone(),
                number: child.number,
            };
            if children
                .iter()
                .any(|c| c.to_string().eq_ignore_ascii_case(&child.to_string()))
            {
                return Ok(());
            }
            let mut next = children.clone();
            next.push(child);
            let body = relations::children_comment(epic.number, &next);
            let at = self.item(epic);
            self.write_on(&forge, &at, labels, BoardOperation::Comment { body })?;
            *children = next;
            Ok(())
        };
        for (child, epic) in std::mem::take(&mut plan.attach) {
            let name = child.display();
            let parent = BoardOperation::Comment {
                body: relations::parent_comment(&epic),
            };
            let written = self
                .labels_of(&child)
                .and_then(|labels| self.write_on(&forge, &child, &labels, parent))
                .and_then(|()| epic_side(&child, &epic));
            match written {
                Ok(()) => plan.done.push(format!("- Attached {name} to {epic}")),
                Err(error) => {
                    failures += 1;
                    plan.left.push(format!(
                        "- {name}: belongs to {epic}, but attaching it failed: {error:#}"
                    ));
                }
            }
        }
        for (child, epic) in std::mem::take(&mut plan.complete) {
            let name = child.display();
            match epic_side(&child, &epic) {
                Ok(()) => plan.done.push(format!(
                    "- Attached {name} to {epic}: its parent comment was there, and the epic now lists it"
                )),
                Err(error) => {
                    failures += 1;
                    plan.left.push(format!(
                        "- {name}: belongs to {epic}, but listing it on the epic failed: {error:#}"
                    ));
                }
            }
        }
        let attached: Vec<&str> = plan.done.iter().map(String::as_str).collect();
        let outcome = match recovering {
            true => "recovered",
            false => "applied",
        };
        let detail = json!({"schema": APPLIED_SCHEMA, "reply": reply_id, "outcome": outcome,
            "attached": attached, "left": plan.left.len()});
        let mut body = format!(
            "The board pass {outcome} [this sweep's verdicts]({}#issuecomment-{reply_id}).\n",
            found.url
        );
        if !plan.done.is_empty() {
            body.push_str(&format!("\n{}\n", plan.done.join("\n")));
        }
        if !plan.left.is_empty() {
            body.push_str(&format!(
                "\nFor {}, who closes this issue once they are settled:\n\n{}\n",
                self.config.operator,
                plan.left.join("\n")
            ));
        }
        body.push_str(&format!("\n```{APPLIED_FENCE}\n{detail:#}\n```\n"));
        let receipt = BoardOperation::Receipt {
            marker: SWEEP_APPLY_MARKER.into(),
            plan: reply_id,
            body,
        };
        self.write_on(&forge, issue, labels, receipt)
            .context("could not record the apply's receipt; its claim remains, and the next pass recovers it")?;
        self.sweep_finish(found, labels, plan.left.len())?;
        Ok(failures)
    }

    /// The sweep issue's last write: labelled `needs-human` where `left` is
    /// more than none, else closed.
    fn sweep_finish(&self, found: &Found, labels: &[String], left: usize) -> Result<()> {
        let operation = match left {
            0 => BoardOperation::State {
                state: "closed".into(),
            },
            _ if found.labelled => return Ok(()),
            _ => BoardOperation::Label {
                scope: NEEDS_HUMAN.into(),
                name: NEEDS_HUMAN.into(),
            },
        };
        self.write_on(&self.epic_forge(), &found.issue, labels, operation)
    }

    /// An applied sweep issue: its label or close, where a pass stopped
    /// before it, else it waits for the operator.
    fn sweep_settle(&mut self, found: &Found, labels: &[String], left: usize) -> Result<()> {
        let url = &found.url;
        if left > 0 && found.labelled {
            self.line(format!(
                "sweep {url}: applied; it waits for {} to close it",
                self.config.operator
            ));
            return Ok(());
        }
        let line = match left {
            0 => format!("sweep {url}: applied, with nothing left; close it"),
            _ => format!(
                "sweep {url}: applied, with something left for the operator; label {NEEDS_HUMAN}"
            ),
        };
        if let Some(held) = self.caps.check(Kind::Write) {
            self.line(format!("{held}: {line}"));
            return Ok(());
        }
        if matches!(self.mode, Mode::Preview) {
            self.caps.count(Kind::Write);
        } else {
            self.sweep_finish(found, labels, left)?;
        }
        self.line(line);
        Ok(())
    }

    /// A sweep issue that waits on a person, `why`: labelled `needs-human`
    /// unless it is already, one write within `--max-writes`.
    fn sweep_stuck(&mut self, found: &Found, labels: &[String], why: &str) {
        let url = &found.url;
        if found.labelled {
            self.line(format!("sweep {url}: {why}"));
            return;
        }
        let line = format!("sweep {url}: {why}; label {NEEDS_HUMAN}");
        if let Some(held) = self.caps.check(Kind::Write) {
            self.line(format!("{held}: {line}"));
            return;
        }
        if matches!(self.mode, Mode::Preview) {
            self.caps.count(Kind::Write);
            self.line(line);
            return;
        }
        let label = BoardOperation::Label {
            scope: NEEDS_HUMAN.into(),
            name: NEEDS_HUMAN.into(),
        };
        // Where its thread could not be read, its labels are read alone.
        let labels = match labels.is_empty() {
            true => self.labels_of(&found.issue),
            false => Ok(labels.to_vec()),
        };
        let written = labels
            .and_then(|labels| self.write_on(&self.epic_forge(), &found.issue, &labels, label));
        match written {
            Ok(()) => self.line(line),
            Err(error) => {
                self.report.failures += 1;
                self.line(format!("{line} failed: {error:#}"));
            }
        }
    }
}
