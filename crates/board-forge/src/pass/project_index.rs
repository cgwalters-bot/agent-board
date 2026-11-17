//! Project fields are a projection of fresh issue-thread evidence, not inputs
//! to scheduling. Incomplete observations do not erase the last known values.

use super::*;
use crate::github::{GitHub, field_writes};

const PLAN: &str = "Plan";
const VERDICT: &str = "Verdict";
const VERDICT_TARGET: &str = "Verdict target";
const RUN: &str = "Run";
pub(super) const VERDICTS: [&str; 8] = [
    "done",
    "obsolete",
    "needs-design",
    "needs-operator",
    "needs-issue",
    "partly",
    "not-done",
    "follow-through",
];

pub(super) const MEMBERS: &str = r#"query($owner: String!, $number: Int!, $cursor: String) {
  repositoryOwner(login: $owner) {
    ... on Organization { projectV2(number: $number) { ...Members } }
    ... on User { projectV2(number: $number) { ...Members } }
  }
}
fragment Members on ProjectV2 {
  id
  items(first: 100, after: $cursor, query: "is:issue is:open") {
    nodes { id content { __typename ... on Issue { id number state repository { nameWithOwner } } } }
    pageInfo { hasNextPage endCursor }
  }
}"#;

pub(super) const ITEM: &str = r#"query($item: ID!) {
  node(id: $item) { ... on ProjectV2Item {
    id project { id }
    content { __typename ... on Issue { id number state updatedAt repository { nameWithOwner } } }
    plan: fieldValueByName(name: "Plan") { __typename ... on ProjectV2ItemFieldTextValue { text field { ... on ProjectV2Field { id } } } }
    verdict: fieldValueByName(name: "Verdict") { __typename ... on ProjectV2ItemFieldSingleSelectValue { name optionId field { ... on ProjectV2SingleSelectField { id } } } }
    target: fieldValueByName(name: "Verdict target") { __typename ... on ProjectV2ItemFieldTextValue { text field { ... on ProjectV2Field { id } } } }
    run: fieldValueByName(name: "Run") { __typename ... on ProjectV2ItemFieldTextValue { text field { ... on ProjectV2Field { id } } } }
  } }
}"#;

pub(super) struct Schema<'a> {
    fields: &'a [Value],
    ids: BTreeMap<&'static str, &'a str>,
    options: BTreeMap<&'a str, &'a str>,
}

impl<'a> Schema<'a> {
    pub(super) fn read(fields: &'a [Value]) -> Result<Self> {
        let mut ids = BTreeMap::new();
        let mut options = BTreeMap::new();
        for (name, kind) in [
            (PLAN, "TEXT"),
            (VERDICT, "SINGLE_SELECT"),
            (VERDICT_TARGET, "TEXT"),
            (RUN, "TEXT"),
        ] {
            let mut named = fields.iter().filter(|f| f["name"] == name);
            let field = named
                .next()
                .with_context(|| format!("provision project field {name}"))?;
            ensure!(
                named.next().is_none() && field["dataType"] == kind,
                "project index requires one {name} {kind} field"
            );
            let id = nonempty(field, "/id").with_context(|| format!("{name} field id"))?;
            ensure!(
                fields.iter().filter(|f| f["id"] == id).count() == 1,
                "duplicate project field id"
            );
            ids.insert(name, id);
            if name == VERDICT {
                let entries = field["options"]
                    .as_array()
                    .context("provision Verdict options")?;
                let mut option_ids = BTreeSet::new();
                for entry in entries {
                    let name = nonempty(entry, "/name")?;
                    let id = nonempty(entry, "/id")?;
                    ensure!(
                        options.insert(name, id).is_none() && option_ids.insert(id),
                        "duplicate Verdict option"
                    );
                }
                ensure!(
                    options.len() == VERDICTS.len()
                        && VERDICTS.iter().all(|v| options.contains_key(v)),
                    "provision Verdict options exactly: {}",
                    VERDICTS.join(", ")
                );
            }
        }
        Ok(Self {
            fields,
            ids,
            options,
        })
    }

    fn current<'n>(&self, node: &'n Value, name: &str) -> Result<Option<&'n str>> {
        let alias = match name {
            PLAN => "plan",
            VERDICT => "verdict",
            VERDICT_TARGET => "target",
            RUN => "run",
            _ => bail!("unknown index field {name}"),
        };
        let value = node
            .get(alias)
            .with_context(|| format!("missing {name} value"))?;
        if value.is_null() {
            return Ok(None);
        }
        ensure!(
            text(value, "/field/id") == self.ids[name],
            "wrong {name} field identity"
        );
        if name == VERDICT {
            ensure!(
                value["__typename"] == "ProjectV2ItemFieldSingleSelectValue",
                "invalid Verdict value type"
            );
            let selected = nonempty(value, "/name")?;
            ensure!(
                self.options
                    .get(selected)
                    .is_some_and(|id| *id == text(value, "/optionId")),
                "unknown Verdict value"
            );
            Ok(Some(selected))
        } else {
            ensure!(
                value["__typename"] == "ProjectV2ItemFieldTextValue",
                "invalid {name} value type"
            );
            Ok(Some(value["text"].as_str().context("missing index text")?))
        }
    }
}

fn nonempty<'a>(node: &'a Value, path: &str) -> Result<&'a str> {
    let value = text(node, path);
    ensure!(!value.is_empty(), "missing {path}");
    Ok(value)
}

#[derive(PartialEq)]
pub(super) struct Version {
    pub(super) id: String,
    pub(super) open: bool,
    updated: OffsetDateTime,
}

impl Version {
    fn read(node: &Value, issue: &ItemRef) -> Result<Self> {
        let (found, id) = intake::backfill_identity(node, &issue.repository)?;
        ensure!(
            key(&found) == key(issue),
            "project index returned another issue"
        );
        Ok(Self {
            id,
            open: node["state"] == "OPEN",
            updated: OffsetDateTime::parse(nonempty(node, "/updatedAt")?, &Rfc3339)
                .context("issue update time")?,
        })
    }
}

pub(super) struct Completion {
    version: Version,
    thread: [u8; 32],
    pub(super) done: bool,
}

pub(super) type Completions = BTreeMap<String, Completion>;

fn thread_digest(comments: &[plan::Comment]) -> Result<[u8; 32]> {
    let mut digest = Sha256::new();
    for c in comments {
        digest.update(serde_json::to_vec(&(
            &c.url,
            &c.body,
            &c.author,
            &c.editor,
            c.edited,
            c.created.unix_timestamp_nanos(),
            c.changed.unix_timestamp_nanos(),
        ))?);
    }
    Ok(digest.finalize().into())
}

fn issue_url(issue: &ItemRef) -> String {
    format!(
        "https://github.com/{}/issues/{}",
        issue.repository, issue.number
    )
}

fn scoped_comments(issue: &ItemRef, comments: &[plan::Comment]) -> Result<()> {
    let prefix = format!("{}#issuecomment-", issue_url(issue)).to_ascii_lowercase();
    let mut ids = BTreeSet::new();
    for comment in comments {
        ensure!(
            comment
                .url
                .to_ascii_lowercase()
                .strip_prefix(&prefix)
                .and_then(|id| id
                    .parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0 && id == n.to_string()))
                .is_some_and(|id| ids.insert(id)),
            "missing, foreign or repeated issue comment identity"
        );
        ensure!(
            comment.changed >= comment.created,
            "comment edited before creation"
        );
    }
    Ok(())
}

fn trusted_edit(c: &plan::Comment, writers: &[&str]) -> bool {
    (!c.edited && c.editor.is_none()) || is(c.editor.as_deref(), writers)
}

fn verdict<'c>(
    comments: &'c [plan::Comment],
    bot: &str,
    writers: &[&str],
) -> Option<(&'c plan::Comment, String)> {
    let newest = comments
        .iter()
        .filter(|c| {
            is(c.author.as_deref(), &[bot])
                && (plan::has_fence(&c.body, plan::TRIAGE_FENCE) || !trusted_edit(c, writers))
        })
        .max_by_key(|c| c.changed)?;
    let value = plan::triage_verdict(&newest.body)?;
    (trusted_edit(newest, writers) && VERDICTS.contains(&value.as_str())).then_some((newest, value))
}

fn done_in_thread(comments: &[plan::Comment], done: &plan::Comment, bot: &str) -> bool {
    let dispatch = format!("<!-- {}: ", plan::DISPATCH_MARKER);
    let housekeeping = |c: &plan::Comment| {
        plan::written_by(c, bot)
            && !plan::has_fence(&c.body, PLAN_FENCE)
            && !plan::has_fence(&c.body, plan::TRIAGE_FENCE)
            && (c.body.starts_with(&dispatch) || crate::supervise::health::is_bare_status(&c.body))
    };
    !comments
        .iter()
        .any(|c| c.url != done.url && c.changed >= done.changed && !housekeeping(c))
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    pub(super) fn index_members(&self, project: &str) -> Result<BTreeMap<String, String>> {
        let board = self.config.github_board().context("no index board")?;
        let mut cursor = None;
        let mut cursors = BTreeSet::new();
        let mut items = BTreeMap::new();
        let mut item_ids = BTreeSet::new();
        for _ in 0..BOARD_PAGES {
            let data = self.client.graphql(
                MEMBERS,
                json!({"owner":board.owner,"number":board.project,"cursor":cursor}),
            )?;
            let found = &data["repositoryOwner"]["projectV2"];
            ensure!(
                found["id"] == project,
                "membership belongs to another project"
            );
            let connection = &found["items"];
            for node in connection["nodes"]
                .as_array()
                .context("missing project members")?
            {
                let content = &node["content"];
                ensure!(
                    content["__typename"] == "Issue",
                    "unreadable project issue membership"
                );
                let repo = nonempty(content, "/repository/nameWithOwner")?;
                let (issue, _) = intake::backfill_identity(content, repo)?;
                let item = nonempty(node, "/id")?.to_owned();
                ensure!(
                    item_ids.insert(item.clone()) && items.insert(key(&issue), item).is_none(),
                    "duplicate project membership"
                );
            }
            match connection["pageInfo"]["hasNextPage"].as_bool() {
                Some(false) => return Ok(items),
                Some(true) => {
                    let next = nonempty(connection, "/pageInfo/endCursor")?.to_owned();
                    ensure!(
                        cursors.insert(next.clone()),
                        "project member cursor repeated"
                    );
                    cursor = Some(next);
                }
                None => bail!("project members have no pageInfo"),
            }
        }
        bail!("project membership exceeds {BOARD_PAGES} pages")
    }

    pub(super) fn index_issue(&self, issue: &ItemRef) -> Result<Version> {
        Version::read(&self.issue_read(issue, intake::BACKFILL_FRESH)?, issue)
    }

    pub(super) fn index_recheck(&self, issue: &ItemRef, version: &Version) -> Result<()> {
        if self.index_issue(issue)? != *version {
            return Err(IndexMoved.into());
        }
        Ok(())
    }

    pub(super) fn index_recheck_completion(
        &self,
        issue: &ItemRef,
        read: &Completion,
    ) -> Result<()> {
        let comments = plan::fresh_issue_comments(&self.client, issue)?;
        if thread_digest(&comments)? != read.thread {
            return Err(IndexMoved.into());
        }
        self.index_recheck(issue, &read.version)
    }

    pub(super) fn index_write_slot(&self) -> Result<()> {
        if let Some(held) = self.caps.check(Kind::Write) {
            return Err(BudgetSpent(held.to_string()).into());
        }
        if self.client.bounds.share_spent() {
            return Err(ShareSpent("project index request share spent".into()).into());
        }
        if !self.client.bounds.fits(1) {
            return Err(BudgetSpent("project index request budget or time spent".into()).into());
        }
        self.caps.count(Kind::Write);
        Ok(())
    }

    pub(super) fn index_fields(
        &mut self,
        (github, project, schema): (&GitHub<Budget<'_, C>>, &str, &Schema<'_>),
        item: Option<&str>,
        issue: &ItemRef,
        version: Version,
    ) -> Result<Completion> {
        let comments = plan::fresh_issue_comments(&self.client, issue)?;
        scoped_comments(issue, &comments)?;
        let writers = self.writers();
        let accepted = plan::newest_plan(&comments, &writers)
            .ok()
            .filter(|(p, c)| self.index_plan(issue, p, c));
        let verdict = verdict(&comments, &self.config.bot, &writers);
        let done = if let Some((c, value)) = &verdict
            && value == "done"
            && done_in_thread(&comments, c, &self.config.bot)
        {
            let node = self.issue_read(issue, CLOSE_QUERY)?;
            ensure!(
                text(&node, "/url").eq_ignore_ascii_case(&issue_url(issue))
                    && node.get("lastEditedAt").is_some()
                    && matches!(node["state"].as_str(), Some("OPEN" | "CLOSED")),
                "incomplete done verdict issue evidence"
            );
            if (node["state"] == "OPEN") != version.open {
                return Err(IndexMoved.into());
            }
            let body = plan::comment(&node)?;
            let events = IssueEvents::read(issue, &node)?;
            node["state"] == "OPEN"
                && body.changed <= c.changed
                && events.renamed <= Some(c.changed)
                && events.reopened <= Some(c.changed)
        } else {
            false
        };
        let run = match self.index_run(issue, &comments, &writers) {
            Ok(run) => run,
            Err(error) if budget_spent(&error) => return Err(error),
            Err(error) => {
                self.report
                    .error(&format!("project index {} Run", issue.display()), &error);
                None
            }
        };
        let mut edits = Vec::new();
        if let Some((job, c)) = accepted {
            edits.push(format!("{PLAN}={} {}", plan::plan_id(c)?, job.kind));
        }
        let verdict_target = verdict.as_ref().map(|(c, _)| c.url.as_str());
        if let Some((c, value)) = verdict {
            edits.push(format!("{VERDICT_TARGET}={}", c.url));
            edits.push(format!("{VERDICT}={value}"));
        }
        if let Some(url) = run {
            edits.push(format!("{RUN}={url}"));
        }
        let reads = Completion {
            version,
            thread: thread_digest(&comments)?,
            done,
        };
        for write in field_writes(schema.fields, &edits)? {
            // Keep the card read, including the target paired with Verdict,
            // as the last read before the mutation.
            if self.apply {
                self.index_recheck_completion(issue, &reads)?;
            }
            if let Some(id) = item {
                let data = self.client.graphql(ITEM, json!({"item":id}))?;
                let node = &data["node"];
                ensure!(
                    node["id"] == id
                        && node["project"]["id"] == project
                        && node["content"]["__typename"] == "Issue",
                    "index item is not on the configured project"
                );
                if Version::read(&node["content"], issue)? != reads.version {
                    return Err(IndexMoved.into());
                }
                if self.apply
                    && write.name == VERDICT
                    && schema.current(node, VERDICT_TARGET)? != verdict_target
                {
                    return Err(IndexMoved.into());
                }
                if schema.current(node, &write.name)? == Some(write.value.as_str()) {
                    continue;
                }
                // Never attribute an old verdict to a newly recorded target,
                // even if a write reply is lost or the new comment disappears.
                if write.name == VERDICT_TARGET && schema.current(node, VERDICT)?.is_some() {
                    self.index_write_slot()?;
                    if self.apply {
                        github.clear_field(project, id, schema.ids[VERDICT], VERDICT)?;
                        self.index_recheck_completion(issue, &reads)?;
                    }
                    self.line(format!(
                        "project index {}: {}clear {VERDICT} before changing its target",
                        issue.display(),
                        if self.apply { "" } else { "would " }
                    ));
                }
            }
            self.index_write_slot()?;
            if self.apply {
                github.set_field(project, item.context("missing index item")?, &write)?;
            }
            self.line(format!(
                "project index {}: {}{}={}",
                issue.display(),
                if self.apply { "" } else { "would set " },
                write.name,
                write.value
            ));
        }
        Ok(reads)
    }

    fn index_plan(&self, issue: &ItemRef, job: &plan::JobPlan, from: &plan::Comment) -> bool {
        if job.kind == "fix" && self.config.fix_plans {
            // A fix plan names the upstream PR, not the tracking issue where
            // its authenticated plan and receipt live. This is a projection,
            // not a substitute for the pass's live fix-plan admission checks.
            return job.brief.contains(NO_UPSTREAM_COMMENT);
        }
        let on_issue = job.repo.eq_ignore_ascii_case(&issue.repository) && job.item == issue.number;
        (on_issue || job.kind == "implement")
            && self.config.works_in(job.work_repository())
            && (job.kind != plan::SESSION_KIND || plan::written_by(from, &self.config.operator))
    }

    fn index_run(
        &self,
        issue: &ItemRef,
        comments: &[plan::Comment],
        writers: &[&str],
    ) -> Result<Option<String>> {
        let mut plans = BTreeMap::new();
        for c in comments {
            if let Ok((job, from)) = plan::newest_plan(std::slice::from_ref(c), writers)
                && self.index_plan(issue, &job, from)
            {
                plans.insert(plan::plan_id(from)?, (job, from));
            }
        }
        let repo = &self.config.dispatch_repo;
        let prefix = format!("https://github.com/{repo}/actions/runs/");
        let latest = comments
            .iter()
            .filter_map(|record| {
                let (job, from) = plans.get(plan::recorded_plan(&record.body)?)?;
                if record.created < from.changed
                    || !is(record.author.as_deref(), writers)
                    || !trusted_edit(record, writers)
                {
                    return None;
                }
                let primary = batch::recorded_primary(&record.body, issue).filter(|primary| {
                    primary.number > 0
                        && primary.number != issue.number
                        && primary.repository.eq_ignore_ascii_case(&issue.repository)
                        && job.target_repo.is_none()
                });
                let link = plan::recorded_run(&record.body, &from.url, primary.as_ref())?;
                let number = link.strip_prefix(&prefix)?;
                let id = number.parse::<u64>().ok().filter(|id| *id > 0)?;
                (number == id.to_string()).then_some((record, job, primary, link, id))
            })
            .max_by_key(|(record, ..)| (record.created, &record.url));
        let Some((_, job, primary, canonical, id)) = latest else {
            return Ok(None);
        };
        let target = primary.unwrap_or_else(|| ItemRef {
            repository: job.work_repository().into(),
            number: job.item,
            ..issue.clone()
        });
        let response = self
            .client
            .get(&format!("{API_ROOT}repos/{repo}/actions/runs/{id}"), None)?;
        ensure!(
            response.status == 200,
            "reading indexed dispatch run {id}: HTTP {}",
            response.status
        );
        let run = response.body;
        let path = format!(".github/workflows/{}", self.config.dispatch_workflow);
        ensure!(
            run["id"].as_u64() == Some(id)
                && run["html_url"] == canonical
                && run["event"] == "workflow_dispatch"
                && text(&run, "/path").split('@').next() == Some(path.as_str())
                && crate::runs::title_matches(text(&run, "/display_title"), &target, &job.kind),
            "indexed dispatch run {id} did not match its plan and caller workflow"
        );
        Ok(Some(canonical.into()))
    }
}

#[derive(Debug)]
pub(super) struct IndexMoved;

impl std::fmt::Display for IndexMoved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("issue changed while its project index was read")
    }
}

impl std::error::Error for IndexMoved {}
