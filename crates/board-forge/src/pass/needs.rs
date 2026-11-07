//! The needs-you list's own sources (agent-board#479), read where the pass
//! publishes it ([`holds::needs_you`]): the review waits stage 2 decided,
//! the operator-action proposals ([`crate::action`]) on the issues
//! `holds.proposals` labels, and the newest maintain digest
//! ([`maintain::DIGEST_SCHEMA`]); and the bot's summaries of the held
//! issues, which give an ask its question and recommendation. Each read is
//! bounded, and one that fails or stops short says so in the list's
//! `sources`, never as an empty list. Comments count only as their writers'
//! and as last edited by them; what they say is data, cut to one line, and
//! each reference is made from validated parts, never from a link in them.
use super::holds::{
    self, Ask, Collected, DIGEST, NeedsYou, PROPOSALS, REVIEWS, Read, RefKind, Reference, Summary,
};
use super::*;
use crate::action::{self, ACTION_FENCE, Proposal};
use crate::plan::{Comment, comment_id, fenced_json};
use sha2::{Digest, Sha256};

mod carry;
pub(super) mod epics;
pub(super) mod inbox;
pub(super) mod inventory;

/// The most issues labelled for proposals read a pass, oldest first; the
/// rest are unread, and their published asks kept.
const MAX_PROPOSAL_ISSUES: usize = 10;
/// The most proposals listed of one issue.
const MAX_PROPOSALS_PER_ISSUE: usize = 10;
/// The most entries listed of a digest.
const MAX_DIGEST_ASKS: usize = 50;
/// The longest label `holds.proposals.label` may be, as GitHub's.
const LABEL_LIMIT: usize = 50;
/// Requests left before each read for the hold snapshot's own: its reads,
/// its write, and a comment read's pages.
pub(super) const NEEDS_READS: usize = WRITE_RESERVE + HOLDS_READS + plan::PLAN_COMMENT_PAGES;
/// Who wrote and last edited the comment of a REST `node_id`.
pub(super) const EDITS_QUERY: &str = r#"query($id: ID!) {
  node(id: $id) { ... on IssueComment { author { login } lastEditedAt editor { login } } }
}"#;
/// What each digest group asks, and the text field its entry recommends by.
const DIGEST_ASKS: &[(&str, &str, &str, RefKind)] = &[
    (
        "approve",
        "Approve at the assessed head?",
        "reason",
        RefKind::ApprovePullRequest,
    ),
    (
        "closer",
        "Review at the assessed head?",
        "concern",
        RefKind::ReviewPullRequest,
    ),
    (
        "waiting",
        "Reply to the author?",
        "reply",
        RefKind::ReplyPullRequest,
    ),
    (
        "stale",
        "Close as stale?",
        "reason",
        RefKind::ClosePullRequest,
    ),
];

/// `holds.proposals`: the open issues of `repository` labelled `label` hold
/// operator-action proposals ([`action::ACTION_SCHEMA`]) by the operator or
/// the bot, which may act only on `repositories` and on `org`'s settings.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalsAt {
    pub repository: String,
    pub label: String,
    pub repositories: Vec<String>,
    #[serde(default)]
    pub org: Option<String>,
}

impl ProposalsAt {
    pub(super) fn check(&self, config: &Config) -> Result<()> {
        validate_repository(&self.repository).context("holds proposals repository")?;
        let label = &self.label;
        ensure!(
            !label.trim().is_empty()
                && label.chars().count() <= LABEL_LIMIT
                && !label.contains(','),
            "holds proposals label is not 1 to {LABEL_LIMIT} characters without a comma"
        );
        self.config(config).context("holds proposals").map(|_| ())
    }

    /// The operator-action configuration its proposals are checked by.
    fn config(&self, config: &Config) -> Result<action::Config> {
        action::Config::from_json(&json!({
            "writers": [config.operator, config.bot],
            "repositories": self.repositories,
            "org": self.org.clone().unwrap_or_default(),
        }))
    }
}

/// Whether someone other than `writers` edited `comment`, or GitHub cannot
/// name who did.
fn foreign_edit(comment: &Comment, writers: &[&str]) -> bool {
    (comment.edited || comment.editor.is_some()) && !is(comment.editor.as_deref(), writers)
}

fn on_github(repository: &str, number: u64) -> ItemRef {
    ItemRef {
        host: DEFAULT_HOST.into(),
        repository: repository.into(),
        number,
    }
}

fn stamp(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

/// `OWNER/REPO#N` as its repository and number, where both are valid.
fn pull_ref(text: &str) -> Option<(String, u64)> {
    let (repository, number) = text.rsplit_once('#')?;
    let number = number.parse().ok().filter(|n| *n > 0)?;
    validate_repository(repository).ok()?;
    Some((repository.to_owned(), number))
}

/// Stage 2's review waits as asks at `at`: a review read whole, partly
/// (`undecided` repositories), not at all (`followed` none), or off.
pub(super) fn review_asks(
    configured: bool,
    followed: Option<&(Vec<poll::Wait>, BTreeSet<String>)>,
    at: &str,
) -> Collected {
    let Some((waits, undecided)) = followed.filter(|_| configured) else {
        return Collected::of(if configured { Read::Unread } else { Read::Off });
    };
    let mut collected = Collected::of(Read::Complete);
    collected.unread = undecided.clone();
    let mut evidence = BTreeMap::new();
    for wait in waits {
        let head = Some(wait.head.clone()).filter(|h| holds::full_head(h));
        let Some(((repository, number), head)) =
            pull_ref(&format!("{}#{}", wait.repository, wait.number)).zip(head)
        else {
            collected
                .unread
                .insert(wait.repository.to_ascii_lowercase());
            continue;
        };
        let kind = match wait.kind {
            "needs-human" | "promote" => RefKind::PullRequest,
            _ => RefKind::ApprovePullRequest,
        };
        let item = format!("{repository}#{number}");
        if let Some(assessment) = &wait.review_bucket {
            collected
                .review_buckets
                .insert(item.to_ascii_lowercase(), assessment.clone());
        }
        if let Some(read) = wait.approval.as_ref().filter(|e| e.assessed_head == head) {
            evidence.insert(item.to_ascii_lowercase(), read.clone());
        }
        let action = Reference {
            head: Some(head.clone()),
            ..Reference::new(kind, &repository, number)
        };
        collected.asks.push(Ask {
            key: item.to_ascii_lowercase(),
            item,
            source: REVIEWS.into(),
            kind: format!("review-{}", wait.kind),
            priority: None,
            waiting_since: at.to_owned(),
            ask: wait.reason.clone(),
            recommendation: None,
            target: action.url(),
            head: Some(head.clone()),
            source_ref: Reference {
                head: Some(head),
                ..Reference::new(RefKind::PullRequest, &repository, number)
            },
            action,
            carried: false,
            group: None,
            chain: holds::EpicChain::default(),
        });
    }
    if !collected.unread.is_empty() {
        collected.read = Read::Partial;
    }
    collected.approval_evidence = Some(evidence);
    collected
}

/// Stage 2's steps observed at `at` that no person has to take, as the
/// needs-you context: a pull request that moves, waits on what the poll
/// follows, or was held. Read as [`review_asks`] reads the waits; one whose
/// reference does not validate is left out, and the read partial.
pub(super) fn step_facts(
    configured: bool,
    followed: Option<&BTreeSet<String>>,
    stepped: &[poll::Stepped],
    at: &str,
) -> (Read, Vec<holds::ContextFact>) {
    let Some(undecided) = followed.filter(|_| configured) else {
        let read = if configured { Read::Unread } else { Read::Off };
        return (read, Vec::new());
    };
    let mut read = match undecided.is_empty() {
        true => Read::Complete,
        false => Read::Partial,
    };
    let mut facts = Vec::new();
    for step in stepped {
        let progress = step.progress();
        if progress == poll::Progress::Person {
            continue;
        }
        let Some((repository, number)) = pull_ref(&format!("{}#{}", step.repository, step.number))
        else {
            read = Read::Partial;
            continue;
        };
        let reason = format!("{}: {}", step.action, step.reason);
        facts.push(holds::ContextFact {
            source: holds::CONTEXT_STEP,
            item: Some(format!("{repository}#{number}")),
            kind: format!("{progress:?}").to_ascii_lowercase(),
            reason: holds::one_line(&reason, holds::MAX_ASK).unwrap_or_default(),
            url: Some(Reference::new(RefKind::PullRequest, &repository, number).url()),
            since: step.waiting_since().and_then(|t| t.format(&Rfc3339).ok()),
            observed_at: Some(at.to_owned()),
        });
    }
    (read, facts)
}

/// The caller's runs that failed in the last day, `failed` by id where
/// they were listed, as the needs-you context observed at `at`. A run's
/// link is made from its repository and id; its title is data.
pub(super) fn run_facts(
    configured: bool,
    failed: Option<&BTreeMap<u64, Value>>,
    at: &str,
) -> (Read, Vec<holds::ContextFact>) {
    let Some(failed) = failed.filter(|_| configured) else {
        let read = if configured { Read::Unread } else { Read::Off };
        return (read, Vec::new());
    };
    // One page is listed: a full one may leave runs out.
    let mut read = match failed.len() as u64 >= RUNS_PER_PAGE {
        true => Read::Partial,
        false => Read::Complete,
    };
    let mut facts = Vec::new();
    for (id, run) in failed {
        let repository = text(run, "/repository/full_name");
        if validate_repository(repository).is_err() {
            read = Read::Partial;
            continue;
        }
        let title = text(run, "/display_title");
        facts.push(holds::ContextFact {
            source: holds::CONTEXT_RUN,
            item: None,
            kind: Some(text(run, "/conclusion"))
                .filter(|c| !c.is_empty())
                .unwrap_or("failure")
                .to_owned(),
            reason: holds::one_line(title, holds::MAX_ASK).unwrap_or_default(),
            url: Some(format!(
                "https://{DEFAULT_HOST}/{repository}/actions/runs/{id}"
            )),
            since: run["updated_at"]
                .as_str()
                .filter(|t| OffsetDateTime::parse(t, &Rfc3339).is_ok())
                .map(str::to_owned),
            observed_at: Some(at.to_owned()),
        });
    }
    (read, facts)
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    /// What the pass observed besides its holds, for the needs-you context
    /// at `at`; nothing is read again for it.
    fn observed(&self, at: &str) -> holds::Observed {
        let config = self.config;
        let followed = self.followed.as_ref().map(|(_, undecided)| undecided);
        let reruns = config.reruns || config.starts.is_some();
        let usage = match (&config.usage, &self.snapshot) {
            (None, _) => (Read::Off, None),
            (Some(_), Some(Some(snapshot))) => (Read::Complete, Some(snapshot.clone())),
            (Some(_), _) => (Read::Unread, None),
        };
        holds::Observed {
            steps: step_facts(config.follows(), followed, &self.stepped, at),
            runs: run_facts(reruns, self.failed_listed.then_some(&self.failed), at),
            usage,
        }
    }
}

/// The proposals on `issue` among its `comments`, one an ask, and whether
/// all were listed: each `writers`' comment with an operator-action block
/// last edited by them that [`Proposal::from_json`] accepts under `config`.
/// One edited by another account, or invalid, is not listed, and the issue
/// is not read whole.
pub(super) fn proposal_asks(
    issue: &ItemRef,
    comments: &[Comment],
    writers: &[&str],
    config: &action::Config,
) -> (Vec<Ask>, bool) {
    let (mut asks, mut whole) = (Vec::new(), true);
    let item = issue.display();
    for comment in comments.iter().filter(|c| is(c.author.as_deref(), writers)) {
        let value = match fenced_json(&comment.body, ACTION_FENCE) {
            Ok(None) => continue,
            Ok(Some(value)) if !foreign_edit(comment, writers) => value,
            _ => {
                whole = false;
                continue;
            }
        };
        let (Ok(proposal), Some(id)) = (
            Proposal::from_json(&value, config),
            comment_id(&comment.url),
        ) else {
            whole = false;
            continue;
        };
        if asks.len() == MAX_PROPOSALS_PER_ISSUE {
            whole = false;
            break;
        }
        let at = |kind| Reference {
            comment: Some(id),
            ..Reference::new(kind, &issue.repository, issue.number)
        };
        let action = at(RefKind::OperatorAction);
        asks.push(Ask {
            key: format!("{}/comment-{id}", item.to_ascii_lowercase()),
            item: item.clone(),
            source: PROPOSALS.into(),
            kind: "operator-action".into(),
            priority: None,
            waiting_since: stamp(comment.changed),
            ask: holds::one_line(&proposal.title, holds::MAX_ASK).unwrap_or_default(),
            recommendation: holds::one_line(&proposal.why, holds::MAX_ASK),
            target: action.url(),
            head: None,
            source_ref: at(RefKind::Comment),
            action,
            carried: false,
            group: None,
            chain: holds::EpicChain::default(),
        });
    }
    (asks, whole)
}

/// The entries of the newest digest among the digest issue `on`'s
/// `comments` that `bot` wrote and last edited, one an ask on its pull
/// request, and whether all were listed. An entry on a repository outside
/// `scope`, the day's maintain repositories, is not the digest's to list
/// and is left out; one malformed, or past [`MAX_DIGEST_ASKS`], leaves the
/// digest not read whole. No digest is nothing to list.
pub(super) fn digest_asks(
    on: &ItemRef,
    comments: &[Comment],
    bot: &str,
    scope: &BTreeSet<String>,
) -> (Vec<Ask>, bool) {
    let bot = [bot];
    let newest = (comments.iter())
        .filter(|c| is(c.author.as_deref(), &bot) && !foreign_edit(c, &bot))
        .filter_map(|c| {
            let digest = fenced_json(&c.body, maintain::DIGEST_FENCE).ok()??;
            (digest["schema"] == maintain::DIGEST_SCHEMA).then_some((c, digest))
        })
        .max_by_key(|(c, _)| (c.changed, comment_id(&c.url)));
    let Some((comment, digest)) = newest else {
        return (Vec::new(), true);
    };
    let Some(id) = comment_id(&comment.url) else {
        return (Vec::new(), false);
    };
    let (mut asks, mut whole) = (Vec::<Ask>::new(), true);
    for (group, question, field, kind) in DIGEST_ASKS {
        let headed = maintain::DIGEST_GROUPS.contains(&(group, true));
        let Some(entries) = digest[*group].as_array() else {
            whole = false;
            continue;
        };
        for entry in entries {
            let Some((repository, number)) = entry["pr"].as_str().and_then(pull_ref) else {
                whole = false;
                continue;
            };
            if !scope.contains(&repository.to_ascii_lowercase()) {
                continue;
            }
            let head = entry["head"].as_str().filter(|h| holds::full_head(h));
            if headed && head.is_none() {
                whole = false;
                continue;
            }
            let item = format!("{repository}#{number}");
            let key = format!("{}/{DIGEST}", item.to_ascii_lowercase());
            if asks.iter().any(|a| a.key == key) {
                continue;
            }
            if asks.len() == MAX_DIGEST_ASKS {
                return (asks, false);
            }
            let head = head.filter(|_| headed).map(str::to_owned);
            let action = Reference {
                head: head.clone(),
                ..Reference::new(*kind, &repository, number)
            };
            asks.push(Ask {
                key,
                item,
                source: DIGEST.into(),
                kind: format!("digest-{group}"),
                priority: None,
                waiting_since: stamp(comment.changed),
                ask: (*question).into(),
                recommendation: (entry[*field].as_str())
                    .and_then(|t| holds::one_line(t, holds::MAX_ASK)),
                target: action.url(),
                head,
                source_ref: Reference {
                    comment: Some(id),
                    ..Reference::new(RefKind::Comment, &on.repository, on.number)
                },
                action,
                carried: false,
                group: None,
                chain: holds::EpicChain::default(),
            });
        }
    }
    (asks, whole)
}

/// The newest summary among `comments` that `bot` wrote and last edited
/// that asks or recommends something ([`holds::summary`]).
pub(super) fn bot_summary(comments: &[Comment], bot: &str) -> Option<Summary> {
    let bot = [bot];
    (comments.iter())
        .filter(|c| is(c.author.as_deref(), &bot) && !foreign_edit(c, &bot))
        .map(|c| (c, holds::summary(&c.body)))
        .filter(|(_, s)| s.question.is_some() || s.recommendation.is_some())
        .max_by_key(|(c, _)| (c.changed, comment_id(&c.url)))
        .map(|(_, s)| s)
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    /// Whether the pass has the requests and time for one more of these
    /// reads, with the hold snapshot's own left.
    fn room(&self) -> bool {
        let bounds = self.client.bounds;
        bounds.left.get() > NEEDS_READS && Instant::now() <= bounds.admit_by
    }

    /// The needs-you list of `snapshot`, reading its own sources and then
    /// the summaries of the highest ranked held issues; a read that fails
    /// is one `needs-you …` line, and the list says what was not read.
    pub(super) fn needs_you(
        &mut self,
        snapshot: &mut holds::Snapshot,
        previous: Option<&Value>,
        on: &ItemRef,
    ) -> NeedsYou {
        // Reserve cheap identity validation before any optional source body,
        // summary or descendant activity reads can exhaust the pass.
        let mut starts: Vec<_> = snapshot
            .holds
            .iter()
            .filter_map(|h| h.item.as_deref())
            .filter_map(|i| pull_ref(i).map(|(r, n)| on_github(&r, n)))
            .collect();
        if let Some(old) = previous.and_then(|p| {
            let list = &p["needs_you"];
            list.get("identities").unwrap_or(&list["items"]).as_array()
        }) {
            starts.extend(
                old.iter()
                    .filter_map(|i| i["item"].as_str())
                    .filter_map(|i| pull_ref(i).map(|(r, n)| on_github(&r, n))),
            );
        }
        let mut attempted = BTreeSet::new();
        let mut nodes = BTreeMap::new();
        self.inbox_chains(starts, &mut nodes, &mut attempted);
        let filtered = carry::retire(snapshot, previous, &nodes);
        let previous = filtered.as_ref();
        let options = match &self.board {
            Some(Some(board)) => board.options.clone(),
            _ => Vec::new(),
        };
        let config = self.config;
        let mut reviews = review_asks(config.follows(), self.followed.as_ref(), &snapshot.at);
        if !config.holds.as_ref().is_some_and(|h| h.approval_evidence) {
            reviews.approval_evidence = None;
        }
        let proposals = match config.holds.as_ref().and_then(|h| h.proposals.as_ref()) {
            Some(at) => self.proposals(at),
            None => Collected::off(),
        };
        let digest = match &config.maintain {
            Some(maintain) => self.newest_digest(maintain),
            None => Collected::off(),
        };
        let mut reading = holds::Reading {
            operator: &config.operator,
            epics: config.epics.is_some(),
            considered: self.unconsidered == 0,
            epics_whole: self.deferred_epics.is_empty(),
            options: &options,
            on,
            reviews,
            proposals,
            digest,
            inbox: self.inbox(),
            observed: self.observed(&snapshot.at),
            summaries: (Read::Complete, BTreeMap::new()),
        };
        let list = holds::needs_you(snapshot, previous, &reading);
        let proposal_label = config
            .holds
            .as_ref()
            .and_then(|h| h.proposals.as_ref())
            .map(|p| p.label.as_str());
        let starts = list
            .identities
            .iter()
            .filter(|i| {
                !nodes.contains_key(&i.item.to_ascii_lowercase())
                    && !attempted.contains(&i.item.to_ascii_lowercase())
            })
            .filter_map(|i| pull_ref(&i.item).map(|(r, n)| on_github(&r, n)))
            .collect();
        self.inbox_chains(starts, &mut nodes, &mut attempted);
        carry::sources(&mut reading, &nodes);
        let filtered = carry::retire(snapshot, previous, &nodes);
        let previous = filtered.as_ref();
        let mut list = holds::needs_you(snapshot, previous, &reading);
        inventory::revalidate(&mut list, &nodes, proposal_label);
        self.inbox_epics(&mut list, &nodes, false);
        reading.summaries = self.summaries(&list);
        let mut list = holds::needs_you(snapshot, previous, &reading);
        inventory::revalidate(&mut list, &nodes, proposal_label);
        self.inbox_epics(&mut list, &nodes, true);
        carry::evidence(&mut list, previous, &nodes);
        list.retain_evidence(reading.reviews.approval_evidence.as_ref());
        if config.holds.as_ref().is_some_and(|h| h.act_now) {
            list.rank_act_now(&snapshot.holds);
        }
        holds::preserve_changed_at(&mut list, previous);
        list
    }

    /// Whether the published hold `comment`, as the REST API lists it, is
    /// the bot's as it last edited it, so that its needs-you list may carry
    /// over: anyone with write access may edit the bot's comment, and so
    /// plant asks the bot never read. One that cannot be checked is not.
    pub(super) fn bot_edited(&mut self, comment: &Value) -> bool {
        // One read, before the other sources': skipped, nothing they did
        // not read could be carried.
        let bounds = self.client.bounds;
        let room =
            bounds.left.get() > WRITE_RESERVE + HOLDS_READS && Instant::now() <= bounds.admit_by;
        let read = match (comment["node_id"].as_str(), room) {
            (Some(id), true) => self.client.graphql(EDITS_QUERY, json!({"id": id})),
            (None, _) => Err(anyhow::anyhow!("it has no node id")),
            (_, false) => Err(anyhow::anyhow!(
                "the pass's request budget or time is spent"
            )),
        };
        let why = match read {
            Ok(data) => {
                let (node, bot) = (&data["node"], [self.config.bot.as_str()]);
                let login = |field: &str| node[field]["login"].as_str();
                let edited = !node["lastEditedAt"].is_null() || login("editor").is_some();
                match is(login("author"), &bot) && (!edited || is(login("editor"), &bot)) {
                    true => return true,
                    false => "it is not the bot's as last edited".to_owned(),
                }
            }
            Err(error) => format!("{error:#}"),
        };
        self.line(format!(
            "needs-you: the published list carries nothing over: {why}"
        ));
        false
    }

    /// The proposals on the open issues `at` labels, oldest first.
    fn proposals(&mut self, at: &ProposalsAt) -> Collected {
        let (config, bot) = (self.config, self.config.bot.as_str());
        let writers = [config.operator.as_str(), bot];
        let mut collected = Collected::of(Read::Complete);
        // Skipped, it is read in part: what it published is kept.
        if !self.room() {
            let why = "the pass's request budget or time is spent";
            self.line(format!("needs-you proposals error: {why}"));
            collected.read = Read::Partial;
            collected.unread.insert(at.repository.to_ascii_lowercase());
            return collected;
        }
        let label: String = url::form_urlencoded::byte_serialize(at.label.as_bytes()).collect();
        let url = format!(
            "{API_ROOT}repos/{}/issues?state=open&labels={label}&sort=created&direction=asc&per_page=100",
            at.repository
        );
        let listed = at.config(config).and_then(|checks| {
            let response = self.client.get(&url, None)?;
            ensure!(
                response.status == 200,
                "GitHub HTTP status {}",
                response.status
            );
            let issues = response.body.as_array().context("issues are not a list")?;
            Ok((checks, issues.clone()))
        });
        let (checks, issues) = match listed {
            Ok(listed) => listed,
            Err(error) => {
                self.line(format!("needs-you proposals error: {error:#}"));
                return Collected::of(Read::Unread);
            }
        };
        // A full page may not be all of them.
        if issues.len() >= 100 {
            collected.read = Read::Partial;
            collected.unread.insert(at.repository.to_ascii_lowercase());
        }
        let numbers = (issues.iter())
            .filter(|i| i.get("pull_request").is_none())
            .filter_map(|i| i["number"].as_u64());
        for (n, number) in numbers.enumerate() {
            let issue = on_github(&at.repository, number);
            let comments = match n < MAX_PROPOSAL_ISSUES && self.room() {
                true => plan::issue_comments(&self.client, &issue).map_err(|e| {
                    self.line(format!(
                        "needs-you proposals error: {}: {e:#}",
                        issue.display()
                    ))
                }),
                false => Err(()),
            };
            let Ok(comments) = comments else {
                collected.read = Read::Partial;
                collected.unread.insert(key(&issue));
                continue;
            };
            let (asks, whole) = proposal_asks(&issue, &comments, &writers, &checks);
            if !whole {
                collected.read = Read::Partial;
            }
            collected.asks.extend(asks);
        }
        collected
    }

    /// The newest digest's entries, within the day's maintain repositories;
    /// unread where the pass knows no day.
    fn newest_digest(&mut self, maintain: &maintain::Maintain) -> Collected {
        let scope: Option<BTreeSet<String>> = (self.report.maintain.as_ref())
            .map(|day| day.repos.iter().map(|r| r.to_ascii_lowercase()).collect());
        let on = on_github(&maintain.digest.repository, maintain.digest.issue);
        let read = match (scope, self.room()) {
            (Some(scope), true) => plan::issue_comments(&self.client, &on).map(|c| (c, scope)),
            // Maintain already reported the failure. With no known scope,
            // keep all previously published digest asks until we can read it.
            (None, _) => return Collected::of(Read::Unread),
            (_, false) => Err(anyhow::anyhow!(
                "the pass's request budget or time is spent"
            )),
        };
        match read {
            Ok((comments, scope)) => {
                let (asks, whole) = digest_asks(&on, &comments, &self.config.bot, &scope);
                Collected {
                    review_buckets: BTreeMap::new(),
                    approval_evidence: None,
                    read: if whole { Read::Complete } else { Read::Partial },
                    asks,
                    unread: BTreeSet::new(),
                }
            }
            Err(error) => {
                self.line(format!("needs-you digest error: {error:#}"));
                Collected::of(Read::Unread)
            }
        }
    }

    /// Every listed hold's summary, within the pass's request/time bounds.
    /// Metadata is checked before reusing a body, including writers and
    /// editors: a foreign edit must invalidate a previously trusted summary.
    fn summaries(&mut self, list: &NeedsYou) -> (Read, BTreeMap<String, Option<Summary>>) {
        let (mut read, mut said) = (Read::Complete, BTreeMap::new());
        let held = (list.items.iter())
            .filter(|a| a.source_ref.kind == RefKind::Hold && a.source != "pass");
        for ask in held {
            let issue = on_github(&ask.source_ref.repository, ask.source_ref.number);
            if !self.room()
                || self.client.bounds.left.get() <= NEEDS_READS + plan::PLAN_COMMENT_PAGES
            {
                read = Read::Partial;
                break;
            }
            // A blocking hold is on a pull request, whose summary an issue
            // read cannot find.
            let pull = ask.action.kind == RefKind::PullRequest;
            let summary = cached_summary(
                &self.client,
                &issue,
                pull,
                &self.config.bot,
                list.summary_cache.get(&ask.key),
            );
            match summary {
                Ok(summary) => {
                    said.insert(ask.key.clone(), Some(summary));
                }
                Err(error) => {
                    read = Read::Partial;
                    let item = issue.display();
                    self.line(format!("needs-you summary error: {item}: {error:#}"));
                }
            }
        }
        (read, said)
    }
}

/// Hash all bounded comment metadata, not just the selected summary: a new
/// comment, deletion or edit to any candidate must invalidate the cache.
fn revision(comments: &[Comment], bot: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(bot.to_ascii_lowercase());
    for c in comments {
        hash.update(format!(
            "{:?}",
            (&c.url, c.changed, &c.author, c.edited, &c.editor)
        ));
    }
    format!("{:x}", hash.finalize())
}

fn cached_summary<T: Transport>(
    client: &T,
    issue: &ItemRef,
    pull: bool,
    bot: &str,
    cached: Option<&Summary>,
) -> Result<Summary> {
    let metadata = plan::comment_metadata(client, issue, pull)?;
    let current = revision(&metadata, bot);
    if let Some(cached) = cached.filter(|s| s.revision.as_deref() == Some(current.as_str())) {
        return Ok(cached.clone());
    }
    let comments = if pull {
        plan::pull_request_comments(client, issue)?
    } else {
        plan::issue_comments(client, issue)?
    };
    let mut summary = bot_summary(&comments, bot).unwrap_or_default();
    // Use the body's own metadata, so an intervening edit cannot cache a
    // stale body under the revision observed before it was downloaded.
    summary.revision = Some(revision(&comments, bot));
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOT: &str = "cgwalters-bot";
    const HEAD: &str = "488f45ebacd03a8bc88dceb3d703a5322850ce77";

    #[test]
    fn summary_cache_revalidates_edits_deletions_and_writers() {
        struct Comments {
            nodes: Vec<Value>,
            bodies: std::cell::Cell<usize>,
        }
        impl Transport for Comments {
            fn graphql(&self, query: &str, _: Value) -> Result<Value> {
                let body = query.contains(" body ");
                if body {
                    self.bodies.set(self.bodies.get() + 1);
                }
                let nodes: Vec<_> = self
                    .nodes
                    .iter()
                    .cloned()
                    .map(|mut n| {
                        if !body {
                            n.as_object_mut().unwrap().remove("body");
                        }
                        n
                    })
                    .collect();
                Ok(json!({"repository": {"issue": {"comments": {"nodes": nodes,
                    "pageInfo": {"hasNextPage": false, "endCursor": null}}}}}))
            }
        }
        let node = json!({"url": "https://github.com/o/r/issues/1#issuecomment-1",
            "body": "Question for the operator: Ship?\nRecommended: Yes.",
            "createdAt": "2026-10-28T08:00:00Z", "lastEditedAt": null,
            "author": {"login": BOT}, "editor": null});
        let mut client = Comments {
            nodes: vec![node.clone()],
            bodies: std::cell::Cell::new(0),
        };
        let issue = on_github("o/r", 1);
        let cached = cached_summary(&client, &issue, false, BOT, None).unwrap();
        assert_eq!(cached.question.as_deref(), Some("Ship?"));
        assert_eq!(client.bodies.get(), 1);
        assert_eq!(
            cached_summary(&client, &issue, false, BOT, Some(&cached)).unwrap(),
            cached
        );
        assert_eq!(
            client.bodies.get(),
            1,
            "unchanged body must not be read again"
        );
        let changed = |patch: Value| {
            let mut n = node.clone();
            n.as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            n
        };
        for (name, nodes, question) in [
            (
                "edit",
                vec![changed(
                    json!({"lastEditedAt": "2026-10-28T09:00:00Z", "editor": {"login": BOT},
                "body": "Decision needed: Wait?"}),
                )],
                Some("Wait?"),
            ),
            (
                "foreign edit",
                vec![changed(
                    json!({"lastEditedAt": "2026-10-28T09:00:00Z", "editor": {"login": "visitor"}}),
                )],
                None,
            ),
            ("deletion", vec![], None),
            (
                "new summary",
                vec![
                    node.clone(),
                    changed(
                        json!({"url": "https://github.com/o/r/issues/1#issuecomment-2",
                "createdAt": "2026-10-28T09:00:00Z", "body": "Decision needed: Wait?"}),
                    ),
                ],
                Some("Wait?"),
            ),
        ] {
            client.nodes = nodes;
            let before = client.bodies.get();
            let said = cached_summary(&client, &issue, false, BOT, Some(&cached)).unwrap();
            assert_eq!(said.question.as_deref(), question, "{name}");
            assert_eq!(client.bodies.get(), before + 1, "{name}");
        }
    }

    /// Comment `id` by `author`, edited by `editor` if any, `minutes` after
    /// 08:00 on 28 October.
    pub(super) fn comment(
        id: u64,
        (author, editor): (&str, Option<&str>),
        minutes: i64,
        body: &str,
    ) -> Comment {
        let at = OffsetDateTime::parse("2026-10-28T08:00:00Z", &Rfc3339).unwrap();
        Comment {
            url: format!("https://github.com/o/ops/issues/5#issuecomment-{id}"),
            body: body.into(),
            author: Some(author.into()),
            edited: editor.is_some(),
            editor: editor.map(Into::into),
            created: at + time::Duration::minutes(minutes),
            changed: at + time::Duration::minutes(minutes),
        }
    }

    /// Each review wait is one ask at its head, approving where the
    /// operator approves; the source is unread where stage 2 did not run,
    /// partial where it left a repository undecided, and off without
    /// followed repositories. A head that is not a full commit is no ask,
    /// and leaves its repository unread.
    #[test]
    fn review_waits() {
        let wait = |number, kind, head: &str| poll::Wait {
            review_bucket: None,
            approval: None,
            repository: "cgwalters-bot/praxis-credential-broker".into(),
            number,
            head: head.into(),
            kind,
            reason: format!("{kind} reason"),
        };
        let waits = vec![
            wait(1, "approve", HEAD),
            wait(2, "withheld", HEAD),
            wait(3, "needs-human", HEAD),
        ];
        let at = "2026-10-28T09:00:00Z";
        let followed = (waits.clone(), BTreeSet::new());
        let collected = review_asks(true, Some(&followed), at);
        assert_eq!(collected.read, Read::Complete);
        let asks: Vec<Value> = (collected.asks.iter())
            .map(|a| {
                json!([
                    a.key,
                    a.kind,
                    a.action.kind,
                    a.head,
                    a.waiting_since,
                    a.target
                ])
            })
            .collect();
        let pull =
            |n| format!("https://github.com/cgwalters-bot/praxis-credential-broker/pull/{n}");
        let key = |n| format!("cgwalters-bot/praxis-credential-broker#{n}");
        assert_eq!(
            asks,
            [
                json!([
                    key(1),
                    "review-approve",
                    "approve_pull_request",
                    HEAD,
                    at,
                    pull(1)
                ]),
                json!([
                    key(2),
                    "review-withheld",
                    "approve_pull_request",
                    HEAD,
                    at,
                    pull(2)
                ]),
                json!([
                    key(3),
                    "review-needs-human",
                    "pull_request",
                    HEAD,
                    at,
                    pull(3)
                ]),
            ]
        );
        assert_eq!(review_asks(false, Some(&followed), at).read, Read::Off);
        assert_eq!(review_asks(true, None, at).read, Read::Unread);
        let undecided = (waits, ["o/r".to_owned()].into());
        let partial = review_asks(true, Some(&undecided), at);
        assert_eq!((partial.read, partial.asks.len()), (Read::Partial, 3));
        let short = (vec![wait(4, "approve", "488f45e")], BTreeSet::new());
        let partial = review_asks(true, Some(&short), at);
        assert_eq!(partial.read, Read::Partial);
        assert!(partial.asks.is_empty());
        assert!(
            partial
                .unread
                .contains("cgwalters-bot/praxis-credential-broker")
        );
    }

    /// Steps no person takes, and failed runs, are context with their
    /// progress or conclusion and links made from validated parts; a step
    /// only a person takes is an ask, not context. Each source is off,
    /// unread, partial or complete as stage 2 and the run listing were.
    #[test]
    fn context_steps_and_runs() {
        let at = "2026-10-28T09:00:00Z";
        let step = |repository: &str, number, action, reason: &str, ran| poll::Stepped {
            repository: repository.into(),
            number,
            head: HEAD.into(),
            action,
            reason: reason.into(),
            ran,
            moved_at: OffsetDateTime::parse("2026-10-28T08:00:00Z", &Rfc3339).ok(),
            waiting: if action == "wait" {
                poll::WaitReason::Followed { since: None }
            } else {
                poll::WaitReason::Person
            },
        };
        let stepped = [
            step("o/r", 1, "wait", "checks pending: build", false),
            step("o/r", 2, "merge", "checks green", true),
            step("o/r", 3, "dispatch", "not paced", false),
            step("o/r", 4, "needs-human", "labelled", false),
            step("bad repo", 5, "merge", "checks green", true),
        ];
        let none = BTreeSet::new();
        let (read, facts) = step_facts(true, Some(&none), &stepped, at);
        assert_eq!(read, Read::Partial);
        let got: Vec<_> = (facts.iter())
            .map(|f| json!([f.item, f.kind, f.reason, f.url, f.since]))
            .collect();
        let pull = |n| format!("https://github.com/o/r/pull/{n}");
        let since = "2026-10-28T08:00:00Z";
        assert_eq!(
            got,
            [
                json!([
                    "o/r#1",
                    "waiting",
                    "wait: checks pending: build",
                    pull(1),
                    since
                ]),
                json!(["o/r#2", "moving", "merge: checks green", pull(2), since]),
                json!(["o/r#3", "held", "dispatch: not paced", pull(3), since]),
            ]
        );
        assert_eq!(
            step_facts(true, Some(&none), &stepped[..3], at).0,
            Read::Complete
        );
        let undecided = ["o/x".to_owned()].into();
        assert_eq!(step_facts(true, Some(&undecided), &[], at).0, Read::Partial);
        assert_eq!(step_facts(true, None, &stepped, at), (Read::Unread, vec![]));
        assert_eq!(
            step_facts(false, Some(&none), &stepped, at),
            (Read::Off, vec![])
        );

        let run = |repository: &str| {
            json!({"repository": {"full_name": repository}, "conclusion": "failure",
                "display_title": "dispatch agent-board#7\nmore", "html_url": "https://evil.example/",
                "updated_at": "2026-10-28T07:00:00Z"})
        };
        let failed = BTreeMap::from([(7, run("o/tracker")), (8, run("../x"))]);
        let (read, facts) = run_facts(true, Some(&failed), at);
        assert_eq!(read, Read::Partial);
        let got: Vec<_> = (facts.iter())
            .map(|f| json!([f.source, f.kind, f.reason, f.url, f.since, f.observed_at]))
            .collect();
        assert_eq!(
            got,
            [json!([
                holds::CONTEXT_RUN,
                "failure",
                "dispatch agent-board#7",
                "https://github.com/o/tracker/actions/runs/7",
                "2026-10-28T07:00:00Z",
                at
            ])]
        );
        let one = BTreeMap::from([(7, run("o/tracker"))]);
        assert_eq!(run_facts(true, Some(&one), at).0, Read::Complete);
        let page: BTreeMap<_, _> = (0..RUNS_PER_PAGE)
            .map(|id| (id, run("o/tracker")))
            .collect();
        assert_eq!(run_facts(true, Some(&page), at).0, Read::Partial);
        assert_eq!(run_facts(true, None, at), (Read::Unread, vec![]));
        assert_eq!(run_facts(false, Some(&one), at), (Read::Off, vec![]));
    }

    /// Each writer's valid proposal on an issue is its own ask, keyed by
    /// its comment; a visitor's is none, and one edited by another account,
    /// invalid or acting outside the configured repositories is not listed
    /// and leaves the issue not read whole.
    #[test]
    fn proposals_one_an_ask() {
        let config = action::Config::from_json(&json!({
            "writers": ["cgwalters", BOT],
            "repositories": ["o/ops", "cgwalters-bot/praxis-credential-broker"],
        }))
        .unwrap();
        let block = |title: &str, repo: &str| {
            let action = json!({"type": "add_label", "repo": repo, "number": 3, "label": "ready"});
            let value = json!({"schema": action::ACTION_SCHEMA, "title": title,
                "why": "Because.\nMore.", "actions": [action]});
            format!("Proposed.\n\n```{ACTION_FENCE}\n{value}\n```\n")
        };
        let broker = "cgwalters-bot/praxis-credential-broker";
        let issue = on_github("o/ops", 5);
        let writers = ["cgwalters", BOT];
        let three = vec![
            comment(1, (BOT, None), 0, &block("First", "o/ops")),
            comment(2, ("cgwalters", None), 1, &block("Second", broker)),
            comment(3, (BOT, Some(BOT)), 2, &block("Third", "o/ops")),
            comment(4, ("visitor", None), 3, &block("Visitor", "o/ops")),
            comment(5, (BOT, None), 4, "No proposal here."),
        ];
        let (asks, whole) = proposal_asks(&issue, &three, &writers, &config);
        assert!(whole);
        let listed: Vec<Value> = (asks.iter())
            .map(|a| {
                json!([
                    a.key,
                    a.ask,
                    a.recommendation,
                    a.action.comment,
                    a.waiting_since
                ])
            })
            .collect();
        assert_eq!(
            listed,
            [
                json!([
                    "o/ops#5/comment-1",
                    "First",
                    "Because.",
                    1,
                    "2026-10-28T08:00:00Z"
                ]),
                json!([
                    "o/ops#5/comment-2",
                    "Second",
                    "Because.",
                    2,
                    "2026-10-28T08:01:00Z"
                ]),
                json!([
                    "o/ops#5/comment-3",
                    "Third",
                    "Because.",
                    3,
                    "2026-10-28T08:02:00Z"
                ]),
            ]
        );
        assert_eq!(
            asks[0].target,
            "https://github.com/o/ops/issues/5#issuecomment-1"
        );
        for refused in [
            comment(6, (BOT, Some("visitor")), 5, &block("Edited", "o/ops")),
            comment(6, (BOT, None), 5, &block("Elsewhere", "elsewhere/r")),
            comment(
                6,
                (BOT, None),
                5,
                "```agent-board-action\n{\"schema\": 1}\n```",
            ),
        ] {
            let mut comments = three.clone();
            comments.push(refused.clone());
            let (asks, whole) = proposal_asks(&issue, &comments, &writers, &config);
            assert_eq!((asks.len(), whole), (3, false), "{}", refused.body);
        }
    }

    /// The newest digest the bot wrote and last edited is read, each entry
    /// an ask on its pull request with the group's action, at its head where
    /// its group names one; an entry outside the day's repositories is left
    /// out, and one without a full head leaves the digest not read whole.
    #[test]
    fn digest_entries() {
        let digest = include_str!("../../../../fixtures/pass/maintain-digest.md");
        let on = on_github("o/ops", 9);
        let scope: BTreeSet<String> = ["up/proj".to_owned()].into();
        let older = digest.replace("up/proj#7", "up/proj#70");
        let comments = vec![
            comment(1, (BOT, None), 0, &older),
            comment(2, (BOT, None), 5, digest),
            comment(3, (BOT, Some("visitor")), 9, &older),
            comment(4, ("visitor", None), 9, &older),
        ];
        let (asks, whole) = digest_asks(&on, &comments, BOT, &scope);
        assert!(whole);
        let listed: Vec<Value> = (asks.iter())
            .map(|a| json!([a.key, a.kind, a.action.kind, a.head, a.recommendation]))
            .collect();
        let head = |d: char| d.to_string().repeat(40);
        assert_eq!(
            listed,
            [
                json!([
                    "up/proj#7/digest",
                    "digest-approve",
                    "approve_pull_request",
                    head('1'),
                    "Dependency bump, lockfile only; all checks green at this head."
                ]),
                json!([
                    "up/proj#8/digest",
                    "digest-approve",
                    "approve_pull_request",
                    head('2'),
                    "Typo fix in docs; all checks green at this head."
                ]),
                json!([
                    "up/proj#9/digest",
                    "digest-closer",
                    "review_pull_request",
                    head('3'),
                    "Changes the lock order."
                ]),
                json!([
                    "up/proj#10/digest",
                    "digest-waiting",
                    "reply_pull_request",
                    null,
                    "Thanks! Could you rebase on main and add a test?"
                ]),
                json!([
                    "up/proj#3/digest",
                    "digest-stale",
                    "close_pull_request",
                    null,
                    "No activity in 90 days."
                ]),
            ]
        );
        let first = &asks[0];
        assert_eq!(first.target, "https://github.com/up/proj/pull/7");
        assert_eq!(first.source_ref.comment, Some(2));
        assert_eq!(first.waiting_since, "2026-10-28T08:05:00Z");
        let unheaded = digest.replace(&format!("\"head\": \"{}\"", head('1')), "\"head\": \"1\"");
        let (asks, whole) = digest_asks(&on, &[comment(2, (BOT, None), 5, &unheaded)], BOT, &scope);
        assert_eq!((asks.len(), whole), (4, false));
        let (asks, whole) = digest_asks(&on, &comments[3..], BOT, &scope);
        assert_eq!((asks.len(), whole), (0, true), "no digest of the bot's");
    }

    /// The newest summary the bot wrote and last edited that asks or
    /// recommends something; another account's, or one edited by another,
    /// is not read.
    #[test]
    fn bot_summaries() {
        let asks = |q: &str| format!("**Decision needed:** {q}");
        let comments = vec![
            comment(1, (BOT, None), 0, &asks("Old?")),
            comment(2, (BOT, None), 1, &asks("New?")),
            comment(
                3,
                (BOT, None),
                2,
                "**Decision needed:**\n**Recommendation:**",
            ),
            comment(4, (BOT, Some("visitor")), 3, &asks("Edited?")),
            comment(5, ("visitor", None), 4, &asks("Visitor?")),
        ];
        let said = bot_summary(&comments, BOT).unwrap();
        assert_eq!(said.question.as_deref(), Some("New?"));
        assert_eq!(bot_summary(&comments[2..], BOT), None);
    }

    /// The shared consumer fixture is what the producer publishes for its
    /// rows: current evidence, evidence of another head left out, and one
    /// row's evicted for size and counted. A row is current only where its
    /// entry's head is the row's.
    #[test]
    fn approval_evidence_matches_the_shared_fixture() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../fixtures/approval-evidence-needs-you.json"
        ))
        .unwrap();
        let expected = &fixture["approval_evidence"];
        let at = expected["observed_at"].as_str().unwrap();
        let rows = fixture["rows"].as_array().unwrap();
        let waits = rows
            .iter()
            .map(|row| {
                let key = row["key"].as_str().unwrap();
                let (repository, number) = pull_ref(key).unwrap();
                let head = row["head"].as_str().unwrap();
                let read_at = row["read_at"].as_str().unwrap_or(head);
                let approval = match expected["items"].get(key) {
                    Some(e) => serde_json::from_value(e.clone()).unwrap(),
                    None => poll::approval::Evidence {
                        assessed_head: read_at.into(),
                        diff: poll::approval::Diff::Lines {
                            additions: 1,
                            deletions: 0,
                        },
                        ci: poll::approval::Ci {
                            state: poll::approval::CiState::Green,
                            complete: true,
                        },
                        review: None,
                    },
                };
                poll::Wait {
                    repository,
                    number,
                    head: head.into(),
                    kind: "approve",
                    reason: "approve this head?".into(),
                    review_bucket: None,
                    approval: Some(approval),
                }
            })
            .collect();
        let on = on_github("a/ops", 1);
        let reading = holds::Reading {
            operator: BOT,
            epics: false,
            considered: true,
            epics_whole: true,
            options: &[],
            on: &on,
            reviews: review_asks(true, Some(&(waits, BTreeSet::new())), at),
            proposals: Collected::off(),
            digest: Collected::off(),
            inbox: BTreeMap::new(),
            observed: holds::Observed::off(),
            summaries: (Read::Off, BTreeMap::new()),
        };
        let mut snapshot = holds::snapshot(&[], at.into(), Default::default());
        let list = holds::needs_you(&snapshot, None, &reading);
        assert_eq!(list.items.len(), rows.len());
        snapshot.needs_you = Some(list);
        snapshot.holds.push(holds::Published {
            item: None,
            url: None,
            epic: false,
            kind: holds::HoldKind::Wip,
            reason: String::new(),
            operator: false,
            priority: None,
            question: None,
            since: at.into(),
            carried: false,
            watched: false,
            releases: 1,
        });
        snapshot.holds[0].reason = "x".into();
        let size = holds::render(&snapshot).unwrap().chars().count();
        snapshot.holds[0].reason.push('x');
        let per_char = holds::render(&snapshot).unwrap().chars().count() - size;
        let pad = (holds::MAX_COMMENT_CHARS - size) / per_char + 2;
        snapshot.holds[0].reason = "x".repeat(pad);
        let (fitted, body) = holds::fit(snapshot).unwrap();
        assert!(body.chars().count() <= holds::MAX_COMMENT_CHARS);
        assert_eq!(fitted.truncated, 0);
        let parsed = holds::parse(&body).unwrap();
        let published = &parsed["needs_you"];
        assert_eq!(published["schema"], fixture["schema"]);
        assert_eq!(&published["approval_evidence"], expected);
        // Older v2 publications and unknown extensions still carry the same
        // decision inventory; neither supplies fresh approval observations.
        for extension in [None, Some(json!({"schema":"future/v99","items":{}}))] {
            let mut previous = parsed.clone();
            let list = previous["needs_you"].as_object_mut().unwrap();
            list.remove("approval_evidence");
            if let Some(extension) = extension {
                list.insert("approval_evidence".into(), extension);
            }
            let unread = holds::Reading {
                reviews: Collected::of(Read::Unread),
                proposals: Collected::off(),
                digest: Collected::off(),
                inbox: BTreeMap::new(),
                observed: holds::Observed::off(),
                summaries: (Read::Off, BTreeMap::new()),
                ..reading
            };
            let snapshot = holds::snapshot(&[], at.into(), Default::default());
            let carried = holds::needs_you(&snapshot, Some(&previous), &unread);
            assert_eq!(carried.count, rows.len());
            assert!(carried.items.iter().all(|a| a.carried));
            assert!(carried.approval_evidence.is_none());
        }
        for row in rows {
            let entry = &published["approval_evidence"]["items"][row["key"].as_str().unwrap()];
            let current = entry["assessed_head"] == row["head"];
            let shown = row["shown"].as_str().unwrap();
            assert_eq!(current, !shown.starts_with("unknown"), "{shown}");
            if current {
                // Consumers read it back strictly.
                serde_json::from_value::<poll::approval::Evidence>(entry.clone()).unwrap();
            }
        }
    }

    /// The `pr.operatorPaths` approval wait [`approval_board`] ranks last.
    const LATE_APPROVAL: &str = "o/zeta#174";

    /// A derived board, not a recording: the shape of tracker#976's inbox
    /// of 174 decisions at `at`. 40 operator holds with long questions and
    /// mixed Priorities, 121 operator-action proposals with long asks and
    /// recommendations, two groups of stranded pull requests (30 members),
    /// ten labelled approvals and, ranked after the other 173 decisions as
    /// it has no Priority and the latest key, a `pr.operatorPaths` approval
    /// wait; beside them 30 holds that wait on no one. Its list is read
    /// from stage 2's waits as a pass reads them, after `previous`.
    fn approval_board(at: &str, previous: Option<&Value>) -> holds::Snapshot {
        let head = |n: u64| format!("{n:040x}");
        let wait = |repository: &str, n: u64, kind, reason: &str| poll::Wait {
            repository: repository.into(),
            number: n,
            head: head(n),
            kind,
            reason: reason.into(),
            review_bucket: None,
            approval: None,
        };
        let mut waits: Vec<_> = (1..=20)
            .map(|n| wait("o/stranded", n, "needs-human", "fix rounds 2 of 2 used"))
            .chain(
                (21..=30)
                    .map(|n| wait("o/drafts", n, "needs-human", "a stranded draft; inspect it")),
            )
            .chain((1..=3).map(|n| {
                let reason = "labelled operator-approval; it needs operator's approval of the head";
                wait("o/app", n, "approve", reason)
            }))
            .collect();
        let (repository, number) = pull_ref(LATE_APPROVAL).unwrap();
        waits.push(wait(
            &repository,
            number,
            "approve",
            "pr.operatorPaths: .github/pass.json; it needs operator's approval of the head",
        ));
        let proposals = (1..=128)
            .map(|n| Ask {
                waiting_since: format!("2026-11-04T{:02}:{:02}:00Z", n / 60, n % 60),
                ask: format!(
                    "Apply proposal {n}: {}",
                    "move the bounded label ".repeat(18)
                ),
                recommendation: Some("Apply it after action check. ".repeat(15)),
                ..own_proposal(n)
            })
            .collect();
        let mut snapshot = holds::snapshot(&[], at.into(), Default::default());
        snapshot.pass.candidates = Some(30);
        // As [`holds::carry`] keeps it from the published snapshot.
        snapshot.pass.progress_at = "2026-11-05T11:00:00Z".into();
        let hold = |n: u64, operator: bool| holds::Published {
            item: Some(format!(
                "o/{}#{n}",
                if operator { "tracker" } else { "work" }
            )),
            url: None,
            epic: false,
            kind: match operator {
                true => holds::HoldKind::NeedsHuman,
                false => holds::HoldKind::InFlight,
            },
            reason: "labelled needs-human".into(),
            operator,
            priority: operator.then(|| format!("P{}", n % 4)),
            question: operator
                .then(|| format!("Decide {n}: {}?", "which bound to keep ".repeat(4))),
            since: format!("2026-11-0{}T08:00:00Z", 1 + n % 4),
            carried: false,
            watched: false,
            releases: 1,
        };
        snapshot.holds = (1..=40)
            .map(|n| hold(n, true))
            .chain((1..=30).map(|n| hold(n, false)))
            .collect();
        let on = on_github("o/ops", 1);
        let options: Vec<String> = ["P0", "P1", "P2", "P3"].map(Into::into).into();
        let reading = holds::Reading {
            operator: "operator",
            epics: false,
            considered: true,
            epics_whole: true,
            options: &options,
            on: &on,
            reviews: review_asks(true, Some(&(waits, BTreeSet::new())), at),
            proposals: holds::Collected {
                asks: proposals,
                ..Collected::of(Read::Complete)
            },
            digest: Collected::off(),
            inbox: BTreeMap::new(),
            observed: holds::Observed::off(),
            summaries: (Read::Complete, BTreeMap::new()),
        };
        snapshot.needs_you = Some(holds::needs_you(&snapshot, previous, &reading));
        snapshot
    }

    /// Whether `list` publishes `key`'s card whole, with its full head, and
    /// a matching descriptor that offers the review at that head and the
    /// source record it was read from.
    fn approval_kept(list: &NeedsYou, key: &str) -> bool {
        let Some(card) = list.items.iter().find(|a| a.key == key) else {
            return false;
        };
        let headed = card.head.as_deref().is_some_and(holds::full_head);
        let offered = list.requests.iter().any(|r| {
            r.key == key
                && r.target == card.action
                && (r.actions.iter())
                    .any(|a| a.kind == holds::requests::REVIEW_AT_HEAD && a.supported)
        });
        let source = (list.request_sources.iter()).any(|s| s.key == key && s.ask.head == card.head);
        headed && card.action.head == card.head && offered && source
    }

    /// The board of [`approval_board`] past the comment limit keeps every
    /// actionable approval, the late `pr.operatorPaths` one included, whole,
    /// and cuts the lowest ranked other cards instead; the rest keep their
    /// order, the totals and inventory stay, and the next pass's same list
    /// writes nothing. Its publication is the shared fixture review replays.
    #[test]
    fn approvals_survive_fitting_the_174_decision_board() {
        let at = "2026-11-05T12:00:00Z";
        let board = approval_board(at, None);
        let full = board.needs_you.clone().unwrap();
        assert_eq!((full.counts.decisions, full.counts.groups), (174, 2));
        assert_eq!(full.counts.members, 202);
        assert!(full.complete);
        let keys: Vec<_> = full.items.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys.last(), Some(&LATE_APPROVAL), "ranked last");
        let approvals: Vec<_> = (full.items.iter())
            .filter(|a| a.action.kind == RefKind::ApprovePullRequest)
            .map(|a| a.key.clone())
            .collect();
        assert_eq!(approvals.len(), 4);
        assert!(approvals.iter().all(|k| approval_kept(&full, k)));
        let (fitted, body) = holds::fit(board.clone()).unwrap();
        assert!(body.chars().count() <= holds::MAX_COMMENT_CHARS);
        let list = fitted.needs_you.as_ref().unwrap();
        assert!(
            list.items.len() < full.items.len(),
            "the board must not fit whole"
        );
        for key in &approvals {
            assert!(approval_kept(list, key), "{key}");
        }
        // The operator's holds stay; the others go before any card.
        let operator = fitted.holds.iter().filter(|h| h.operator).count();
        assert_eq!((operator, fitted.truncated), (40, 30));
        // The other cards are the highest ranked of theirs, in rank order,
        // and the approvals keep their places among them.
        let kept: Vec<_> = list.items.iter().map(|a| a.key.as_str()).collect();
        let others = |keys: &[&str]| -> Vec<String> {
            (keys.iter())
                .filter(|k| !approvals.iter().any(|a| a == *k))
                .map(|k| k.to_string())
                .collect()
        };
        assert_eq!(others(&kept), others(&keys)[..kept.len() - approvals.len()]);
        let mut order = keys.clone();
        order.retain(|k| kept.contains(k));
        assert_eq!(kept, order);
        // Canonical totals and the inventory stay; detail is counted.
        assert_eq!(
            (list.counts.clone(), list.count),
            (full.counts.clone(), full.count)
        );
        assert_eq!(list.identities, full.identities);
        assert_eq!((list.omitted, list.complete), (0, true));
        assert!(list.omitted_sources.is_empty());
        let carded = |i: &&holds::Identity| list.items.iter().any(|a| a.key == i.detail_key);
        let uncarded = list.identities.iter().filter(|i| !carded(i)).count();
        assert_eq!(list.details_omitted, uncarded);
        assert!(uncarded > 0);
        // Each kept descriptor's card is kept, each request distinct.
        assert!(list.requests.iter().all(|r| kept.contains(&r.key.as_str())));
        let identities: BTreeSet<_> = list.requests.iter().map(|r| &r.identity).collect();
        assert_eq!(identities.len(), list.requests.len());
        // As serialized: strict v2 cards read back with their heads.
        let published = holds::parse(&body).unwrap();
        let cards: Vec<Ask> =
            serde_json::from_value(published["needs_you"]["items"].clone()).unwrap();
        assert_eq!(cards, list.items);
        let late = &published["needs_you"]["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["key"] == LATE_APPROVAL)
            .unwrap();
        assert_eq!(late["target"]["head"], json!(format!("{:040x}", 174)));
        assert_eq!(late["quality"], "decision");
        // The fixture review replays with its parser (agent-board#722).
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../fixtures/needs-you-approval-fit-174.json"
        ))
        .unwrap();
        assert_eq!(
            published, fixture,
            "fixtures/needs-you-approval-fit-174.json"
        );
        // The same board next pass keeps `changed_at` and writes nothing.
        let later = "2026-11-05T12:30:00Z";
        let (again, _) = holds::fit(approval_board(later, Some(&published))).unwrap();
        let list = again.needs_you.as_ref().unwrap();
        assert_eq!(
            (list.changed_at.as_str(), list.validated_at.as_str()),
            (at, later)
        );
        assert!(!holds::changed(Some(&body), &again).unwrap());
        // Without a descriptor offering the review at its head, an approval
        // is not protected: fail closed, it ranks and goes as any card.
        for strip in [true, false] {
            let mut board = board.clone();
            let list = board.needs_you.as_mut().unwrap();
            for request in list.requests.iter_mut().filter(|r| r.key == LATE_APPROVAL) {
                match strip {
                    true => request.actions.clear(),
                    false => request.target.head = None,
                }
            }
            let (fitted, _) = holds::fit(board).unwrap();
            let list = fitted.needs_you.unwrap();
            assert!(
                !list.items.iter().any(|a| a.key == LATE_APPROVAL),
                "{strip}"
            );
            assert_eq!(list.identities, full.identities);
        }
    }

    /// Protected approvals alone past the limit: the highest ranked keep
    /// their cards, the rest are counted in `details_omitted` and keep
    /// their identities, and the body is still published within the limit.
    /// A non-approval ranked first goes before any of them.
    #[test]
    fn approvals_alone_past_the_limit_still_publish() {
        let (at, later) = ("2026-11-05T12:00:00Z", "2026-11-05T12:30:00Z");
        let board = |at: &str, previous: Option<&Value>| {
            let waits: Vec<_> = (1..=200)
                .map(|n| poll::Wait {
                    repository: "o/app".into(),
                    number: n,
                    head: format!("{n:040x}"),
                    kind: "approve",
                    reason: format!("pr.operatorPaths: {}", ".github/pass.json ".repeat(30)),
                    review_bucket: None,
                    approval: None,
                })
                .collect();
            let mut first = own_proposal(1);
            first.waiting_since = "2026-10-01T00:00:00Z".into();
            let on = on_github("o/ops", 1);
            let reading = holds::Reading {
                operator: "operator",
                epics: false,
                considered: true,
                epics_whole: true,
                options: &[],
                on: &on,
                reviews: review_asks(true, Some(&(waits, BTreeSet::new())), at),
                proposals: holds::Collected {
                    asks: vec![first],
                    ..Collected::of(Read::Complete)
                },
                digest: Collected::off(),
                inbox: BTreeMap::new(),
                observed: holds::Observed::off(),
                summaries: (Read::Complete, BTreeMap::new()),
            };
            let mut snapshot = holds::snapshot(&[], at.into(), Default::default());
            (snapshot.pass.candidates, snapshot.pass.progress_at) = (Some(0), "".into());
            snapshot.needs_you = Some(holds::needs_you(&snapshot, previous, &reading));
            snapshot
        };
        let full = board(at, None).needs_you.unwrap();
        assert_eq!(full.items[0].source, PROPOSALS, "ranked first");
        let (fitted, body) = holds::fit(board(at, None)).unwrap();
        assert!(body.chars().count() <= holds::MAX_COMMENT_CHARS);
        let list = fitted.needs_you.unwrap();
        assert!(list.items.iter().all(|a| a.source == REVIEWS));
        let kept = list.items.len();
        assert!(kept > 0 && kept < 200);
        assert_eq!(list.items, full.items[1..=kept]);
        assert!(list.items.iter().all(|a| approval_kept(&list, &a.key)));
        assert_eq!(list.details_omitted, 201 - kept);
        assert_eq!((list.identities.len(), list.omitted), (201, 0));
        assert_eq!(list.counts, full.counts);
        // The evicted approvals keep their age: the next pass writes nothing.
        let published = holds::parse(&body).unwrap();
        let (again, _) = holds::fit(board(later, Some(&published))).unwrap();
        assert_eq!(again.needs_you.as_ref().unwrap().changed_at, at);
        assert!(!holds::changed(Some(&body), &again).unwrap());
    }

    /// An operator-action proposal on `o/ops#n` as [`approval_board`]'s.
    fn own_proposal(n: u64) -> Ask {
        let action = Reference {
            comment: Some(n),
            ..Reference::new(RefKind::OperatorAction, "o/ops", n)
        };
        Ask {
            key: format!("o/ops#{n}/comment-{n}"),
            item: format!("o/ops#{n}"),
            source: PROPOSALS.into(),
            kind: "operator-action".into(),
            priority: None,
            waiting_since: "2026-11-04T00:00:00Z".into(),
            ask: "Apply the proposal?".into(),
            recommendation: None,
            target: action.url(),
            head: None,
            source_ref: Reference {
                comment: Some(n),
                ..Reference::new(RefKind::Comment, "o/ops", n)
            },
            action,
            carried: false,
            group: None,
            chain: holds::EpicChain::default(),
        }
    }

    #[test]
    fn draft_buckets_survive_grouping_but_never_carry_as_quick() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../fixtures/draft-review-needs-you.json"
        ))
        .unwrap();
        let on = on_github("a/ops", 1);
        let at = "2026-11-05T00:00:00Z";
        let waits = (1..=3)
            .map(|number| {
                let key = format!("cgwalters-forge/bootc#{number}");
                let bucket = fixture["review_buckets"].get(&key).map(|v| {
                    serde_json::from_value::<poll::drafts::Assessment>(v.clone()).unwrap()
                });
                poll::Wait {
                    repository: "cgwalters-forge/bootc".into(),
                    number,
                    head: bucket
                        .as_ref()
                        .map(|b| b.assessed_head.clone())
                        .unwrap_or_else(|| "c".repeat(40)),
                    kind: "promote",
                    reason: "drafts to inspect and promote".into(),
                    review_bucket: bucket,
                    approval: None,
                }
            })
            .collect();
        let mut reading = holds::Reading {
            operator: BOT,
            epics: false,
            considered: false,
            epics_whole: true,
            options: &[],
            on: &on,
            reviews: review_asks(true, Some(&(waits, BTreeSet::new())), at),
            proposals: Collected::off(),
            digest: Collected::off(),
            inbox: BTreeMap::new(),
            observed: holds::Observed::off(),
            summaries: (Read::Off, BTreeMap::new()),
        };
        let snapshot = holds::snapshot(&[], at.into(), Default::default());
        let first = holds::needs_you(&snapshot, None, &reading);
        let published = serde_json::to_value(&first).unwrap();
        assert_eq!(published["schema"], fixture["schema"]);
        assert_eq!(published["items"][0]["group"], fixture["group"]);
        assert_eq!(published["review_buckets"], fixture["review_buckets"]);
        // Old strict Ask parsers still round-trip the grouped card unchanged;
        // assessments are a separate optional list extension, per member.
        let ask: Ask = serde_json::from_value(published["items"][0].clone()).unwrap();
        assert_eq!(serde_json::to_value(ask).unwrap(), published["items"][0]);
        reading.reviews = Collected::of(Read::Unread);
        let carried = holds::needs_you(
            &snapshot,
            Some(&json!({"schema":holds::HOLDS_SCHEMA,"needs_you":published})),
            &reading,
        );
        assert!(!carried.items.is_empty());
        assert!(carried.items[0].carried);
        assert!(
            carried.review_buckets.is_empty(),
            "unread asks cannot retain quick"
        );
    }
}
