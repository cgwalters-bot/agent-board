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

/// The most issues labelled for proposals read a pass, oldest first; the
/// rest are unread, and their published asks kept.
const MAX_PROPOSAL_ISSUES: usize = 10;
/// The most proposals listed of one issue.
const MAX_PROPOSALS_PER_ISSUE: usize = 10;
/// The most entries listed of a digest.
const MAX_DIGEST_ASKS: usize = 50;
/// The most held issues whose summaries are read a pass, the highest
/// ranked first.
const MAX_SUMMARIES: usize = 10;
/// The longest label `holds.proposals.label` may be, as GitHub's.
const LABEL_LIMIT: usize = 50;
/// Requests left before each read for the hold snapshot's own: its reads,
/// its write, and a comment read's pages.
const NEEDS_READS: usize = WRITE_RESERVE + HOLDS_READS + plan::PLAN_COMMENT_PAGES;
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
            "needs-human" => RefKind::PullRequest,
            _ => RefKind::ApprovePullRequest,
        };
        let item = format!("{repository}#{number}");
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
        });
    }
    if !collected.unread.is_empty() {
        collected.read = Read::Partial;
    }
    collected
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
        snapshot: &holds::Snapshot,
        previous: Option<&Value>,
        on: &ItemRef,
    ) -> NeedsYou {
        let options = match &self.board {
            Some(Some(board)) => board.options.clone(),
            _ => Vec::new(),
        };
        let config = self.config;
        let reviews = review_asks(
            !config.follow.is_empty(),
            self.followed.as_ref(),
            &snapshot.at,
        );
        let proposals = match config.holds.as_ref().and_then(|h| h.proposals.as_ref()) {
            Some(at) => self.proposals(at),
            None => Collected::off(),
        };
        let digest = match &config.maintain {
            Some(maintain) => self.newest_digest(maintain),
            None => Collected::off(),
        };
        let mut reading = holds::Reading {
            epics: config.epics.is_some(),
            considered: self.unconsidered == 0,
            options: &options,
            on,
            reviews,
            proposals,
            digest,
            summaries: (Read::Complete, BTreeMap::new()),
        };
        let list = holds::needs_you(snapshot, previous, &reading);
        reading.summaries = self.summaries(&list);
        holds::needs_you(snapshot, previous, &reading)
    }

    /// The proposals on the open issues `at` labels, oldest first.
    fn proposals(&mut self, at: &ProposalsAt) -> Collected {
        let (config, bot) = (self.config, self.config.bot.as_str());
        let writers = [config.operator.as_str(), bot];
        let mut collected = Collected::of(Read::Complete);
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
            (None, _) => Err(anyhow::anyhow!("no maintain day is known")),
            (_, false) => Err(anyhow::anyhow!(
                "the pass's request budget or time is spent"
            )),
        };
        match read {
            Ok((comments, scope)) => {
                let (asks, whole) = digest_asks(&on, &comments, &self.config.bot, &scope);
                Collected {
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

    /// The bot's summaries of the first [`MAX_SUMMARIES`] held issues and
    /// blocking pull requests of `list`, by key.
    fn summaries(&mut self, list: &NeedsYou) -> (Read, BTreeMap<String, Summary>) {
        let (mut read, mut said) = (Read::Complete, BTreeMap::new());
        let held = (list.items.iter())
            .filter(|a| a.source_ref.kind == RefKind::Hold && a.source != "pass");
        for (n, ask) in held.enumerate() {
            let issue = on_github(&ask.source_ref.repository, ask.source_ref.number);
            if n == MAX_SUMMARIES || !self.room() {
                read = Read::Partial;
                break;
            }
            // A blocking hold is on a pull request, whose summary an issue
            // read cannot find.
            let comments = match ask.action.kind {
                RefKind::PullRequest => plan::pull_request_comments(&self.client, &issue),
                _ => plan::issue_comments(&self.client, &issue),
            };
            match comments {
                Ok(comments) => {
                    if let Some(summary) = bot_summary(&comments, &self.config.bot) {
                        said.insert(ask.key.clone(), summary);
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    const BOT: &str = "cgwalters-bot";
    const HEAD: &str = "488f45ebacd03a8bc88dceb3d703a5322850ce77";

    /// Comment `id` by `author`, edited by `editor` if any, `minutes` after
    /// 08:00 on 28 October.
    fn comment(
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
}
